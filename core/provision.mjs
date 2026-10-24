import { randomBytes } from "node:crypto";
import { checkJobs } from "./check.mjs";

export const KEYS = Object.freeze({
  hookToken: "CIBURST_HOOK_TOKEN",
  dispatchUrl: "CIBURST_DISPATCH_URL",
  dispatchAccessKey: "CIBURST_DISPATCH_ACCESS_KEY",
  dispatchSecretKey: "CIBURST_DISPATCH_SECRET_KEY",
  cacheAccessKey: "CIBURST_CACHE_ACCESS_KEY",
  cacheSecretKey: "CIBURST_CACHE_SECRET_KEY",
  runnerId: (tag) => `CIBURST_RUNNER_ID_${envName(tag)}`,
  runnerToken: (tag) => `CIBURST_RUNNER_TOKEN_${envName(tag)}`,
});

export const ROTATIONS = Object.freeze(["runner", "hook", "key", "cache"]);

/** @returns {import("./types.mjs").Secrets} */
export function secretsFromOutputs(config, outputs) {
  const runnerTokens = {};
  for (const pool of config.pools) {
    const token = outputs[KEYS.runnerToken(pool.tag)];
    if (token) runnerTokens[pool.tag] = token;
  }
  const pair = (access, secret) =>
    outputs[secret] ? { accessKey: outputs[access] ?? "", secretKey: outputs[secret] } : undefined;
  return {
    hookToken: outputs[KEYS.hookToken],
    runnerTokens,
    dispatch: pair(KEYS.dispatchAccessKey, KEYS.dispatchSecretKey),
    cache: pair(KEYS.cacheAccessKey, KEYS.cacheSecretKey),
  };
}

/**
 * Orchestrates provider, GitLab and host steps. GitLab is optional (plan without a token).
 * @param {{ config: import("./types.mjs").Config, provider: import("./types.mjs").Provider, host: import("./types.mjs").Host, gitlab?: ReturnType<import("./gitlab.mjs").createGitlab>, outputs: { read: Function, write: Function, clear: Function }, log: { info: Function, warn: Function, error: Function } }} deps
 */
