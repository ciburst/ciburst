import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { validateConfig } from "../../core/config.mjs";

export const CONTRACT_CONFIG = Object.freeze({
  name: "contract",
  gitlab: { url: "https://gitlab.example", projectIds: [7] },
  host: { type: "fake" },
  provider: { type: "fake" },
  defaultPool: "ci",
  pools: [
    { tag: "ci", image: "example.invalid/node:24", stages: ["test"] },
    { tag: "build", image: "example.invalid/kaniko:1", stages: ["build"], maxRuns: 1 },
  ],
  cache: { type: "s3", expireDays: 14 },
});

const CAPABILITY_FIELDS = {
  name: "string",
  dockerDaemon: "boolean",
  maxVcpu: "number",
  maxMemoryMiB: "number",
  maxStorageMiB: "number",
  maxDurationS: "number",
  billing: "string",
};

const LIVE = ["queued", "running"];

/**
 * Runs the provider contract.
 * `make(config, previous)` returns a provider for `config`; with `previous` given it must share that
 * provider's backend, so that a changed config is planned against the existing resources.
 * @param {string} name
 * @param {(config: import("../../core/types.mjs").Config, previous?: import("../../core/types.mjs").Provider) => Promise<import("../../core/types.mjs").Provider> | import("../../core/types.mjs").Provider} make
 */
export function providerContract(name, make) {
  const config = validateConfig(structuredClone(CONTRACT_CONFIG));
  const env = { RUNNER_TOKEN: "glrt-contract" };
  const labels = { project: "7", pipeline: "1", job: "2", stage: "test" };

  describe(`provider contract: ${name}`, () => {
    it("describes its capabilities", async () => {
      const caps = (await make(config)).capabilities();
      for (const [field, type] of Object.entries(CAPABILITY_FIELDS)) {
        assert.equal(typeof caps[field], type, field);
      }
      assert.ok(Array.isArray(caps.archs) && caps.archs.length > 0);
    });

    it("plans every resource on an empty backend and nothing after apply", async () => {
      const provider = await make(config);
      const first = await provider.plan();
      assert.ok(first.length >= config.pools.length, "one step per pool at least");
      assert.ok(
        first.every((step) => step.action === "create"),
        JSON.stringify(first),
      );
      for (const step of first) {
        assert.equal(typeof step.id, "string");
        assert.equal(typeof step.resource, "string");
      }
      const outputs = await provider.apply(first);
      assert.equal(typeof outputs, "object");
      const second = await provider.plan();
      assert.ok(
        second.every((step) => step.action === "noop"),
        JSON.stringify(second),
      );
      assert.equal(second.length, first.length);
    });

    it("updates a changed pool and deletes a removed one", async () => {
      const provider = await make(config);
      await provider.apply(await provider.plan());
      const changed = validateConfig({
        ...structuredClone(CONTRACT_CONFIG),
        pools: [{ ...CONTRACT_CONFIG.pools[0], vcpu: 4 }],
      });
      const again = await make(changed, provider);
      const steps = await again.plan();
      assert.ok(steps.some((step) => step.id === "definition:ci" && step.action === "update"));
      assert.ok(steps.some((step) => step.id === "definition:build" && step.action === "delete"));
      await again.apply(steps);
      assert.ok((await again.plan()).every((step) => step.action === "noop"));
    });

    it("rotates keys on request and returns the new secrets", async () => {
      const provider = await make(config);
      const created = await provider.apply(await provider.plan());
      const rotated = await provider.apply(await provider.plan({ rotate: ["key", "cache"] }));
      assert.ok(created.CIBURST_DISPATCH_SECRET_KEY);
      assert.ok(rotated.CIBURST_DISPATCH_SECRET_KEY);
      assert.notEqual(rotated.CIBURST_DISPATCH_SECRET_KEY, created.CIBURST_DISPATCH_SECRET_KEY);
      assert.notEqual(rotated.CIBURST_CACHE_SECRET_KEY, created.CIBURST_CACHE_SECRET_KEY);
    });

    it("refuses to start without a runner token or an unprovisioned pool", async () => {
      const provider = await make(config);
      await assert.rejects(() => provider.start({ pool: config.pools[0], env, labels }));
      await provider.apply(await provider.plan());
      await assert.rejects(() => provider.start({ pool: config.pools[0], env: {}, labels }));
    });

    it("starts, lists and stops a run", async () => {
      const provider = await make(config);
      await provider.apply(await provider.plan());
      const { runId } = await provider.start({ pool: config.pools[0], env, labels });
      assert.equal(typeof runId, "string");
      const listed = await provider.list({ pool: "ci" });
      const run = listed.find((r) => r.runId === runId);
      assert.ok(run, "run listed");
      assert.ok(LIVE.includes(run.state));
      assert.deepEqual(run.labels, labels);
      assert.equal((await provider.list({ pool: "build" })).length, 0);
      await provider.stop(runId);
      const after = (await provider.list()).find((r) => r.runId === runId);
      assert.ok(!after || !LIVE.includes(after.state));
    });

    it("destroys everything so that plan starts over", async () => {
      const provider = await make(config);
      await provider.apply(await provider.plan());
      await provider.destroy();
      assert.ok((await provider.plan()).every((step) => step.action === "create"));
    });
  });
}
