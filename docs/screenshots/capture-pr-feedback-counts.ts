import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const repositoryRoot = resolve(import.meta.dir, "../..");
const webRoot = join(repositoryRoot, "apps/fabro-web");
const runsRoute = join(webRoot, "app/routes/runs.tsx");
const chromium =
  "/opt/playwright/chromium_headless_shell-1243/chrome-headless-shell-linux-arm64/chrome-headless-shell";
const temporaryDirectory = await mkdtemp(join(tmpdir(), "fabro-pr-feedback-counts-"));
const bundleDirectory = join(temporaryDirectory, "bundle");
const entrypoint = join(temporaryDirectory, "fixture.tsx");

const fixtureSource = `
import React from "react";
import { createRoot } from "react-dom/client";
import { InReviewColumn } from "@fixture-runs";

const pullRequests = [
  {
    owner: "acme",
    repo: "payments",
    number: 142,
    title: "Harden webhook retry handling",
    html_url: "https://github.example/acme/payments/pull/142",
    draft: false,
    updated_at: "2026-11-20T12:00:00Z",
    run_ids: [],
    review_readiness: {
      status: "unresolved_comments",
      head_sha: "1111111111111111111111111111111111111111",
      unresolved_thread_count: 2,
      feedback_awaiting_response_count: 1,
      checked_at: "2026-11-20T12:01:00Z",
    },
  },
  {
    owner: "acme",
    repo: "console",
    number: 87,
    title: "Polish audit log filters",
    html_url: "https://github.example/acme/console/pull/87",
    draft: true,
    updated_at: "2026-11-20T11:00:00Z",
    run_ids: [],
    review_readiness: {
      status: "ready",
      head_sha: "2222222222222222222222222222222222222222",
      unresolved_thread_count: 0,
      feedback_awaiting_response_count: 0,
      checked_at: "2026-11-20T12:01:00Z",
    },
  },
];

function FixturePreview() {
  return (
    <main className="min-h-screen bg-atmosphere px-10 py-10 text-fg">
      <p className="mb-2 font-mono text-xs uppercase tracking-widest text-fg-muted">Runs workspace · GitHub review readiness</p>
      <h1 className="mb-8 text-2xl font-semibold">In-review feedback counts</h1>
      <div className="max-w-md">
        <InReviewColumn pullRequests={pullRequests} loading={false} error={false} />
      </div>
    </main>
  );
}

createRoot(document.getElementById("root")!).render(<FixturePreview />);
`;

const html = `<!doctype html><html class="h-full bg-atmosphere scheme-only-dark"><head><link rel="stylesheet" href="/app.css" /></head><body class="h-full font-sans antialiased"><div id="root"></div><script type="module" src="/fixture.js"></script></body></html>`;

async function run(command: string[], cwd: string) {
  const process = Bun.spawn(command, { cwd, stdout: "inherit", stderr: "inherit" });
  if ((await process.exited) !== 0) throw new Error(`Command failed: ${command.join(" ")}`);
}

try {
  await writeFile(entrypoint, fixtureSource);
  const build = await Bun.build({
    entrypoints: [entrypoint],
    outdir: bundleDirectory,
    naming: "fixture.js",
    target: "browser",
    plugins: [{
      name: "export-in-review-column",
      setup(builder) {
        builder.onResolve(
          { filter: /^(react|react-dom\/client|react\/jsx-dev-runtime)$/ },
          (args) => ({ path: Bun.resolveSync(args.path, webRoot) }),
        );
        builder.onResolve({ filter: /^@fixture-runs$/ }, () => ({ path: runsRoute, namespace: "fixture-runs" }));
        builder.onLoad({ filter: /.*/, namespace: "fixture-runs" }, async () => {
          const source = await readFile(runsRoute, "utf8");
          return { contents: source.replace("function InReviewColumn({", "export function InReviewColumn({"), loader: "tsx" };
        });
      },
    }],
  });
  if (!build.success) throw new Error(build.logs.map((log) => log.message).join("\n"));
  const tailwindPackage = Bun.resolveSync("@tailwindcss/cli/package.json", webRoot);
  const tailwindBin = join(resolve(tailwindPackage, ".."), JSON.parse(await readFile(tailwindPackage, "utf8")).bin.tailwindcss);
  await run([process.execPath, tailwindBin, "-i", "app/app.css", "-o", join(bundleDirectory, "app.css"), "--minify"], webRoot);
  await writeFile(join(bundleDirectory, "index.html"), html);
  const server = Bun.serve({
    port: 0,
    fetch(request) {
      const fileName = new URL(request.url).pathname.slice(1) || "index.html";
      return new Response(Bun.file(join(bundleDirectory, fileName)));
    },
  });
  try {
    await run(
      [chromium, "--headless", "--no-sandbox", "--hide-scrollbars", "--window-size=900,700", `--user-data-dir=${join(temporaryDirectory, "chrome")}`, `--screenshot=${join(repositoryRoot, "docs/screenshots/pr-feedback-counts.png")}`, `http://127.0.0.1:${server.port}`],
      repositoryRoot,
    );
  } finally {
    server.stop(true);
  }
} finally {
  await rm(temporaryDirectory, { recursive: true, force: true });
}
