import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import {
  validateConfig,
  loadConfig,
  checkCapabilities,
  poolForStage,
  ConfigError,
  POOL_DEFAULTS,
} from "../core/config.mjs";

const FIXTURE = fileURLToPath(new URL("./fixtures/config.fake.json", import.meta.url));

const valid = () => ({
  name: "demo",
  gitlab: { url: "https://gitlab.example/", projectIds: [1, 2] },
  host: { type: "fake" },
  provider: { type: "fake" },
  defaultPool: "ci",
  pools: [
    { tag: "ci", image: "img/node", stages: ["test"] },
    { tag: "build", image: "img/kaniko", stages: ["build"], vcpu: 4 },
  ],
});

function errorsOf(raw) {
  try {
    validateConfig(raw);
  } catch (error) {
    assert.ok(error instanceof ConfigError);
    return error.errors;
  }
  assert.fail("expected a ConfigError");
}

describe("validateConfig", () => {
  it("applies defaults and normalizes the url", async () => {
    const config = await loadConfig(FIXTURE);
    assert.equal(config.pools[0].vcpu, POOL_DEFAULTS.vcpu);
    assert.equal(config.pools[0].maxRuns, 10);
    assert.equal(config.pools[1].maxRuns, 2);
    assert.deepEqual(config.cache, { type: "s3", expireDays: 30 });
    const normalized = validateConfig(valid());
    assert.equal(normalized.gitlab.url, "https://gitlab.example");
    assert.deepEqual(normalized.cache, { type: "none", expireDays: 30 });
  });

  it("rejects a non-object", () => {
    assert.deepEqual(errorsOf(null), ["config must be a JSON object"]);
    assert.deepEqual(errorsOf([]), ["config must be a JSON object"]);
  });

  it("collects every error at once", () => {
    const raw = valid();
    raw.name = "Bad Name";
    raw.gitlab = { url: "http://insecure", projectIds: [] };
    raw.host = { type: "Fake!" };
    raw.defaultPool = "missing";
    raw.pools.push({ tag: "ci", image: "", stages: ["test"], vcpu: 0 });
    raw.cache = { type: "disk", expireDays: -1 };
    const errors = errorsOf(raw);
    const expect = (needle) =>
      assert.ok(
        errors.some((e) => e.includes(needle)),
        `${needle} in ${errors}`,
      );
    expect("name:");
    expect("gitlab.url");
    expect("gitlab.projectIds");
    expect("host.type");
    expect("pools[2].tag: duplicate ci");
    expect("pools[2].image");
    expect("pools[2].vcpu");
    expect("pools[2].stages: test already mapped to pool ci");
    expect("defaultPool: missing is not a pool");
    expect("cache.type");
    expect("cache.expireDays");
  });

  it("rejects empty pools and bad stage lists", () => {
    assert.ok(errorsOf({ ...valid(), pools: [] }).some((e) => e.startsWith("pools:")));
    const raw = valid();
    raw.pools[0].stages = ["", 3];
    assert.ok(errorsOf(raw).some((e) => e.includes("pools[0].stages: array")));
  });

  it("routes stages to pools with the default as fallback", () => {
    const config = validateConfig(valid());
    assert.equal(poolForStage(config, "build").tag, "build");
    assert.equal(poolForStage(config, "test").tag, "ci");
    assert.equal(poolForStage(config, "deploy").tag, "ci");
    assert.equal(poolForStage(config, undefined).tag, "ci");
  });

  it("checks pools against provider capabilities", () => {
    const config = validateConfig(valid());
    const caps = {
      name: "tiny",
      maxVcpu: 2,
      maxMemoryMiB: 4096,
      maxStorageMiB: 10240,
      maxDurationS: 600,
    };
    const errors = checkCapabilities(config, caps);
    assert.deepEqual(errors, [
      "pools[0].timeoutS: 3600 above tiny maximum 600",
      "pools[1].vcpu: 4 above tiny maximum 2",
      "pools[1].timeoutS: 3600 above tiny maximum 600",
    ]);
    assert.deepEqual(checkCapabilities(config, { ...caps, maxVcpu: 6, maxDurationS: 86400 }), []);
  });
});
