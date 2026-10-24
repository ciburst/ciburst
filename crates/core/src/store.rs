use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::Result;

/// A JSON document slot for modules that keep local state (the fake provider and host).
#[async_trait]
pub trait Store: Send + Sync {
    async fn load(&self) -> Result<Option<Value>>;
    async fn save(&self, value: Option<Value>) -> Result<()>;
}

#[derive(Default)]
pub struct MemoryStore {
    value: Mutex<Option<Value>>,
}

impl MemoryStore {
    pub fn new() -> MemoryStore {
        MemoryStore::default()
    }
}

#[async_trait]
impl Store for MemoryStore {
    async fn load(&self) -> Result<Option<Value>> {
        Ok(self.value.lock().expect("store").clone())
    }

    async fn save(&self, value: Option<Value>) -> Result<()> {
        *self.value.lock().expect("store") = value;
        Ok(())
    }
}

/// One named slot inside a JSON file shared by several slots.
pub struct FileStore {
    path: PathBuf,
    slot: String,
}

impl FileStore {
    pub fn new(path: impl Into<PathBuf>, slot: impl Into<String>) -> FileStore {
        FileStore {
            path: path.into(),
            slot: slot.into(),
        }
    }

    async fn read_all(&self) -> Result<Map<String, Value>> {
        match tokio::fs::read_to_string(&self.path).await {
            Ok(text) => match serde_json::from_str::<Value>(&text)? {
                Value::Object(map) => Ok(map),
                _ => Ok(Map::new()),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
            Err(error) => Err(error.into()),
        }
    }
}

#[async_trait]
impl Store for FileStore {
    async fn load(&self) -> Result<Option<Value>> {
        Ok(self.read_all().await?.get(&self.slot).cloned())
    }

    async fn save(&self, value: Option<Value>) -> Result<()> {
        let mut all = self.read_all().await?;
        match value {
            Some(value) => {
                all.insert(self.slot.clone(), value);
            }
            None => {
                all.remove(&self.slot);
            }
        }
        let text = format!("{}\n", serde_json::to_string_pretty(&Value::Object(all))?);
        write_private(&self.path, text.as_bytes()).await
    }
}

/// Writes a file readable by the owner only (mode 600 where the platform supports it).
pub async fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, bytes).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_store_keeps_slots_apart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("state.json");
        let a = FileStore::new(&path, "a");
        let b = FileStore::new(&path, "b");
        assert_eq!(a.load().await.unwrap(), None);
        a.save(Some(serde_json::json!({ "x": 1 }))).await.unwrap();
        b.save(Some(serde_json::json!({ "y": 2 }))).await.unwrap();
        assert_eq!(
            FileStore::new(&path, "a").load().await.unwrap(),
            Some(serde_json::json!({ "x": 1 }))
        );
        a.save(None).await.unwrap();
        assert_eq!(a.load().await.unwrap(), None);
        assert_eq!(b.load().await.unwrap(), Some(serde_json::json!({ "y": 2 })));
    }

    #[tokio::test]
    async fn memory_store_round_trips() {
        let store = MemoryStore::new();
        store.save(Some(Value::from(3))).await.unwrap();
        assert_eq!(store.load().await.unwrap(), Some(Value::from(3)));
        store.save(None).await.unwrap();
        assert_eq!(store.load().await.unwrap(), None);
    }
}
