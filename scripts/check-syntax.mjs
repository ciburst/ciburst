import { readdir } from "node:fs/promises";
import { join } from "node:path";
import { spawn } from "node:child_process";

const SKIP = new Set(["node_modules", ".git", ".local", ".claude", "coverage"]);

async function* walk(dir) {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    if (SKIP.has(entry.name)) continue;
    const path = join(dir, entry.name);
    if (entry.isDirectory()) yield* walk(path);
    else if (/\.(mjs|js|cjs)$/.test(entry.name)) yield path;
  }
}

function check(file) {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, ["--check", file], {
      stdio: ["ignore", "ignore", "pipe"],
    });
    let stderr = "";
    child.stderr.on("data", (chunk) => (stderr += chunk));
    child.on("close", (code) => resolve({ file, ok: code === 0, stderr }));
  });
}

const results = [];
for await (const file of walk(process.cwd())) results.push(await check(file));
const failed = results.filter((r) => !r.ok);
for (const r of failed) process.stderr.write(`${r.file}\n${r.stderr}\n`);
process.stdout.write(`${results.length - failed.length} of ${results.length} files parse\n`);
process.exit(failed.length === 0 ? 0 : 1);
