import { createHash } from "node:crypto";
import { memoryStore } from "../../core/store.mjs";

/**
 * In-memory dispatcher host: remembers a digest of config and secrets and a fake URL.
 * @param {{ config: import("../../core/types.mjs").Config, store?: ReturnType<typeof memoryStore> }} deps
 * @returns {import("../../core/types.mjs").Host}
 */
export function createHost({ config, store = memoryStore() }) {
  const resource = `dispatcher ${config.name}-dispatch`;
  const url = `https://${config.name}-dispatch.fake.invalid/`;
  const digestOf = (secrets) =>
    createHash("sha256").update(JSON.stringify({ config, secrets })).digest("hex");

  return {
    async plan({ secrets }) {
      const current = await store.load();
      const action = !current ? "create" : current.digest === digestOf(secrets) ? "noop" : "update";
      return [{ id: "dispatcher", action, resource }];
    },

    async apply(steps, { secrets }) {
      for (const step of steps) {
        if (step.id !== "dispatcher") throw new Error(`unknown step ${step.id}`);
        if (step.action !== "noop") await store.save({ digest: digestOf(secrets), url });
      }
      const current = await store.load();
      return current ? { CIBURST_DISPATCH_URL: current.url } : {};
    },

    async destroy() {
      await store.save(null);
    },

    async url() {
      return (await store.load())?.url;
    },
  };
}
