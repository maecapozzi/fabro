import { readdir, readFile, stat } from "node:fs/promises";
import { resolve } from "node:path";
import { isDeepStrictEqual } from "node:util";

const packageRoot = resolve(import.meta.dirname, "..");
const root = resolve(packageRoot, "public");
const MAX_FILES = 2_000;
const MAX_FILE_BYTES = 10 * 1024 * 1024;
const MAX_TOTAL_BYTES = 32 * 1024 * 1024;
const EXPECTED_ALTO_MANIFEST = {
  name: "Fabro UI prototype",
  surfaces: { "/": "web" },
  governance: {
    secrets: {
      FABRO_BACKEND_ORIGIN: { scope: "all" },
    },
  },
};

async function files(directory) {
  const entries = await readdir(directory, { withFileTypes: true });
  const nested = await Promise.all(entries.map(async (entry) => {
    const path = resolve(directory, entry.name);
    return entry.isDirectory() ? files(path) : [path];
  }));
  return nested.flat();
}

async function main() {
  let altoManifest;
  try {
    altoManifest = JSON.parse(await readFile(resolve(packageRoot, "alto.json"), "utf8"));
  } catch (cause) {
    throw new Error(`alto.json is missing or invalid: ${cause.message}`);
  }
  if (!isDeepStrictEqual(altoManifest, EXPECTED_ALTO_MANIFEST)) {
    throw new Error(
      "alto.json must declare exactly the Fabro UI prototype root web surface and "
      + "FABRO_BACKEND_ORIGIN secret with scope \"all\" and no environment binding.",
    );
  }

  const all = await files(root).catch(() => {
    throw new Error("public/ is missing. Run `npm run export-ui` after building apps/fabro-web.");
  });
  if (all.length > MAX_FILES) throw new Error(`Asset count ${all.length} exceeds ${MAX_FILES}.`);

  let total = 0;
  for (const file of all) {
    const size = (await stat(file)).size;
    if (size > MAX_FILE_BYTES) throw new Error(`${file} exceeds the 10 MiB per-file limit.`);
    total += size;
  }
  if (total > MAX_TOTAL_BYTES) throw new Error(`Asset total ${total} exceeds the 32 MiB limit.`);

  const provenance = JSON.parse(await readFile(resolve(root, ".fabro-ui-provenance.json"), "utf8"));
  if (typeof provenance.buildId !== "string" || !provenance.buildId) {
    throw new Error("Asset provenance does not identify a Fabro UI build.");
  }
  console.log(`Validated ${all.length} UI assets (${total} bytes), build ${provenance.buildId}.`);
}

main().catch((error) => {
  console.error(error.message);
  process.exit(1);
});
