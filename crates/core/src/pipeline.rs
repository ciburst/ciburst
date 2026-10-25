//! The jobs of a pipeline as `ciburst check` reads them from GitLab's merged YAML (D46): image,
//! services and script lines after `extends` and `default:`, and the top-level variables.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::{Error, Result};

/// Levels of `extends` GitLab resolves, the job itself included.
pub const MAX_EXTENDS_LEVELS: usize = 11;

/// Levels of variables inside variable values that `expand` follows.
const MAX_EXPANSION_LEVELS: usize = 10;

/// Top-level keys that name no job; the last five are the global forms of `default:` keys.
const KEYWORDS: [&str; 11] = [
    "stages",
    "variables",
    "default",
    "include",
    "workflow",
    "spec",
    "image",
    "services",
    "cache",
    "before_script",
    "after_script",
];

const SCRIPTS: [&str; 3] = ["before_script", "script", "after_script"];

/// What a lane needs to know of a job.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobSpec {
    /// `image:` or its `name:`, variables not expanded.
    pub image: Option<String>,
    /// Names of the `services:`.
    pub services: Vec<String>,
    /// Lines of `before_script`, `script` and `after_script`.
    pub script: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pipeline {
    pub jobs: BTreeMap<String, JobSpec>,
    /// The top-level `variables:`.
    pub variables: BTreeMap<String, String>,
}

impl Pipeline {
    /// The pipeline of a merged YAML document read as JSON. Hidden jobs (`.name`) only serve
    /// as bases of `extends`; `default:` (or the global key) gives `image` and `services` to
    /// jobs without them, as far as their `inherit: default` allows.
    pub fn from_value(root: &Value) -> Result<Pipeline> {
        let Some(root) = root.as_object() else {
            return Ok(Pipeline::default());
        };
        let default = root.get("default").and_then(Value::as_object);
        let default_of = |key: &str| default.and_then(|d| d.get(key)).or_else(|| root.get(key));
        let mut jobs = BTreeMap::new();
        for (name, value) in root {
            if name.starts_with('.') || KEYWORDS.contains(&name.as_str()) || !value.is_object() {
                continue;
            }
            let job = extended(root, name, 0, &mut vec![name.clone()])?;
            let inherited = |key: &str| match job.get(key) {
                Some(value) => Some(value),
                None if inherits_default(&job, key) => default_of(key),
                None => None,
            };
            let mut script = Vec::new();
            for key in SCRIPTS {
                if let Some(value) = job.get(key) {
                    script_lines(value, &mut script);
                }
            }
            jobs.insert(
                name.clone(),
                JobSpec {
                    image: inherited("image").and_then(image_name),
                    services: inherited("services").map(service_names).unwrap_or_default(),
                    script,
                },
            );
        }
        let variables = root
            .get("variables")
            .and_then(Value::as_object)
            .map(|vars| {
                vars.iter()
                    .map(|(key, value)| (key.clone(), variable_value(value)))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Pipeline { jobs, variables })
    }

    /// The job `name`, also under the names GitLab gives the copies of a `parallel` job:
    /// `<name> 1/3` and `<name>: [a, b]`.
    pub fn job(&self, name: &str) -> Option<&JobSpec> {
        if let Some(job) = self.jobs.get(name) {
            return Some(job);
        }
        self.jobs.get(self.parallel_of(name)?)
    }

    /// The job whose copy `name` is: `unit` of `unit 1/3` and of `unit: [a, b]`, when the
    /// pipeline has that job and no job of the copy's own name.
    pub fn parallel_of<'a>(&self, name: &'a str) -> Option<&'a str> {
        if self.jobs.contains_key(name) {
            return None;
        }
        let base = match name.rsplit_once(' ') {
            Some((base, index)) if is_index(index) => base,
            _ => name.strip_suffix(']')?.rsplit_once(": [")?.0,
        };
        self.jobs.contains_key(base).then_some(base)
    }

    /// `text` with `$NAME` and `${NAME}` replaced by top-level variables, whose values expand
    /// in turn; `$$` is a `$`. `Err` names the first variable the pipeline lacks.
    pub fn expand(&self, text: &str) -> std::result::Result<String, String> {
        self.expand_within(text, 0)
    }

    fn expand_within(&self, text: &str, depth: usize) -> std::result::Result<String, String> {
        let mut out = String::new();
        let mut rest = text;
        while let Some(at) = rest.find('$') {
            out.push_str(&rest[..at]);
            let after = &rest[at + 1..];
            if let Some(tail) = after.strip_prefix('$') {
                out.push('$');
                rest = tail;
                continue;
            }
            let reference = match after.strip_prefix('{') {
                Some(braced) => braced
                    .split_once('}')
                    .filter(|(name, _)| is_variable_name(name)),
                None => {
                    let end = after
                        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                        .unwrap_or(after.len());
                    (end > 0).then(|| after.split_at(end))
                }
            };
            let Some((name, tail)) = reference else {
                out.push('$');
                rest = after;
                continue;
            };
            let value = self
                .variables
                .get(name)
                .filter(|_| depth < MAX_EXPANSION_LEVELS)
                .ok_or_else(|| name.to_string())?;
            out.push_str(&self.expand_within(value, depth + 1)?);
            rest = tail;
        }
        out.push_str(rest);
        Ok(out)
    }
}

