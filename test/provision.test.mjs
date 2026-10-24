import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { validateConfig } from "../core/config.mjs";
import { createProvisioner, secretsFromOutputs, KEYS } from "../core/provision.mjs";
import { createGitlab } from "../core/gitlab.mjs";
import { createMemoryOutputs } from "../core/plan.mjs";
import { createMemoryLog } from "../core/log.mjs";
import { createDispatcher } from "../core/dispatch.mjs";
import { createProvider } from "../providers/fake/provider.mjs";
import { createHost } from "../hosts/fake/host.mjs";
import { createFakeGitlab, FAKE_TOKEN } from "./fakes/gitlab-api.mjs";

const base = (overrides = {}) =>
  validateConfig({
    name: "demo",
    gitlab: { url: "https://gitlab.example", projectIds: [7] },
    host: { type: "fake" },
    provider: { type: "fake" },
    defaultPool: "ci",
    pools: [
      { tag: "ci", image: "img/node", stages: ["test"] },
      { tag: "build", image: "img/kaniko", stages: ["build"] },
    ],
    cache: { type: "s3" },
    ...overrides,
  });

function setup({ config = base(), gitlab = true, fakeOptions } = {}) {
  const fake = createFakeGitlab(fakeOptions);
  const provider = createProvider({ config });
  const host = createHost({ config });
  const outputs = createMemoryOutputs();
  const log = createMemoryLog();
  const provisioner = createProvisioner({
    config,
    provider,
    host,
    gitlab: gitlab
      ? createGitlab({ url: config.gitlab.url, token: FAKE_TOKEN, fetch: fake.fetch })
      : undefined,
    outputs,
    log,
  });
  return { config, fake, provider, host, outputs, log, provisioner };
}

const actions = (steps) => Object.fromEntries(steps.map((s) => [s.id, s.action]));

