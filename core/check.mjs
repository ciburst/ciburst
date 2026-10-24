import { poolForStage } from "./config.mjs";

const DEFAULT_STAGE = "test";

/**
 * Compares the jobs of a linted pipeline with the stage map.
 * @param {import("./types.mjs").Config} config
 * @param {{ name: string, stage?: string, tag_list?: string[] }[]} jobs
 * @returns {{ job: string, stage: string, tags: string[], pool: string, problem: string }[]}
 */
export function checkJobs(config, jobs) {
  const poolTags = new Set(config.pools.map((pool) => pool.tag));
  const problems = [];
  for (const job of jobs) {
    const tags = job.tag_list ?? [];
    const stage = job.stage ?? DEFAULT_STAGE;
    const expected = poolForStage(config, stage);
    const chosen = tags.filter((tag) => poolTags.has(tag));
    let problem;
    if (chosen.length > 1) {
      problem = `tagged for several pools: ${chosen.join(", ")}`;
    } else if (chosen.length === 1 && chosen[0] !== expected.tag) {
      problem = `stage ${stage} maps to pool ${expected.tag} but the job is tagged ${chosen[0]}`;
    } else if (tags.length === 0) {
      problem = `untagged: add tags [${expected.tag}] or a hosted runner tag`;
    }
    if (problem) problems.push({ job: job.name, stage, tags, pool: expected.tag, problem });
  }
  return problems;
}