/// The pipeline of GitLab's merged YAML (CI lint with `include_merged_yaml`).
#[cfg(feature = "pipeline")]
pub fn parse_merged_yaml(text: &str) -> Result<Pipeline> {
    let documents = yaml_rust2::YamlLoader::load_from_str(text)
        .map_err(|error| Error::other(format!("merged YAML: {error}")))?;
    Pipeline::from_value(&documents.first().map(yaml_to_json).unwrap_or(Value::Null))
}

#[cfg(feature = "pipeline")]
fn yaml_to_json(yaml: &yaml_rust2::Yaml) -> Value {
    use yaml_rust2::Yaml;
    match yaml {
        Yaml::String(text) | Yaml::Real(text) => Value::String(text.clone()),
        Yaml::Integer(number) => Value::from(*number),
        Yaml::Boolean(flag) => Value::Bool(*flag),
        Yaml::Array(items) => Value::Array(items.iter().map(yaml_to_json).collect()),
        Yaml::Hash(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let key = match yaml_to_json(key) {
                        Value::String(text) => text,
                        Value::Null => String::new(),
                        other => other.to_string(),
                    };
                    // Ruby writes symbol keys as `:name`.
                    let key = key.strip_prefix(':').map(String::from).unwrap_or(key);
                    (key, yaml_to_json(value))
                })
                .collect(),
        ),
        Yaml::Null | Yaml::Alias(_) | Yaml::BadValue => Value::Null,
    }
}

/// The job `name` with its `extends` resolved as GitLab does: the bases in order, each later
/// one and then the job itself winning; mappings merge deeply, other values are replaced.
fn extended(
    root: &Map<String, Value>,
    name: &str,
    depth: usize,
    path: &mut Vec<String>,
) -> Result<Map<String, Value>> {
    let job_name = path[0].clone();
    let fail = |message: String| Error::other(format!("job {job_name}: {message}"));
    let job = root
        .get(name)
        .and_then(Value::as_object)
        .ok_or_else(|| fail(format!("extends {name}, which is no job or hidden job")))?;
    let bases: Vec<&str> = match job.get("extends") {
        None | Some(Value::Null) => return Ok(job.clone()),
        Some(Value::String(base)) => vec![base.as_str()],
        Some(Value::Array(list)) => list
            .iter()
            .map(|base| {
                base.as_str()
                    .ok_or_else(|| fail("extends lists a non-string".to_string()))
            })
            .collect::<Result<_>>()?,
        Some(other) => return Err(fail(format!("extends is {other}"))),
    };
    if depth + 1 >= MAX_EXTENDS_LEVELS {
        return Err(fail(format!(
            "extends nests deeper than {MAX_EXTENDS_LEVELS} levels"
        )));
    }
    let mut merged = Map::new();
    for base in bases {
        if path.iter().any(|seen| seen == base) {
            return Err(fail(format!("extends {base} in a circle")));
        }
        path.push(base.to_string());
        let resolved = extended(root, base, depth + 1, path)?;
        path.pop();
        deep_merge(&mut merged, resolved);
    }
    deep_merge(&mut merged, job.clone());
    Ok(merged)
}