describe("provisioner", () => {
  it("plans everything on an empty state, applies, then plans nothing", async () => {
    const { provisioner, outputs, fake } = setup();
    const first = await provisioner.plan();
    assert.ok(
      first.every((s) => s.action === "create"),
      JSON.stringify(first),
    );
    assert.deepEqual(Object.keys(actions(first)).sort(), [
      "bucket",
      "definition:build",
      "definition:ci",
      "dispatcher",
      "hook:7",
      "key:cache",
      "key:dispatch",
      "project",
      "runner:build",
      "runner:ci",
    ]);
    assert.equal(
      first.find((s) => s.id === "hook:7").detail,
      "url known after the dispatcher is deployed",
    );

    const applied = await provisioner.apply();
    assert.ok(
      applied.every((s) => s.action === "create"),
      JSON.stringify(applied),
    );
    const values = await outputs.read();
    assert.ok(values[KEYS.hookToken]);
    assert.ok(values[KEYS.runnerToken("ci")].startsWith("glrt-"));
    assert.ok(values[KEYS.runnerToken("build")].startsWith("glrt-"));
    assert.ok(values[KEYS.runnerId("ci")]);
    assert.ok(values[KEYS.dispatchSecretKey]);
    assert.ok(values[KEYS.cacheSecretKey]);
    assert.equal(values[KEYS.dispatchUrl], "https://demo-dispatch.fake.invalid/");
    assert.equal(fake.state.runners.length, 2);
    assert.equal(fake.state.hooks.length, 1);
    assert.equal(fake.state.hooks[0].url, values[KEYS.dispatchUrl]);
    assert.equal(fake.state.hooks[0].token, values[KEYS.hookToken]);

    const second = await provisioner.plan();
    assert.ok(
      second.every((s) => s.action === "noop"),
      JSON.stringify(second),
    );
    const again = await provisioner.apply();
    assert.ok(
      again.every((s) => s.action === "noop"),
      JSON.stringify(again),
    );
    assert.deepEqual(await outputs.read(), values);
  });

  it("gives secrets to the host and the dispatcher works with them", async () => {
    const { provisioner, outputs, config, provider } = setup();
    await provisioner.apply();
    const secrets = secretsFromOutputs(config, await outputs.read());
    assert.deepEqual(Object.keys(secrets.runnerTokens).sort(), ["build", "ci"]);
    assert.ok(secrets.cache.secretKey);
    const dispatcher = createDispatcher({ config, provider, secrets, log: createMemoryLog() });
    const response = await dispatcher.handle({
      headers: { "x-gitlab-token": secrets.hookToken },
      body: {
        object_kind: "build",
        build_status: "pending",
        build_id: 1,
        pipeline_id: 2,
        project_id: 7,
        build_stage: "test",
      },
    });
    assert.equal(response.body.result, "started");
    const status = await provisioner.status();
    assert.deepEqual(status.pools, [
      { tag: "ci", live: 1, maxRuns: 10 },
      { tag: "build", live: 0, maxRuns: 10 },
    ]);
    assert.equal(status.url, "https://demo-dispatch.fake.invalid/");
  });

  it("enables the runners in every further project and hooks each project", async () => {
    const { provisioner, fake } = setup({
      config: base({ gitlab: { url: "https://gitlab.example", projectIds: [7, 8] } }),
    });
    const plan = await provisioner.plan();
    assert.equal(actions(plan)["runner-enable:8:ci"], "create");
    assert.equal(actions(plan)["hook:8"], "create");
    await provisioner.apply();
    assert.equal(fake.state.runners.length, 2);
    assert.ok(fake.state.runners.every((r) => r.projects.includes(8)));
    assert.equal(fake.state.hooks.length, 2);
    assert.ok((await provisioner.plan()).every((s) => s.action === "noop"));
  });

  it("recreates a runner whose token is missing and rotates on request", async () => {
    const { provisioner, outputs, fake } = setup();
    await provisioner.apply();
    const before = await outputs.read();
    await outputs.clear();
    await outputs.write({ [KEYS.hookToken]: before[KEYS.hookToken] });
    const plan = await provisioner.plan();
    assert.equal(actions(plan)["runner:ci"], "update");
    assert.match(plan.find((s) => s.id === "runner:ci").detail, /token missing/);
    await provisioner.apply();
    const after = await outputs.read();
    assert.notEqual(after[KEYS.runnerToken("ci")], before[KEYS.runnerToken("ci")]);
    assert.equal(fake.state.runners.length, 2);

    await provisioner.apply({ rotate: ["runner", "hook", "key", "cache"] });
    const rotated = await outputs.read();
    assert.notEqual(rotated[KEYS.runnerToken("ci")], after[KEYS.runnerToken("ci")]);
    assert.notEqual(rotated[KEYS.hookToken], after[KEYS.hookToken]);
    assert.notEqual(rotated[KEYS.dispatchSecretKey], after[KEYS.dispatchSecretKey]);
    assert.notEqual(rotated[KEYS.cacheSecretKey], after[KEYS.cacheSecretKey]);
    assert.equal(fake.state.hooks[0].token, rotated[KEYS.hookToken]);
    assert.equal(fake.state.runners.length, 2);
    assert.ok((await provisioner.plan()).every((s) => s.action === "noop"));
    await assert.rejects(() => provisioner.plan({ rotate: ["everything"] }), /unknown rotation/);
  });

  it("destroys gitlab resources, host, provider and outputs", async () => {
    const { provisioner, outputs, fake, provider, host } = setup();
    await provisioner.apply();
    await provisioner.destroy();
    assert.deepEqual(fake.state.runners, []);
    assert.deepEqual(fake.state.hooks, []);
    assert.deepEqual(await outputs.read(), {});
    assert.equal(await host.url(), undefined);
    assert.ok((await provider.plan()).every((s) => s.action === "create"));
  });

  it("plans and applies without gitlab when no token is given", async () => {
    const { provisioner, outputs } = setup({ gitlab: false });
    const plan = await provisioner.plan();
    assert.ok(!plan.some((s) => s.id.startsWith("runner") || s.id.startsWith("hook")));
    await provisioner.apply();
    const values = await outputs.read();
    assert.ok(values[KEYS.dispatchUrl]);
    assert.equal(values[KEYS.runnerToken("ci")], undefined);
    await assert.rejects(() => provisioner.check(), /needs a GitLab token/);
  });

  it("checks pipelines against the stage map", async () => {
    const { provisioner } = setup({
      config: base({ gitlab: { url: "https://gitlab.example", projectIds: [7, 8, 9] } }),
      fakeOptions: {
        jobs: {
          7: [{ name: "unit", stage: "test", tag_list: ["ci"] }],
          8: [{ name: "image", stage: "build", tag_list: ["ci"] }],
        },
        lintErrors: { 9: ["invalid yaml"] },
      },
    });
    const report = await provisioner.check();
    assert.deepEqual(
      report.map((r) => [r.projectId, r.valid, r.problems.length, r.errors.length]),
      [
        [7, true, 0, 0],
        [8, true, 1, 0],
        [9, false, 0, 1],
      ],
    );
    assert.equal(report[1].problems[0].job, "image");
  });
});
