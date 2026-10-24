import { describe, it, beforeEach } from "node:test";
import assert from "node:assert/strict";
import { validateConfig } from "../core/config.mjs";
import { createDispatcher, runEnv } from "../core/dispatch.mjs";
import { createMemoryLog } from "../core/log.mjs";
import { createProvider } from "../providers/fake/provider.mjs";

const config = validateConfig({
  name: "demo",
  gitlab: { url: "https://gitlab.example", projectIds: [7] },
  host: { type: "fake" },
  provider: { type: "fake" },
  defaultPool: "ci",
  pools: [
    { tag: "ci", image: "img/node", stages: ["test"] },
    { tag: "build", image: "img/kaniko", stages: ["build"], maxRuns: 1 },
  ],
  cache: { type: "s3" },
});

const secrets = {
  hookToken: "hook-secret",
  runnerTokens: { ci: "glrt-ci", build: "glrt-build" },
  cache: { accessKey: "AK", secretKey: "SK" },
};

const event = (overrides = {}) => ({
  object_kind: "build",
  build_id: 42,
  build_stage: "test",
  build_status: "pending",
  pipeline_id: 9,
  project_id: 7,
  ...overrides,
});

const request = (body, token = "hook-secret") => ({
  headers: { "X-Gitlab-Token": token, "X-Gitlab-Event": "Job Hook" },
  body: typeof body === "string" ? body : JSON.stringify(body),
});

describe("dispatcher", () => {
  let provider;
  let log;
  let dispatcher;
  let started;

  beforeEach(async () => {
    provider = createProvider({ config });
    await provider.apply(await provider.plan());
    started = [];
    const recording = {
      ...provider,
      start: (req) => {
        started.push(req);
        return provider.start(req);
      },
    };
    log = createMemoryLog();
    dispatcher = createDispatcher({ config, provider: recording, secrets, log });
  });

  it("rejects a missing or wrong token without a body", async () => {
    assert.deepEqual(await dispatcher.handle({ headers: {}, body: "{}" }), {
      status: 401,
      body: {},
    });
    assert.equal((await dispatcher.handle(request(event(), "nope"))).status, 401);
    assert.equal((await dispatcher.handle(request(event(), "hook-secret-longer"))).status, 401);
  });

  it("rejects an empty expected token", async () => {
    const d = createDispatcher({ config, provider, secrets: { ...secrets, hookToken: "" }, log });
    assert.equal((await d.handle(request(event(), ""))).status, 401);
  });

  it("answers 400 for a body that is not JSON", async () => {
    const response = await dispatcher.handle(request("{not json"));
    assert.deepEqual(response, { status: 400, body: { result: "bad_request" } });
  });

  it("ignores events that are not pending jobs of a configured project", async () => {
    const cases = [
      [event({ object_kind: "pipeline" }), "not_job_event"],
      [null, "not_job_event"],
      [event({ build_status: "running" }), "status"],
      [event({ project_id: 8 }), "project"],
    ];
    for (const [body, reason] of cases) {
      const response = await dispatcher.handle(request(body));
      assert.equal(response.status, 200);
      assert.equal(response.body.result, "ignored");
      assert.equal(response.body.reason, reason);
    }
    assert.equal(started.length, 0);
  });

  it("starts a run in the pool of the stage with token, cache keys and labels", async () => {
    const response = await dispatcher.handle(request(event({ build_stage: "build" })));
    assert.equal(response.status, 200);
    assert.equal(response.body.result, "started");
    assert.equal(response.body.pool, "build");
    assert.match(response.body.runId, /^fake-/);
    assert.equal(started.length, 1);
    assert.equal(started[0].pool.tag, "build");
    assert.deepEqual(started[0].env, {
      RUNNER_TOKEN: "glrt-build",
      CACHE_S3_ACCESS_KEY: "AK",
      CACHE_S3_SECRET_KEY: "SK",
    });
    assert.deepEqual(started[0].labels, { project: "7", pipeline: "9", job: "42", stage: "build" });
    assert.ok(
      log.lines.some((line) => line.startsWith("info started") && line.includes('pool="build"')),
    );
    assert.ok(!log.lines.some((line) => line.includes("glrt-")));
  });

  it("falls back to the default pool and accepts header names in any case", async () => {
    const response = await dispatcher.handle({
      headers: { "x-gitlab-token": "hook-secret" },
      body: event({ build_stage: "deploy" }),
    });
    assert.equal(response.body.result, "started");
    assert.equal(response.body.pool, "ci");
  });

  it("answers at_capacity once a pool has maxRuns live runs", async () => {
    const first = await dispatcher.handle(request(event({ build_stage: "build" })));
    assert.equal(first.body.result, "started");
    const second = await dispatcher.handle(request(event({ build_stage: "build", build_id: 43 })));
    assert.deepEqual(second, { status: 200, body: { result: "at_capacity", pool: "build" } });
    await provider.stop(first.body.runId);
    const third = await dispatcher.handle(request(event({ build_stage: "build", build_id: 44 })));
    assert.equal(third.body.result, "started");
  });

  it("answers 500 and logs when the provider fails", async () => {
    const failing = createDispatcher({
      config,
      provider: { ...provider, start: async () => Promise.reject(new Error("quota")) },
      secrets,
      log,
    });
    const response = await failing.handle(request(event()));
    assert.deepEqual(response, { status: 500, body: { result: "error" } });
    assert.ok(
      log.lines.some((line) => line.startsWith("error start_failed") && line.includes("quota")),
    );
  });
});

describe("runEnv", () => {
  it("requires the pool token and the cache key when the cache is s3", () => {
    assert.throws(
      () => runEnv(config, { runnerTokens: {} }, config.pools[0]),
      /no runner token for pool ci/,
    );
    assert.throws(
      () => runEnv(config, { runnerTokens: { ci: "t" } }, config.pools[0]),
      /no cache key/,
    );
    const noCache = validateConfig({ ...config, cache: { type: "none" } });
    assert.deepEqual(runEnv(noCache, { runnerTokens: { ci: "t" } }, noCache.pools[0]), {
      RUNNER_TOKEN: "t",
    });
  });
});
