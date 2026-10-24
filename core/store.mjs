import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname } from "node:path";

/** JSON document store with one slot, in memory. */
export function memoryStore(initial = null) {
  let value = initial;
  return {
    load: async () => (value === null ? null : structuredClone(value)),
    async save(next) {
      value = next === null ? null : structuredClone(next);
    },
  };
}

/** JSON document store with one named slot inside a file shared by several slots. */
export function fileStore(path, slot) {
  async function readAll() {
    try {
      return JSON.parse(await readFile(path, "utf8"));
    } catch (error) {
      if (error.code === "ENOENT") return {};
      throw error;
    }
  }
  return {
    async load() {
      const all = await readAll();
      return all[slot] ?? null;
    },
    async save(next) {
      const all = await readAll();
      if (next === null) delete all[slot];
      else all[slot] = next;
      await mkdir(dirname(path), { recursive: true });
      await writeFile(path, `${JSON.stringify(all, null, 2)}\n`, { mode: 0o600 });
    },
  };
}
