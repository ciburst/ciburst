import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname } from "node:path";

const ORDER = ["create", "update", "delete", "noop"];

/** @param {import("./types.mjs").Step[]} steps */
export function formatPlan(steps) {
  if (steps.length === 0) return "nothing to do";
  const width = Math.max(...steps.map((step) => step.action.length));
  return steps
    .map((step) => {
      const detail = step.detail ? `  (${step.detail})` : "";
      return `${step.action.padEnd(width)}  ${step.resource}${detail}`;
    })
    .join("\n");
}

/** @param {import("./types.mjs").Step[]} steps */
export function summarizePlan(steps) {
  const counts = Object.fromEntries(ORDER.map((action) => [action, 0]));
  for (const step of steps) counts[step.action] = (counts[step.action] ?? 0) + 1;
  return `${counts.create} to create, ${counts.update} to update, ${counts.delete} to delete, ${counts.noop} unchanged`;
}

export function parseEnv(text) {
  const values = {};
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line || line.startsWith("#")) continue;
    const eq = line.indexOf("=");
    if (eq <= 0) continue;
    const key = line.slice(0, eq).trim();
    let value = line.slice(eq + 1).trim();
    const quoted =
      value.length >= 2 &&
      ((value.startsWith('"') && value.endsWith('"')) ||
        (value.startsWith("'") && value.endsWith("'")));
    if (quoted) value = value.slice(1, -1);
    values[key] = value;
  }
  return values;
}

export function formatEnv(values) {
  return Object.keys(values)
    .sort()
    .map((key) => `${key}=${values[key]}`)
    .join("\n")
    .concat("\n");
}

export async function readEnvFile(path) {
  try {
    return parseEnv(await readFile(path, "utf8"));
  } catch (error) {
    if (error.code === "ENOENT") return {};
    throw error;
  }
}

/** Writes values into an env file. Existing keys win unless overwrite is set, so secrets shown once survive reruns. */
export async function writeEnvFile(path, values, { overwrite = false } = {}) {
  const existing = await readEnvFile(path);
  const merged = overwrite ? { ...existing, ...values } : { ...values, ...existing };
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, formatEnv(merged), { mode: 0o600 });
  return merged;
}

/** Outputs store backed by an env file. */
export function createFileOutputs(path) {
  return {
    read: () => readEnvFile(path),
    write: (values, options) => writeEnvFile(path, values, options),
    async clear() {
      await mkdir(dirname(path), { recursive: true });
      await writeFile(path, "", { mode: 0o600 });
    },
  };
}

/** Outputs store in memory, for tests. */
export function createMemoryOutputs(initial = {}) {
  let values = { ...initial };
  return {
    read: async () => ({ ...values }),
    async write(next, { overwrite = false } = {}) {
      values = overwrite ? { ...values, ...next } : { ...next, ...values };
      return { ...values };
    },
    async clear() {
      values = {};
    },
  };
}
