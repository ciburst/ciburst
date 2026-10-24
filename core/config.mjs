import { readFile } from "node:fs/promises";

export const POOL_DEFAULTS = Object.freeze({
  stages: [],
  vcpu: 2,
  memoryMiB: 4096,
  storageMiB: 10240,
  timeoutS: 3600,
  waitTimeoutS: 60,
  maxRuns: 10,
});

export const CACHE_DEFAULTS = Object.freeze({ type: "none", expireDays: 30 });

const NAME = /^[a-z][a-z0-9-]{1,30}$/;
const TAG = /^[a-z0-9][a-z0-9-]{0,62}$/;
const TYPE = /^[a-z0-9-]+$/;
const POOL_NUMBERS = ["vcpu", "memoryMiB", "storageMiB", "timeoutS", "waitTimeoutS", "maxRuns"];
const LIMITS = [
  ["vcpu", "maxVcpu"],
  ["memoryMiB", "maxMemoryMiB"],
  ["storageMiB", "maxStorageMiB"],
  ["timeoutS", "maxDurationS"],
];

export class ConfigError extends Error {
  constructor(errors) {
    super(`invalid config:\n  ${errors.join("\n  ")}`);
    this.name = "ConfigError";
    this.errors = errors;
  }
}

/** @returns {import("./types.mjs").Config} */
export function validateConfig(raw) {
  if (!isObject(raw)) throw new ConfigError(["config must be a JSON object"]);
  const errors = [];
  const config = { ...raw };

  if (typeof config.name !== "string" || !NAME.test(config.name)) {
    errors.push("name: 2 to 31 lowercase letters, digits or hyphens, starting with a letter");
  }

  if (!isObject(config.gitlab)) {
    errors.push("gitlab: object with url and projectIds");
  } else {
    config.gitlab = { ...config.gitlab };
    if (!isHttpsUrl(config.gitlab.url)) errors.push("gitlab.url: https URL");
    else config.gitlab.url = config.gitlab.url.replace(/\/+$/, "");
    const ids = config.gitlab.projectIds;
    if (
      !Array.isArray(ids) ||
      ids.length === 0 ||
      !ids.every((id) => Number.isInteger(id) && id > 0)
    ) {
      errors.push("gitlab.projectIds: non-empty array of positive integers");
    }
  }

  for (const key of ["host", "provider"]) {
    if (
      !isObject(config[key]) ||
      typeof config[key].type !== "string" ||
      !TYPE.test(config[key].type)
    ) {
      errors.push(`${key}.type: lowercase letters, digits and hyphens`);
    }
  }

  if (!Array.isArray(config.pools) || config.pools.length === 0) {
    errors.push("pools: non-empty array");
  } else {
    const tags = new Set();
    const stageOwner = new Map();
    config.pools = config.pools.map((entry, index) => {
      const where = `pools[${index}]`;
      if (!isObject(entry)) {
        errors.push(`${where}: object`);
        return entry;
      }
      const pool = { ...POOL_DEFAULTS, ...entry };
      if (typeof pool.tag !== "string" || !TAG.test(pool.tag)) {
        errors.push(`${where}.tag: lowercase letters, digits and hyphens`);
      } else if (tags.has(pool.tag)) {
        errors.push(`${where}.tag: duplicate ${pool.tag}`);
      } else {
        tags.add(pool.tag);
      }
      if (typeof pool.image !== "string" || pool.image.length === 0) {
        errors.push(`${where}.image: required`);
      }
      for (const field of POOL_NUMBERS) {
        if (!Number.isInteger(pool[field]) || pool[field] <= 0) {
          errors.push(`${where}.${field}: positive integer`);
        }
      }
      if (!Array.isArray(pool.stages) || !pool.stages.every((s) => typeof s === "string" && s)) {
        errors.push(`${where}.stages: array of stage names`);
      } else {
        for (const stage of pool.stages) {
          if (stageOwner.has(stage)) {
            errors.push(
              `${where}.stages: ${stage} already mapped to pool ${stageOwner.get(stage)}`,
            );
          } else {
            stageOwner.set(stage, pool.tag);
          }
        }
      }
      return pool;
    });
    if (typeof config.defaultPool !== "string") errors.push("defaultPool: required pool tag");
    else if (!tags.has(config.defaultPool)) {
      errors.push(`defaultPool: ${config.defaultPool} is not a pool`);
    }
  }

  config.cache = { ...CACHE_DEFAULTS, ...(isObject(config.cache) ? config.cache : {}) };
  if (!["none", "s3"].includes(config.cache.type)) errors.push("cache.type: none or s3");
  if (!Number.isInteger(config.cache.expireDays) || config.cache.expireDays <= 0) {
    errors.push("cache.expireDays: positive integer");
  }

  if (errors.length > 0) throw new ConfigError(errors);
  return config;
}

/** @returns {string[]} errors */
export function checkCapabilities(config, caps) {
  const errors = [];
  config.pools.forEach((pool, index) => {
    for (const [field, limit] of LIMITS) {
      if (pool[field] > caps[limit]) {
        errors.push(
          `pools[${index}].${field}: ${pool[field]} above ${caps.name} maximum ${caps[limit]}`,
        );
      }
    }
  });
  return errors;
}

export function poolByTag(config, tag) {
  return config.pools.find((pool) => pool.tag === tag);
}

export function poolForStage(config, stage) {
  return (
    config.pools.find((pool) => pool.stages.includes(stage)) ??
    poolByTag(config, config.defaultPool)
  );
}

export async function loadConfig(path) {
  return validateConfig(JSON.parse(await readFile(path, "utf8")));
}

function isObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function isHttpsUrl(value) {
  try {
    return typeof value === "string" && new URL(value).protocol === "https:";
  } catch {
    return false;
  }
}