export function createProvisioner({ config, provider, host, gitlab, outputs, log }) {
  const runnerDescription = (pool) => `${config.name}-${pool.tag}`;
  const [primaryProject, ...otherProjects] = config.gitlab.projectIds;

  async function runnerSteps(current, { rotate = false } = {}) {
    if (!gitlab) return [];
    const runners = await gitlab.listRunners(primaryProject);
    const steps = [];
    for (const pool of config.pools) {
      const description = runnerDescription(pool);
      const existing = runners.find((runner) => runner.description === description);
      const hasToken = Boolean(current[KEYS.runnerToken(pool.tag)]);
      let action = "create";
      let detail;
      if (existing && rotate) {
        action = "update";
        detail = "delete and recreate, new token";
      } else if (existing && hasToken) {
        action = "noop";
      } else if (existing) {
        action = "update";
        detail = "token missing in outputs, delete and recreate";
      }
      steps.push({
        id: `runner:${pool.tag}`,
        action,
        resource: `gitlab runner ${description}`,
        detail,
      });
    }
    for (const projectId of otherProjects) {
      const enabled = await gitlab.listRunners(projectId);
      for (const pool of config.pools) {
        const present = enabled.some((runner) => runner.description === runnerDescription(pool));
        steps.push({
          id: `runner-enable:${projectId}:${pool.tag}`,
          action: present ? "noop" : "create",
          resource: `gitlab runner ${runnerDescription(pool)} enabled in project ${projectId}`,
        });
      }
    }
    return steps;
  }

  async function hookSteps({ rotate = false } = {}) {
    if (!gitlab) return [];
    const url = await host.url();
    const steps = [];
    for (const projectId of config.gitlab.projectIds) {
      const hooks = await gitlab.listHooks(projectId);
      const existing = url ? hooks.find((hook) => hook.url === url) : undefined;
      let action = "create";
      let detail = url ? undefined : "url known after the dispatcher is deployed";
      if (existing && rotate) {
        action = "update";
        detail = "new token";
      } else if (existing) {
        action = "noop";
      }
      steps.push({
        id: `hook:${projectId}`,
        action,
        resource: `gitlab hook in project ${projectId}`,
        detail,
      });
    }
    return steps;
  }

  async function applyRunners(steps) {
    const created = {};
    const runners = await gitlab.listRunners(primaryProject);
    for (const pool of config.pools) {
      const step = steps.find((s) => s.id === `runner:${pool.tag}`);
      if (!step || step.action === "noop") continue;
      const existing = runners.find((runner) => runner.description === runnerDescription(pool));
      if (existing) await gitlab.deleteRunner(existing.id);
      const runner = await gitlab.createRunner({
        projectId: primaryProject,
        tag: pool.tag,
        description: runnerDescription(pool),
      });
      created[KEYS.runnerId(pool.tag)] = String(runner.id);
      created[KEYS.runnerToken(pool.tag)] = runner.token;
      for (const projectId of otherProjects) await gitlab.enableRunner(projectId, runner.id);
      log.info("runner", { pool: pool.tag, id: runner.id });
    }
    if (Object.keys(created).length > 0) await outputs.write(created, { overwrite: true });
    const current = await outputs.read();
    for (const step of steps) {
      if (!step.id.startsWith("runner-enable:") || step.action === "noop") continue;
      const [, projectId, tag] = step.id.split(":");
      if (created[KEYS.runnerId(tag)]) continue;
      const runnerId = current[KEYS.runnerId(tag)];
      if (!runnerId) {
        log.warn("runner_enable_skipped", {
          project: projectId,
          pool: tag,
          reason: "runner id unknown",
        });
        continue;
      }
      await gitlab.enableRunner(Number(projectId), Number(runnerId));
    }
  }

  async function applyHooks(steps, hookToken) {
    const url = await host.url();
    if (!url) throw new Error("dispatcher url unknown, the host did not deploy");
    for (const step of steps) {
      if (step.action === "noop") continue;
      const projectId = Number(step.id.split(":")[1]);
      const hooks = await gitlab.listHooks(projectId);
      const existing = hooks.find((hook) => hook.url === url);
      if (existing)
        await gitlab.updateHook({ projectId, hookId: existing.id, url, token: hookToken });
      else await gitlab.createHook({ projectId, url, token: hookToken });
      log.info("hook", { project: projectId });
    }
  }

  async function ensureHookToken({ rotate = false } = {}) {
    const current = await outputs.read();
    if (current[KEYS.hookToken] && !rotate) return current[KEYS.hookToken];
    const token = randomBytes(24).toString("base64url");
    await outputs.write({ [KEYS.hookToken]: token }, { overwrite: true });
    return token;
  }

  function providerRotations(rotate) {
    return rotate.filter((kind) => kind === "key" || kind === "cache");
  }

  /** @param {{ rotate?: string[] }} [options] */
  async function plan({ rotate = [] } = {}) {
    assertRotations(rotate);
    const current = await outputs.read();
    const secrets = secretsFromOutputs(config, current);
    return [
      ...(await provider.plan({ rotate: providerRotations(rotate) })),
      ...(await runnerSteps(current, { rotate: rotate.includes("runner") })),
      ...(await host.plan({ secrets })),
      ...(await hookSteps({ rotate: rotate.includes("hook") })),
    ];
  }

  /** Applies in phases so that later phases see the outputs of earlier ones. */
  async function apply({ rotate = [] } = {}) {
    assertRotations(rotate);
    const applied = [];
    const hookToken = await ensureHookToken({ rotate: rotate.includes("hook") });

    const providerRotate = providerRotations(rotate);
    const providerSteps = await provider.plan({ rotate: providerRotate });
    const providerOutputs = await provider.apply(providerSteps);
    await outputs.write(providerOutputs, { overwrite: providerRotate.length > 0 });
    applied.push(...providerSteps);

    const runners = await runnerSteps(await outputs.read(), { rotate: rotate.includes("runner") });
    if (gitlab) await applyRunners(runners);
    applied.push(...runners);

    const secrets = secretsFromOutputs(config, await outputs.read());
    secrets.hookToken = hookToken;
    const hostSteps = await host.plan({ secrets });
    const hostOutputs = await host.apply(hostSteps, { secrets });
    await outputs.write(hostOutputs, { overwrite: true });
    applied.push(...hostSteps);

    const hooks = await hookSteps({ rotate: rotate.includes("hook") });
    if (gitlab) await applyHooks(hooks, hookToken);
    applied.push(...hooks);
    return applied;
  }

  async function destroy() {
    const current = await outputs.read();
    if (gitlab) {
      const url = await host.url();
      for (const projectId of config.gitlab.projectIds) {
        const hooks = await gitlab.listHooks(projectId);
        for (const hook of hooks)
          if (url && hook.url === url) await gitlab.deleteHook(projectId, hook.id);
      }
      const runners = await gitlab.listRunners(primaryProject);
      for (const pool of config.pools) {
        const existing = runners.find((runner) => runner.description === runnerDescription(pool));
        const id = existing?.id ?? current[KEYS.runnerId(pool.tag)];
        if (id) await gitlab.deleteRunner(id);
      }
    }
    await host.destroy();
    await provider.destroy();
    await outputs.clear();
    log.info("destroyed", { name: config.name });
  }

  async function status() {
    const runs = await provider.list();
    const live = (run) => run.state === "queued" || run.state === "running";
    const pools = config.pools.map((pool) => ({
      tag: pool.tag,
      live: runs.filter((run) => run.pool === pool.tag && live(run)).length,
      maxRuns: pool.maxRuns,
    }));
    return { url: await host.url(), pools, runs };
  }

  async function check() {
    if (!gitlab) throw new Error("check needs a GitLab token");
    const report = [];
    for (const projectId of config.gitlab.projectIds) {
      const lint = await gitlab.lintJobs(projectId);
      const problems = lint.valid ? checkJobs(config, lint.jobs ?? []) : [];
      report.push({ projectId, valid: Boolean(lint.valid), errors: lint.errors ?? [], problems });
    }
    return report;
  }

  return { plan, apply, destroy, status, check };
}

function assertRotations(rotate) {
  for (const kind of rotate) {
    if (!ROTATIONS.includes(kind))
      throw new Error(`unknown rotation ${kind}, use ${ROTATIONS.join(", ")}`);
  }
}

function envName(tag) {
  return tag.toUpperCase().replace(/[^A-Z0-9]/g, "_");
}