fn deep_merge(base: &mut Map<String, Value>, over: Map<String, Value>) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(Value::Object(inner)), Value::Object(value)) => deep_merge(inner, value),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// `inherit: default:` true (absent), false, or the list of keys the job takes.
fn inherits_default(job: &Map<String, Value>, key: &str) -> bool {
    match job
        .get("inherit")
        .and_then(|inherit| inherit.get("default"))
    {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Array(keys)) => keys.iter().any(|k| k.as_str() == Some(key)),
        _ => true,
    }
}

fn image_name(value: &Value) -> Option<String> {
    match value {
        Value::String(name) => Some(name.clone()),
        Value::Object(image) => image.get("name")?.as_str().map(String::from),
        _ => None,
    }
}

fn service_names(value: &Value) -> Vec<String> {
    let one = |service: &Value| image_name(service);
    match value {
        Value::Array(services) => services.iter().filter_map(one).collect(),
        other => one(other).into_iter().collect(),
    }
}

/// Script entries are strings or nested lists of them; a string may hold several lines.
fn script_lines(value: &Value, lines: &mut Vec<String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                script_lines(item, lines);
            }
        }
        Value::String(text) => lines.extend(text.lines().map(String::from)),
        Value::Null => {}
        other => lines.push(other.to_string()),
    }
}

