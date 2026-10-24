import { timingSafeEqual } from "node:crypto";
import { poolForStage } from "./config.mjs";

const LIVE = new Set(["queued", "running"]);

/**
 * Environment a run receives on top of its pool definition. Secret values only.
 * @param {import("./types.mjs").Config} config
 * @param {import("./types.mjs").Secrets} secrets
 * @param {import("./types.mjs").Pool} pool
 */
export function runEnv(config, secrets, pool) {
  const token = secrets.runnerTokens?.[pool.tag];
  if (!token) throw new Error(`no runner token for pool ${pool.tag}`);
  const env = { RUNNER_TOKEN: token };
  if (config.cache.type === "s3") {
    if (!secrets.cache) throw new Error("cache is s3 but no cache key is configured");
    env.CACHE_S3_ACCESS_KEY = secrets.cache.accessKey;
    env.CACHE_S3_SECRET_KEY = secrets.cache.secretKey;
  }
  return env;
}

/**
 * @param {{ config: import("./types.mjs").Config, provider: import("./types.mjs").Provider, secrets: import("./types.mjs").Secrets, log: { info: Function, warn: Function, error: Function } }} deps
 */
export function createDispatcher({ config, provider, secrets, log }) {
  const reply = (status, result, extra = {}) => ({ status, body: { result, ...extra } });
  const ignored = (reason, extra) => reply(200, "ignored", { reason, ...extra });

  /** @param {import("./types.mjs").DispatchRequest} request @returns {Promise<import("./types.mjs").DispatchResponse>} */
  async function handle({ headers, body }) {
    if (!tokenMatches(header(headers, "x-gitlab-token"), secrets.hookToken)) {
      return { status: 401, body: {} };
    }
    const event = parseBody(body);
    if (event === undefined) return reply(400, "bad_request");
    if (!event || event.object_kind !== "build") return ignored("not_job_event");
    if (event.build_status !== "pending") return ignored("status", { status: event.build_status });
    if (!config.gitlab.projectIds.includes(event.project_id)) return ignored("project");

    const pool = poolForStage(config, event.build_stage);
    const labels = {
      project: String(event.project_id),
      pipeline: String(event.pipeline_id),
      job: String(event.build_id),
      stage: String(event.build_stage ?? ""),
    };
    try {
      const live = (await provider.list({ pool: pool.tag })).filter((run) => LIVE.has(run.state));
      if (live.length >= pool.maxRuns) {
        log.info("at_capacity", { ...labels, pool: pool.tag, live: live.length });
        return reply(200, "at_capacity", { pool: pool.tag });
      }
      const { runId } = await provider.start({ pool, env: runEnv(config, secrets, pool), labels });
      log.info("started", { ...labels, pool: pool.tag, run: runId });
      return reply(200, "started", { pool: pool.tag, runId });
    } catch (error) {
      log.error("start_failed", { ...labels, pool: pool.tag, error: error.message });
      return reply(500, "error");
    }
  }

  return { handle };
}

function header(headers, name) {
  const key = Object.keys(headers ?? {}).find((k) => k.toLowerCase() === name);
  return key === undefined ? undefined : headers[key];
}

function tokenMatches(given, expected) {
  if (typeof given !== "string" || typeof expected !== "string" || expected.length === 0)
    return false;
  const a = Buffer.from(given);
  const b = Buffer.from(expected);
  return a.length === b.length && timingSafeEqual(a, b);
}

function parseBody(body) {
  if (typeof body !== "string") return body ?? null;
  try {
    return JSON.parse(body);
  } catch {
    return undefined;
  }
}
