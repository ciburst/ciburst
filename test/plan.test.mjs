import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  formatPlan,
  summarizePlan,
  parseEnv,
  formatEnv,
  writeEnvFile,
  readEnvFile,
  createFileOutputs,
  createMemoryOutputs,
} from "../core/plan.mjs";
import { fileStore, memoryStore } from "../core/store.mjs";

const steps = [
  { id: "a", action: "create", resource: "project demo" },
  { id: "b", action: "noop", resource: "job definition demo-ci", detail: "unchanged" },
  { id: "c", action: "delete", resource: "job definition demo-old" },
];

describe("plan formatting", () => {
  it("prints one aligned line per step and a summary", () => {
    assert.equal(
      formatPlan(steps),
      "create  project demo\nnoop    job definition demo-ci  (unchanged)\ndelete  job definition demo-old",
    );
    assert.equal(summarizePlan(steps), "1 to create, 0 to update, 1 to delete, 1 unchanged");
    assert.equal(formatPlan([]), "nothing to do");
  });
});

describe("env files", () => {
  it("parses comments, blanks and quotes", () => {
    const parsed = parseEnv("# comment\n\nA=1\nB=\"two words\"\nC='x'\nBAD\n=nokey\nD=a=b\n");
    assert.deepEqual(parsed, { A: "1", B: "two words", C: "x", D: "a=b" });
    assert.equal(formatEnv({ B: "2", A: "1" }), "A=1\nB=2\n");
  });

  it("keeps existing keys unless overwrite is set", async () => {
    const dir = await mkdtemp(join(tmpdir(), "ciburst-"));
    try {
      const path = join(dir, "nested", "outputs.env");
      assert.deepEqual(await readEnvFile(path), {});
      await writeEnvFile(path, { SECRET: "first", URL: "u1" });
      await writeEnvFile(path, { SECRET: "second", NEW: "n" });
      assert.deepEqual(await readEnvFile(path), { SECRET: "first", URL: "u1", NEW: "n" });
      await writeEnvFile(path, { URL: "u2" }, { overwrite: true });
      assert.equal((await readEnvFile(path)).URL, "u2");
      const outputs = createFileOutputs(path);
      await outputs.clear();
      assert.equal(await readFile(path, "utf8"), "");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  it("memory outputs behave the same", async () => {
    const outputs = createMemoryOutputs({ A: "1" });
    await outputs.write({ A: "2", B: "3" });
    assert.deepEqual(await outputs.read(), { A: "1", B: "3" });
    await outputs.write({ A: "2" }, { overwrite: true });
    assert.equal((await outputs.read()).A, "2");
    await outputs.clear();
    assert.deepEqual(await outputs.read(), {});
  });
});

describe("stores", () => {
  it("file store keeps slots apart and survives reloads", async () => {
    const dir = await mkdtemp(join(tmpdir(), "ciburst-"));
    try {
      const path = join(dir, "state.json");
      const a = fileStore(path, "a");
      const b = fileStore(path, "b");
      assert.equal(await a.load(), null);
      await a.save({ x: 1 });
      await b.save({ y: 2 });
      assert.deepEqual(await fileStore(path, "a").load(), { x: 1 });
      assert.deepEqual(await fileStore(path, "b").load(), { y: 2 });
      await a.save(null);
      assert.equal(await a.load(), null);
      assert.deepEqual(await b.load(), { y: 2 });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  it("memory store returns copies", async () => {
    const store = memoryStore();
    const value = { list: [1] };
    await store.save(value);
    value.list.push(2);
    assert.deepEqual(await store.load(), { list: [1] });
  });
});
