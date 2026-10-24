#!/usr/bin/env node
import { parseArgs } from "node:util";
import { resolve, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { loadConfig, checkCapabilities, ConfigError } from "../core/config.mjs";
import { createLog } from "../core/log.mjs";
import { createFileOutputs, readEnvFile, formatPlan, summarizePlan } from "../core/plan.mjs";
import { createGitlab } from "../core/gitlab.mjs";
import { createProvisioner, ROTATIONS } from "../core/provision.mjs";
import { fileStore } from "../core/store.mjs";

const USAGE = `usage: ciburst <command> [options]

commands
  plan                 show what apply would change
  apply                create or update every resource, write <local>/outputs.env
  destroy --yes <name> remove every resource of <name>
  status               live runs per pool
  check                compare every project's pipeline with the stage map
  rotate <kind>        runner | hook | key | cache, then apply

options
  --config <path>      default config.json
  --local <dir>        secrets and outputs directory, default .local
  --skip-gitlab        plan or apply without GitLab steps
`;

const ROOT = resolve(fileURLToPath(new URL("..", import.meta.url)));
const MODULE = /^[a-z0-9-]+$/;

async function main(argv) {
  const { values, positionals } = parseArgs({
    args: argv,
    allowPositionals: true,
    options: {
      config: { type: "string", default: "config.json" },
      local: { type: "string", default: ".local" },
      "skip-gitlab": { type: "boolean", default: false },
      yes: { type: "string" },
      help: { type: "boolean", short: "h", default: false },
    },
  });
  const [command, argument] = positionals;
  if (values.help || !command) {
    process.stdout.write(USAGE);
    return command ? 0 : 2;
  }

  const log = createLog();
  const config = await loadConfig(values.config);
  const localDir = resolve(values.local);
  const credentials = {
    scaleway: await readEnvFile(join(localDir, "scaleway.env")),
    gitlab: await readEnvFile(join(localDir, "gitlab.env")),
  };

  const provider = await loadModule("providers", config.provider.type, "createProvider", {
    config,
    credentials: credentials.scaleway,
    log,
    store: fileStore(join(localDir, "state.json"), "provider"),
  });
  const capabilityErrors = checkCapabilities(config, provider.capabilities());
  if (capabilityErrors.length > 0) throw new ConfigError(capabilityErrors);
  const host = await loadModule("hosts", config.host.type, "createHost", {
    config,
    credentials: credentials.scaleway,
    log,
    store: fileStore(join(localDir, "state.json"), "host"),
    root: ROOT,
  });

  const gitlabToken = process.env.GITLAB_TOKEN ?? credentials.gitlab.GITLAB_TOKEN;
  let gitlab;
  if (!values["skip-gitlab"]) {
    if (!gitlabToken) {
      throw new Error(
        `GITLAB_TOKEN missing in ${join(localDir, "gitlab.env")}; pass --skip-gitlab to leave GitLab untouched`,
      );
    }
    gitlab = createGitlab({ url: config.gitlab.url, token: gitlabToken });
  }

  const outputs = createFileOutputs(join(localDir, "outputs.env"));
  const provisioner = createProvisioner({ config, provider, host, gitlab, outputs, log });
  const out = (text) => process.stdout.write(`${text}\n`);

  switch (command) {
    case "plan": {
      const steps = await provisioner.plan();
      out(formatPlan(steps));
      out(summarizePlan(steps));
      return 0;
    }
    case "apply": {
      const steps = await provisioner.apply();
      out(formatPlan(steps));
      out(summarizePlan(steps));
      out(`outputs: ${join(localDir, "outputs.env")}`);
      return 0;
    }
    case "rotate": {
      if (!ROTATIONS.includes(argument ?? "")) {
        out(`rotate needs one of: ${ROTATIONS.join(", ")}`);
        return 2;
      }
      const steps = await provisioner.apply({ rotate: [argument] });
      out(formatPlan(steps));
      out(summarizePlan(steps));
      return 0;
    }
    case "destroy": {
      if (values.yes !== config.name) {
        out(`destroy removes every resource of ${config.name}; confirm with --yes ${config.name}`);
        return 2;
      }
      await provisioner.destroy();
      out(`destroyed ${config.name}`);
      return 0;
    }
    case "status": {
      const report = await provisioner.status();
      out(`dispatcher: ${report.url ?? "not deployed"}`);
      for (const pool of report.pools)
        out(`pool ${pool.tag}: ${pool.live} live of ${pool.maxRuns}`);
      for (const run of report.runs.slice(-20)) {
        out(
          `run ${run.runId} ${run.pool} ${run.state} ${run.createdAt} job=${run.labels.job ?? ""}`,
        );
      }
      return 0;
    }
    case "check": {
      const report = await provisioner.check();
      let failed = false;
      for (const project of report) {
        if (!project.valid) {
          failed = true;
          out(`project ${project.projectId}: pipeline invalid: ${project.errors.join("; ")}`);
          continue;
        }
        if (project.problems.length === 0) {
          out(`project ${project.projectId}: every job matches the stage map`);
          continue;
        }
        failed = true;
        for (const p of project.problems) {
          out(`project ${project.projectId}: job ${p.job} (stage ${p.stage}): ${p.problem}`);
        }
      }
      return failed ? 1 : 0;
    }
    default:
      process.stdout.write(USAGE);
      return 2;
  }
}

async function loadModule(kind, type, factory, deps) {
  if (!MODULE.test(type)) throw new Error(`${kind} type ${type} is not a module name`);
  const file = join(ROOT, kind, type, kind === "hosts" ? "host.mjs" : "provider.mjs");
  let module;
  try {
    module = await import(pathToFileURL(file).href);
  } catch (error) {
    if (error.code === "ERR_MODULE_NOT_FOUND") {
      throw new Error(`no ${kind.slice(0, -1)} named ${type} under ${kind}/`);
    }
    throw error;
  }
  if (typeof module[factory] !== "function") throw new Error(`${file} does not export ${factory}`);
  return module[factory](deps);
}

main(process.argv.slice(2)).then(
  (code) => process.exit(code),
  (error) => {
    process.stderr.write(`${error.message}\n`);
    process.exit(1);
  },
);
