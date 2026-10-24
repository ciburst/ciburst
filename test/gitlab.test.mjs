import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { createGitlab, GitlabError } from "../core/gitlab.mjs";
import { createFakeGitlab, FAKE_TOKEN } from "./fakes/gitlab-api.mjs";

const jobs = { 7: [{ name: "unit", stage: "test", tag_list: ["ci"] }] };

function client(options) {
  const fake = createFakeGitlab(options);
  return {
    gitlab: createGitlab({ url: "https://gitlab.example/", token: FAKE_TOKEN, fetch: fake.fetch }),
    fake,
  };
}

describe("gitlab client", () => {
  it("creates, lists, enables and deletes runners", async () => {
    const { gitlab, fake } = client();
    const created = await gitlab.createRunner({ projectId: 7, tag: "ci", description: "demo-ci" });
    assert.match(created.token, /^glrt-/);
    const listed = await gitlab.listRunners(7);
    assert.equal(listed.length, 1);
    assert.equal(listed[0].description, "demo-ci");
    assert.deepEqual(listed[0].tag_list, ["ci"]);
    assert.equal(listed[0].token, undefined);
    assert.equal((await gitlab.listRunners(8)).length, 0);
    await gitlab.enableRunner(8, created.id);
    assert.equal((await gitlab.listRunners(8)).length, 1);
    await gitlab.deleteRunner(created.id);
    assert.equal((await gitlab.listRunners(7)).length, 0);
    const call = fake.state.calls.find((c) => c.path === "/user/runners");
    assert.deepEqual(call.body, {
      runner_type: "project_type",
      project_id: 7,
      tag_list: ["ci"],
      run_untagged: false,
      description: "demo-ci",
    });
  });

  it("manages hooks", async () => {
    const { gitlab, fake } = client();
    const hook = await gitlab.createHook({ projectId: 7, url: "https://d.example/", token: "t1" });
    assert.equal(hook.token, undefined);
    assert.equal((await gitlab.listHooks(7))[0].url, "https://d.example/");
    assert.equal(fake.state.hooks[0].token, "t1");
    assert.equal(fake.state.hooks[0].push_events, false);
    assert.equal(fake.state.hooks[0].job_events, true);
    await gitlab.updateHook({
      projectId: 7,
      hookId: hook.id,
      url: "https://d.example/",
      token: "t2",
    });
    assert.equal(fake.state.hooks[0].token, "t2");
    await gitlab.deleteHook(7, hook.id);
    assert.deepEqual(await gitlab.listHooks(7), []);
  });

  it("lints jobs", async () => {
    const { gitlab } = client({
      jobs,
      lintErrors: { 8: ["jobs config should contain at least one visible job"] },
    });
    const ok = await gitlab.lintJobs(7);
    assert.equal(ok.valid, true);
    assert.deepEqual(ok.jobs, jobs[7]);
    const bad = await gitlab.lintJobs(8);
    assert.equal(bad.valid, false);
    assert.equal(bad.errors.length, 1);
  });

  it("throws a GitlabError with the status", async () => {
    const fake = createFakeGitlab();
    const gitlab = createGitlab({
      url: "https://gitlab.example",
      token: "wrong",
      fetch: fake.fetch,
    });
    await assert.rejects(
      () => gitlab.listRunners(7),
      (error) => error instanceof GitlabError && error.status === 401 && /401/.test(error.message),
    );
    const good = createGitlab({
      url: "https://gitlab.example",
      token: FAKE_TOKEN,
      fetch: fake.fetch,
    });
    await assert.rejects(
      () => good.deleteRunner(1),
      (error) => error.status === 404,
    );
  });
});
