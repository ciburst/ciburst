import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { validateConfig } from "../core/config.mjs";
import { checkJobs } from "../core/check.mjs";

const config = validateConfig({
  name: "demo",
  gitlab: { url: "https://gitlab.example", projectIds: [1] },
  host: { type: "fake" },
  provider: { type: "fake" },
  defaultPool: "ci",
  pools: [
    { tag: "ci", image: "img/node", stages: ["lint", "test"] },
    { tag: "build", image: "img/kaniko", stages: ["build"] },
  ],
});

describe("checkJobs", () => {
  it("accepts jobs whose tag matches their stage's pool or a foreign tag", () => {
    const problems = checkJobs(config, [
      { name: "unit", stage: "test", tag_list: ["ci"] },
      { name: "image", stage: "build", tag_list: ["build"] },
      { name: "deploy", stage: "deploy", tag_list: ["ci"] },
      { name: "hosted", stage: "test", tag_list: ["saas-linux-small-amd64"] },
      { name: "implicit-stage", tag_list: ["ci"] },
    ]);
    assert.deepEqual(problems, []);
  });

  it("reports mismatches, untagged jobs and double pool tags", () => {
    const problems = checkJobs(config, [
      { name: "wrong", stage: "build", tag_list: ["ci"] },
      { name: "naked", stage: "test", tag_list: [] },
      { name: "both", stage: "test", tag_list: ["ci", "build"] },
      { name: "no-list", stage: "lint" },
    ]);
    assert.deepEqual(
      problems.map((p) => [p.job, p.pool, p.problem]),
      [
        ["wrong", "build", "stage build maps to pool build but the job is tagged ci"],
        ["naked", "ci", "untagged: add tags [ci] or a hosted runner tag"],
        ["both", "ci", "tagged for several pools: ci, build"],
        ["no-list", "ci", "untagged: add tags [ci] or a hosted runner tag"],
      ],
    );
  });
});
