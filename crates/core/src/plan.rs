use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;

use crate::Result;
use crate::store::write_private;
use crate::types::{Action, Outputs, Step};

pub fn format_plan(steps: &[Step]) -> String {
    if steps.is_empty() {
        return "nothing to do".to_string();
    }
    let width = steps
        .iter()
        .map(|step| step.action.as_str().len())
        .max()
        .unwrap_or(0);
    steps
        .iter()
        .map(|step| {
            let detail = step
                .detail
                .as_ref()
                .map(|d| format!("  ({d})"))
                .unwrap_or_default();
            format!("{:width$}  {}{detail}", step.action.as_str(), step.resource)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn summarize_plan(steps: &[Step]) -> String {
    let count = |action: Action| steps.iter().filter(|s| s.action == action).count();
    format!(
        "{} to create, {} to update, {} to delete, {} unchanged",
        count(Action::Create),
        count(Action::Update),
        count(Action::Delete),
        count(Action::Noop)
    )
}

pub fn parse_env(text: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let mut value = value.trim();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = &value[1..value.len() - 1];
        }
        values.insert(key.to_string(), value.to_string());
    }
    values
}

pub fn format_env(values: &BTreeMap<String, String>) -> String {
    let mut text = String::new();
    for (key, value) in values {
        text.push_str(key);
        text.push('=');
        text.push_str(value);
        text.push('\n');
    }
    text
}

pub async fn read_env_file(path: &Path) -> Result<BTreeMap<String, String>> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => Ok(parse_env(&text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error.into()),
    }
}

/// Merges values into an env file. Existing keys win unless `overwrite` is set, so a secret
/// shown once by a provider survives reruns.
pub async fn write_env_file(
    path: &Path,
    values: &BTreeMap<String, String>,
    overwrite: bool,
) -> Result<BTreeMap<String, String>> {
    let mut merged = read_env_file(path).await?;
    for (key, value) in values {
        if overwrite || !merged.contains_key(key) {
            merged.insert(key.clone(), value.clone());
        }
    }
    write_private(path, format_env(&merged).as_bytes()).await?;
    Ok(merged)
}

#[async_trait]
pub trait OutputsStore: Send + Sync {
    async fn read(&self) -> Result<Outputs>;
    async fn write(&self, values: &Outputs, overwrite: bool) -> Result<Outputs>;
    async fn clear(&self) -> Result<()>;
}

pub struct FileOutputs {
    path: PathBuf,
}

impl FileOutputs {
    pub fn new(path: impl Into<PathBuf>) -> FileOutputs {
        FileOutputs { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[async_trait]
impl OutputsStore for FileOutputs {
    async fn read(&self) -> Result<Outputs> {
        read_env_file(&self.path).await
    }

    async fn write(&self, values: &Outputs, overwrite: bool) -> Result<Outputs> {
        write_env_file(&self.path, values, overwrite).await
    }

    async fn clear(&self) -> Result<()> {
        write_private(&self.path, b"").await
    }
}

#[derive(Default)]
pub struct MemoryOutputs {
    values: Mutex<Outputs>,
}

impl MemoryOutputs {
    pub fn new() -> MemoryOutputs {
        MemoryOutputs::default()
    }
}

#[async_trait]
impl OutputsStore for MemoryOutputs {
    async fn read(&self) -> Result<Outputs> {
        Ok(self.values.lock().expect("outputs").clone())
    }

    async fn write(&self, values: &Outputs, overwrite: bool) -> Result<Outputs> {
        let mut current = self.values.lock().expect("outputs");
        for (key, value) in values {
            if overwrite || !current.contains_key(key) {
                current.insert(key.clone(), value.clone());
            }
        }
        Ok(current.clone())
    }

    async fn clear(&self) -> Result<()> {
        self.values.lock().expect("outputs").clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steps() -> Vec<Step> {
        vec![
            Step::new("a", Action::Create, "project demo"),
            Step::new("b", Action::Noop, "job definition demo-ci").with_detail("unchanged"),
            Step::new("c", Action::Delete, "job definition demo-old"),
        ]
    }

    #[test]
    fn formats_and_summarizes() {
        assert_eq!(
            format_plan(&steps()),
            "create  project demo\nnoop    job definition demo-ci  (unchanged)\ndelete  job definition demo-old"
        );
        assert_eq!(
            summarize_plan(&steps()),
            "1 to create, 0 to update, 1 to delete, 1 unchanged"
        );
        assert_eq!(format_plan(&[]), "nothing to do");
    }

    #[test]
    fn parses_env_text() {
        let parsed = parse_env("# c\n\nA=1\nB=\"two words\"\nC='x'\nBAD\n=nokey\nD=a=b\n");
        let expected: BTreeMap<String, String> =
            [("A", "1"), ("B", "two words"), ("C", "x"), ("D", "a=b")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        assert_eq!(parsed, expected);
        assert_eq!(format_env(&expected), "A=1\nB=two words\nC=x\nD=a=b\n");
    }

    #[tokio::test]
    async fn env_file_keeps_existing_keys_unless_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("outputs.env");
        let one = |k: &str, v: &str| BTreeMap::from([(k.to_string(), v.to_string())]);
        assert!(read_env_file(&path).await.unwrap().is_empty());
        write_env_file(&path, &one("SECRET", "first"), false)
            .await
            .unwrap();
        write_env_file(&path, &one("SECRET", "second"), false)
            .await
            .unwrap();
        assert_eq!(read_env_file(&path).await.unwrap()["SECRET"], "first");
        write_env_file(&path, &one("SECRET", "third"), true)
            .await
            .unwrap();
        assert_eq!(read_env_file(&path).await.unwrap()["SECRET"], "third");
        let outputs = FileOutputs::new(&path);
        outputs.clear().await.unwrap();
        assert!(outputs.read().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn memory_outputs_behave_the_same() {
        let outputs = MemoryOutputs::new();
        let values = Outputs::from([("A".to_string(), "1".to_string())]);
        outputs.write(&values, false).await.unwrap();
        let next = Outputs::from([
            ("A".to_string(), "2".to_string()),
            ("B".to_string(), "3".to_string()),
        ]);
        outputs.write(&next, false).await.unwrap();
        let read = outputs.read().await.unwrap();
        assert_eq!(read["A"], "1");
        assert_eq!(read["B"], "3");
        outputs.write(&next, true).await.unwrap();
        assert_eq!(outputs.read().await.unwrap()["A"], "2");
        outputs.clear().await.unwrap();
        assert!(outputs.read().await.unwrap().is_empty());
    }
}
