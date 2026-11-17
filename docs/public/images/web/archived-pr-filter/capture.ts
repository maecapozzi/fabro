import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const repositoryRoot = resolve(import.meta.dir, "../../../../..");
const webRoot = join(repositoryRoot, "apps/fabro-web");
const runsRoute = join(webRoot, "app/routes/runs.tsx");
const outputDirectory = import.meta.dir;
const chromium =
  "/opt/playwright/chromium_headless_shell-1243/chrome-headless-shell-linux-arm64/chrome-headless-shell";
const temporaryDirectory = await mkdtemp(join(tmpdir(), "fabro-archived-pr-filter-"));
const bundleDirectory = join(temporaryDirectory, "bundle");
const entrypoint = join(temporaryDirectory, "fixture.tsx");

const fixtureSource = `
import React from "react";
import { createRoot } from "react-dom/client";
import { InReviewColumn } from "@fixture-runs";

const activePullRequest = {
  owner: "acme",
  repo: "active-app",
  number: 142,
  title: "[Active repository] Keep this open pull request",
  html_url: "https://github.example/acme/active-app/pull/142",
  draft: false,
  updated_at: "2026-11-16T12:00:00Z",
  run_ids: [],
};
const archivedPullRequest = {
  owner: "acme",
  repo: "archived-legacy",
  number: 87,
  title: "[Archived repository] Exclude this open pull request",
  html_url: "https://github.example/acme/archived-legacy/pull/87",
  draft: false,
  updated_at: "2026-11-15T12:00:00Z",
  run_ids: [],
};
const before = new URLSearchParams(window.location.search).get("state") === "before";
const pullRequests = before
  ? [activePullRequest, archivedPullRequest]
  : [activePullRequest];

function FixturePreview() {
  return (
    <main className="min-h-screen bg-atmosphere text-fg">
      <header className="border-b border-line bg-panel px-10 py-4">
        <div className="flex items-center justify-between">
          <span className="text-lg font-semibold tracking-tight">Fabro</span>
          <span className="rounded-full border border-violet-400/30 bg-violet-400/10 px-3 py-1 font-mono text-xs uppercase tracking-wide text-violet-300">
            Fixture preview
          </span>
        </div>
      </header>
      <section className="mx-auto max-w-5xl px-10 py-10">
        <p className="mb-2 font-mono text-xs uppercase tracking-widest text-fg-muted">
          Runs workspace · synthetic GitHub data
        </p>
        <h1 className="mb-2 text-2xl font-semibold text-fg">
          {before ? "Before: archived repositories included" : "After: archived repositories excluded"}
        </h1>
        <p className="mb-8 max-w-2xl text-sm leading-6 text-fg-muted">
          {before
            ? "The shared open pull request result contains active and archived repositories."
            : "GitHub applies archived:false, so the shared result and count contain only active repositories."}
        </p>
        <div className="grid grid-cols-[minmax(0,420px)_1fr] gap-12">
          <InReviewColumn
            pullRequests={pullRequests}
            loading={false}
            error={false}
          />
          <aside className="rounded-md border border-dashed border-line p-5 text-sm leading-6 text-fg-muted">
            <p className="font-medium text-fg-2">Fixture meaning</p>
            <p className="mt-2">
              Repository state is written into each synthetic card's repository name and title.
              No live GitHub account or private repository data is used.
            </p>
          </aside>
        </div>
      </section>
    </main>
  );
}

createRoot(document.getElementById("root")!).render(<FixturePreview />);
`;

const html = `<!doctype html>
<html lang="en" class="dark">
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>Archived pull request filter fixture preview</title>
    <link rel="stylesheet" href="/app.css" />
  </head>
  <body><div id="root"></div><script type="module" src="/fixture.js"></script></body>
</html>`;

async function run(command: string[], cwd: string) {
  const process = Bun.spawn(command, {
    cwd,
    stdout: "inherit",
    stderr: "inherit",
  });
  const exitCode = await process.exited;
  if (exitCode !== 0) {
    throw new Error(`Command failed (${exitCode}): ${command.join(" ")}`);
  }
}

try {
  await writeFile(entrypoint, fixtureSource);
  await Bun.write(join(temporaryDirectory, "index.html"), html);

  const build = await Bun.build({
    entrypoints: [entrypoint],
    outdir: bundleDirectory,
    naming: "fixture.js",
    target: "browser",
    plugins: [
      {
        name: "export-real-in-review-component",
        setup(builder) {
          builder.onResolve(
            { filter: /^(react|react-dom\/client|react\/jsx-dev-runtime)$/ },
            (args) => ({ path: Bun.resolveSync(args.path, webRoot) }),
          );
          builder.onResolve({ filter: /^@fixture-runs$/ }, () => ({
            path: runsRoute,
            namespace: "fixture-runs",
          }));
          builder.onLoad({ filter: /.*/, namespace: "fixture-runs" }, async () => {
            const source = await readFile(runsRoute, "utf8");
            const exported = source.replace(
              "function InReviewColumn({",
              "export function InReviewColumn({",
            );
            if (exported === source) {
              throw new Error("Could not expose InReviewColumn for evidence capture");
            }
            return { contents: exported, loader: "tsx" };
          });
        },
      },
    ],
  });
  if (!build.success) {
    throw new Error(build.logs.map((log) => log.message).join("\n"));
  }

  const tailwindPackage = Bun.resolveSync("@tailwindcss/cli/package.json", webRoot);
  const tailwindBin = join(
    resolve(tailwindPackage, ".."),
    JSON.parse(await readFile(tailwindPackage, "utf8")).bin.tailwindcss,
  );
  await run(
    [
      process.execPath,
      tailwindBin,
      "-i",
      "app/app.css",
      "-o",
      join(bundleDirectory, "app.css"),
      "--minify",
    ],
    webRoot,
  );
  await Bun.write(join(bundleDirectory, "index.html"), html);

  const server = Bun.serve({
    port: 0,
    async fetch(request) {
      const path = new URL(request.url).pathname;
      const fileName = path === "/" ? "index.html" : path.slice(1);
      const file = Bun.file(join(bundleDirectory, fileName));
      return (await file.exists()) ? new Response(file) : new Response("Not found", { status: 404 });
    },
  });

  try {
    for (const state of ["before", "after"]) {
      await run(
        [
          chromium,
          "--headless",
          "--no-sandbox",
          "--hide-scrollbars",
          "--force-device-scale-factor=1",
          "--window-size=1200,800",
          `--user-data-dir=${join(temporaryDirectory, `chrome-${state}`)}`,
          `--screenshot=${join(outputDirectory, `${state}.png`)}`,
          `http://127.0.0.1:${server.port}/?state=${state}`,
        ],
        repositoryRoot,
      );
    }
  } finally {
    server.stop(true);
  }
} finally {
  await rm(temporaryDirectory, { recursive: true, force: true });
}
