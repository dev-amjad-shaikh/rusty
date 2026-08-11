import { readdir, readFile, writeFile } from "node:fs/promises";

const root = new URL("../dist/", import.meta.url);

async function normalize(directory) {
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const target = new URL(entry.name, directory);
    if (entry.isDirectory()) {
      await normalize(new URL(`${entry.name}/`, directory));
      continue;
    }
    if (!/[.](?:css|html|js)$/.test(entry.name)) continue;
    const source = await readFile(target, "utf8");
    const clean = source.replace(/[ \t]+$/gm, "");
    if (clean !== source) await writeFile(target, clean, "utf8");
  }
}

await normalize(root);