/// A variable's value: a scalar, or a mapping with `value`.
fn variable_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(map) => map.get("value").map(variable_value).unwrap_or_default(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn is_variable_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `1/3` of a `parallel` copy's name.
fn is_index(text: &str) -> bool {
    text.split_once('/').is_some_and(|(index, total)| {
        [index, total]
            .iter()
            .all(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extends_merges_mappings_deeply_and_replaces_lists() {
        let pipeline = Pipeline::from_value(&json!({
            ".base": {
                "image": "node:24",
                "variables": { "A": "1", "B": "1" },
                "script": ["base"],
                "before_script": ["npm ci"]
            },
            ".other": { "variables": { "B": "2", "C": "2" }, "script": ["other"] },
            "two": { "extends": [".base", ".other"], "variables": { "C": "3" } },
            "one": { "extends": ".base", "script": ["own"] },
            "plain": { "script": "single line\nsecond line" }
        }))
        .unwrap();
        let two = &pipeline.jobs["two"];
        assert_eq!(two.image.as_deref(), Some("node:24"));
        assert_eq!(
            two.script,
            ["npm ci", "other"],
            "later base wins, lists replaced"
        );
        assert_eq!(pipeline.jobs["one"].script, ["npm ci", "own"]);
        assert_eq!(
            pipeline.jobs["plain"].script,
            ["single line", "second line"]
        );
        assert!(
            !pipeline.jobs.contains_key(".base"),
            "hidden jobs are no jobs"
        );
    }

    #[test]
    fn extends_resolves_eleven_levels_and_no_more() {
        let chain = |levels: usize| {
            let mut root = Map::new();
            root.insert(
                ".l0".to_string(),
                json!({ "image": "alpine:3.21", "script": ["x"] }),
            );
            for level in 1..levels - 1 {
                root.insert(
                    format!(".l{level}"),
                    json!({ "extends": format!(".l{}", level - 1) }),
                );
            }
            root.insert(
                "job".to_string(),
                json!({ "extends": format!(".l{}", levels - 2) }),
            );
            Pipeline::from_value(&Value::Object(root))
        };
        let eleven = chain(11).unwrap();
        assert_eq!(eleven.jobs["job"].image.as_deref(), Some("alpine:3.21"));
        let error = chain(12).unwrap_err().to_string();
        assert!(
            error.contains("job job: extends nests deeper than 11 levels"),
            "{error}"
        );
    }

    #[test]
    fn extends_errors_name_the_job() {
        for (root, needle) in [
            (
                json!({ "a": { "extends": ".gone" } }),
                "job a: extends .gone, which is no job",
            ),
            (
                json!({ "a": { "extends": ".b" }, ".b": { "extends": ".c" }, ".c": { "extends": ".b" } }),
                "job a: extends .b in a circle",
            ),
            (
                json!({ "a": { "extends": "a" } }),
                "job a: extends a in a circle",
            ),
            (
                json!({ "a": { "extends": [1] } }),
                "job a: extends lists a non-string",
            ),
            (
                json!({ "a": { "extends": ".s" }, ".s": "text" }),
                "job a: extends .s, which is no job",
            ),
        ] {
            let error = Pipeline::from_value(&root).unwrap_err().to_string();
            assert!(error.contains(needle), "{error}");
        }
    }

    #[test]
    fn default_gives_image_and_services_to_jobs_without_them() {
        let pipeline = Pipeline::from_value(&json!({
            "default": { "image": { "name": "node:24", "entrypoint": [""] }, "services": ["postgres:17"] },
            "stages": ["test"],
            "variables": { "X": "1" },
            "workflow": { "rules": [] },
            "include": [],
            "spec": { "inputs": {} },
            "plain": { "script": ["a"] },
            "own": { "image": "rust:1.96.0", "services": [], "script": ["b"] },
            "none": { "inherit": { "default": false }, "script": ["c"] },
            "some": { "inherit": { "default": ["services"] }, "script": ["d"] },
            "listed": { "services": [{ "name": "redis:8", "alias": "cache" }, "mysql"], "script": ["e"] }
        }))
        .unwrap();
        let names: Vec<&str> = pipeline.jobs.keys().map(String::as_str).collect();
        assert_eq!(names, ["listed", "none", "own", "plain", "some"]);
        let job = |name: &str| &pipeline.jobs[name];
        assert_eq!(job("plain").image.as_deref(), Some("node:24"));
        assert_eq!(job("plain").services, ["postgres:17"]);
        assert_eq!(job("own").image.as_deref(), Some("rust:1.96.0"));
        assert!(job("own").services.is_empty());
        assert_eq!(job("none").image, None);
        assert!(job("none").services.is_empty());
        assert_eq!(job("some").image, None);
        assert_eq!(job("some").services, ["postgres:17"]);
        assert_eq!(job("listed").services, ["redis:8", "mysql"]);
    }

    #[test]
    fn global_image_and_services_act_as_default() {
        let pipeline = Pipeline::from_value(&json!({
            "image": "node:24",
            "services": ["docker:dind"],
            "cache": { "paths": ["node_modules"] },
            "before_script": ["npm ci"],
            "unit": { "script": ["npm test"] }
        }))
        .unwrap();
        assert_eq!(pipeline.jobs.len(), 1);
        assert_eq!(pipeline.jobs["unit"].image.as_deref(), Some("node:24"));
        assert_eq!(pipeline.jobs["unit"].services, ["docker:dind"]);
    }

    #[test]
    fn variables_expand_from_the_top_level() {
        let pipeline = Pipeline::from_value(&json!({
            "variables": {
                "NODE": "24",
                "IMAGE": "node:${NODE}-bookworm",
                "DESCRIBED": { "value": "rust", "description": "toolchain" },
                "NUMBER": 3.10,
                "LOOP": "$LOOP"
            },
            "job": { "variables": { "LOCAL": "x" }, "script": ["a"] }
        }))
        .unwrap();
        let expand = |text: &str| pipeline.expand(text);
        assert_eq!(expand("node:$NODE").unwrap(), "node:24");
        assert_eq!(expand("$IMAGE-slim").unwrap(), "node:24-bookworm-slim");
        assert_eq!(expand("${DESCRIBED}:slim").unwrap(), "rust:slim");
        assert_eq!(expand("python:$NUMBER").unwrap(), "python:3.1");
        assert_eq!(
            expand("a$$NODE b$ c${}d ${not-a-name}").unwrap(),
            "a$NODE b$ c${}d ${not-a-name}"
        );
        assert_eq!(
            expand("$LOCAL"),
            Err("LOCAL".to_string()),
            "job variables do not count"
        );
        assert_eq!(
            expand("$CI_REGISTRY_IMAGE/x"),
            Err("CI_REGISTRY_IMAGE".to_string())
        );
        assert_eq!(expand("${NODE}${MISSING}"), Err("MISSING".to_string()));
        assert_eq!(expand("$LOOP"), Err("LOOP".to_string()));
    }

    #[test]
    fn parallel_copies_find_their_job() {
        let pipeline = Pipeline::from_value(&json!({
            "unit": { "image": "node:24", "script": ["a"] },
            "unit 1/3": { "image": "own", "script": ["b"] }
        }))
        .unwrap();
        let image = |name: &str| pipeline.job(name).and_then(|j| j.image.as_deref());
        assert_eq!(image("unit 2/3"), Some("node:24"));
        assert_eq!(image("unit 1/3"), Some("own"));
        assert_eq!(image("unit: [node, 24]"), Some("node:24"));
        assert_eq!(image("unit x/3"), None);
        assert_eq!(image("unit 2/"), None);
        assert_eq!(image("other"), None);
        assert_eq!(pipeline.parallel_of("unit 2/3"), Some("unit"));
        assert_eq!(pipeline.parallel_of("unit: [android, 13]"), Some("unit"));
        assert_eq!(pipeline.parallel_of("unit 1/3"), None, "a job of its own");
        assert_eq!(pipeline.parallel_of("unit"), None);
        assert_eq!(pipeline.parallel_of("other 1/2"), None);
    }

    #[test]
    fn reads_gitlab_merged_yaml() {
        let text = r#"---
variables:
  RUST: 1.96.0
  PY: 3.10
".rust": &1
  image: rust:${RUST}-bookworm
  script:
  - cargo test
clippy:
  extends: ".rust"
  script:
  - cargo clippy
  - |
    docker build .
    echo done
copy: *1
:symbol_job:
  :image: alpine:3.21
1:
  image: node:24
"#;
        let pipeline = parse_merged_yaml(text).unwrap();
        let clippy = &pipeline.jobs["clippy"];
        assert_eq!(clippy.image.as_deref(), Some("rust:${RUST}-bookworm"));
        assert_eq!(
            clippy.script,
            ["cargo clippy", "docker build .", "echo done"]
        );
        assert_eq!(pipeline.expand("${RUST}").unwrap(), "1.96.0");
        assert_eq!(pipeline.variables["PY"], "3.10", "numbers keep their text");
        assert_eq!(
            pipeline.jobs["copy"].script,
            ["cargo test"],
            "aliases resolve"
        );
        assert_eq!(
            pipeline.jobs["symbol_job"].image.as_deref(),
            Some("alpine:3.21")
        );
        assert_eq!(pipeline.jobs["1"].image.as_deref(), Some("node:24"));
        assert_eq!(parse_merged_yaml("").unwrap(), Pipeline::default());
        assert!(parse_merged_yaml("a: [").is_err());
    }
}
