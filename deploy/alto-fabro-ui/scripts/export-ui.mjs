import { cp, mkdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { execFileSync } from "node:child_process";

const packageRoot = resolve(import.meta.dirname, "..");
const source = resolve(process.env.FABRO_UI_DIST ?? `${packageRoot}/../../apps/fabro-web/dist`);
const destination = resolve(packageRoot, "public");

async function main() {
  const sourceMetadata = await stat(source).catch(() => null);
  if (!sourceMetadata?.isDirectory()) {
    throw new Error(`Fabro UI build is missing at ${source}. Run \`bun run build\` in apps/fabro-web first.`);
  }

  const buildIdDocument = JSON.parse(await readFile(resolve(source, "build-id.json"), "utf8"));
  const buildId = buildIdDocument?.buildId;
  if (typeof buildId !== "string" || !buildId) throw new Error("Fabro UI build has no build-id.json.");

  await rm(destination, { recursive: true, force: true });
  await mkdir(destination, { recursive: true });
  await cp(source, destination, { recursive: true, dereference: true });

  let sourceRevision = "unknown";
  try {
    sourceRevision = execFileSync("git", ["rev-parse", "HEAD"], { cwd: packageRoot, encoding: "utf8" }).trim();
  } catch {
    // A source archive is still deployable; record that its checkout revision was unavailable.
  }
  await writeFile(
    resolve(destination, ".fabro-ui-provenance.json"),
    `${JSON.stringify({ buildId, sourceRevision, exportedAt: new Date().toISOString() }, null, 2)}\n`,
  );
}

main().catch((error) => {
  console.error(error.message);
  process.exit(1);
});
