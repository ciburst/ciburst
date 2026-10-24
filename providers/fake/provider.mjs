import { randomBytes, randomUUID } from "node:crypto";
import { memoryStore } from "../../core/store.mjs";

const CAPABILITIES = Object.freeze({
  name: "fake",
  dockerDaemon: false,
  maxVcpu: 6,
  maxMemoryMiB: 16384,
  maxStorageMiB: 10240,
  maxDurationS: 86400,
  archs: ["amd64"],
  billing: "per-second",
});

const POOL_FIELDS = ["image", "vcpu", "memoryMiB", "storageMiB", "timeoutS", "waitTimeoutS"];

const emptyState = () => ({ project: null, keys: {}, bucket: null, definitions: {}, runs: [] });

/**
 * In-memory provider with the full contract, for tests and dry runs.
 * @param {{ config: import("../../core/types.mjs").Config, store?: ReturnType<typeof memoryStore> }} deps
 * @returns {import("../../core/types.mjs").Provider & { setRunState: Function }}
 */
export function createProvider({ config, store = memoryStore() }) {
  const state = async () => (await store.load()) ?? emptyState();
  const definitionName = (tag) => `${config.name}-${tag}`;

  function keyStep(id, label, present, rotate) {
    const action = !present ? "create" : rotate ? "update" : "noop";
    return step(
      id,
      action,
      `api key ${config.name}-${label}`,
      action === "update" ? "rotate" : undefined,
    );
  }

  return {
    capabilities: () => CAPABILITIES,

    async plan({ rotate = [] } = {}) {
      const s = await state();
      const steps = [];
      steps.push(step("project", s.project ? "noop" : "create", `project ${config.name}`));
      steps.push(
        keyStep("key:dispatch", "dispatch", Boolean(s.keys.dispatch), rotate.includes("key")),
      );
      if (config.cache.type === "s3") {
        steps.push(keyStep("key:cache", "cache", Boolean(s.keys.cache), rotate.includes("cache")));
        const bucketAction = !s.bucket
          ? "create"
          : s.bucket.expireDays === config.cache.expireDays
            ? "noop"
            : "update";
        steps.push(step("bucket", bucketAction, `cache bucket ${config.name}-cache`));
      } else {
        if (s.keys.cache) steps.push(step("key:cache", "delete", `api key ${config.name}-cache`));
        if (s.bucket) steps.push(step("bucket", "delete", `cache bucket ${config.name}-cache`));
      }
      for (const pool of config.pools) {
        const current = s.definitions[pool.tag];
        const action = !current ? "create" : same(current, pool) ? "noop" : "update";
        steps.push(
          step(`definition:${pool.tag}`, action, `job definition ${definitionName(pool.tag)}`),
        );
      }
      for (const tag of Object.keys(s.definitions)) {
        if (!config.pools.some((pool) => pool.tag === tag)) {
          steps.push(step(`definition:${tag}`, "delete", `job definition ${definitionName(tag)}`));
        }
      }
      return steps;
    },

    async apply(steps) {
      const s = await state();
      const outputs = {};
      for (const st of steps) {
        if (st.action === "noop") continue;
        if (st.id === "project") {
          s.project = { id: randomUUID(), name: config.name };
        } else if (st.id === "key:dispatch") {
          s.keys.dispatch = fakeKey();
          outputs.CIBURST_DISPATCH_ACCESS_KEY = s.keys.dispatch.accessKey;
          outputs.CIBURST_DISPATCH_SECRET_KEY = s.keys.dispatch.secretKey;
        } else if (st.id === "key:cache") {
          if (st.action === "delete") {
            delete s.keys.cache;
          } else {
            s.keys.cache = fakeKey();
            outputs.CIBURST_CACHE_ACCESS_KEY = s.keys.cache.accessKey;
            outputs.CIBURST_CACHE_SECRET_KEY = s.keys.cache.secretKey;
          }
        } else if (st.id === "bucket") {
          s.bucket =
            st.action === "delete"
              ? null
              : { name: `${config.name}-cache`, expireDays: config.cache.expireDays };
        } else if (st.id.startsWith("definition:")) {
          const tag = st.id.slice("definition:".length);
          if (st.action === "delete") delete s.definitions[tag];
          else
            s.definitions[tag] = pick(
              config.pools.find((pool) => pool.tag === tag),
              POOL_FIELDS,
            );
        } else {
          throw new Error(`unknown step ${st.id}`);
        }
      }
      await store.save(s);
      return outputs;
    },

    async destroy() {
      await store.save(emptyState());
    },

    async start({ pool, env, labels }) {
      const s = await state();
      if (!s.definitions[pool.tag]) throw new Error(`pool ${pool.tag} is not provisioned`);
      if (!env?.RUNNER_TOKEN) throw new Error("RUNNER_TOKEN missing");
      const run = {
        runId: `fake-${randomUUID()}`,
        pool: pool.tag,
        state: "queued",
        createdAt: new Date().toISOString(),
        labels: { ...labels },
      };
      s.runs.push(run);
      await store.save(s);
      return { runId: run.runId };
    },

    async list({ pool } = {}) {
      const s = await state();
      return s.runs
        .filter((run) => !pool || run.pool === pool)
        .map((run) => ({ ...run, labels: { ...run.labels } }));
    },

    async stop(runId) {
      const s = await state();
      const run = s.runs.find((r) => r.runId === runId);
      if (!run) throw new Error(`unknown run ${runId}`);
      run.state = "done";
      await store.save(s);
    },

    /** Test helper outside the contract. */
    async setRunState(runId, runState) {
      const s = await state();
      const run = s.runs.find((r) => r.runId === runId);
      if (!run) throw new Error(`unknown run ${runId}`);
      run.state = runState;
      await store.save(s);
    },
  };
}

function step(id, action, resource, detail) {
  return detail ? { id, action, resource, detail } : { id, action, resource };
}

function same(current, pool) {
  return POOL_FIELDS.every((field) => current[field] === pool[field]);
}

function pick(object, fields) {
  return Object.fromEntries(fields.map((field) => [field, object[field]]));
}

function fakeKey() {
  return {
    accessKey: `SCWFAKE${randomBytes(6).toString("hex").toUpperCase()}`,
    secretKey: randomUUID(),
  };
}
