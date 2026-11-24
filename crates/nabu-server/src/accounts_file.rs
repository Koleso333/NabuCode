use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone, Serialize, Deserialize)]
pub struct Saved<C> {
    /// Stable id; the email is only known after the first profile read.
    pub key: String,
    #[serde(default)]
    pub email: Option<String>,
    pub credentials: C,
}

/// Every account of one subscription provider in one JSON file `{"accounts": [...]}`.
pub struct AccountsFile<C> {
    path: PathBuf,
    saved: Mutex<Vec<Saved<C>>>,
}

impl<C: Clone + Serialize + DeserializeOwned> AccountsFile<C> {
    /// Also takes in the single account an earlier version kept in `legacy`.
    pub fn load(path: PathBuf, legacy: Option<PathBuf>) -> Arc<Self> {
        let mut saved: Vec<Saved<C>> = std::fs::read(&path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
            .and_then(|v| serde_json::from_value(v["accounts"].clone()).ok())
            .unwrap_or_default();
        let imported = legacy
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|raw| serde_json::from_slice::<C>(&raw).ok());
        let file = Arc::new(Self {
            path,
            saved: Mutex::new(Vec::new()),
        });
        if let (Some(credentials), Some(legacy)) = (imported, legacy) {
            saved.push(Saved {
                key: new_key(),
                email: None,
                credentials,
            });
            if file.write(&saved).is_ok() {
                let _ = std::fs::remove_file(legacy);
            }
        }
        *file.saved.lock().unwrap() = saved;
        file
    }

    pub fn list(&self) -> Vec<Saved<C>> {
        self.saved.lock().unwrap().clone()
    }

    pub fn add(&self, credentials: C) -> String {
        let key = new_key();
        self.change(|saved| {
            saved.push(Saved {
                key: key.clone(),
                email: None,
                credentials,
            })
        });
        key
    }

    pub fn set_email(&self, key: &str, email: &str) {
        self.change(|saved| {
            if let Some(s) = saved.iter_mut().find(|s| s.key == key) {
                s.email = Some(email.to_owned());
            }
        });
    }

    pub fn remove(&self, key: &str) {
        if let Err(error) = self.try_remove(key) {
            eprintln!("[accounts] {error}");
        }
    }

    pub fn credentials(&self, key: &str) -> Option<C> {
        self.saved
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.key == key)
            .map(|s| s.credentials.clone())
    }

    pub fn replace(&self, key: &str, credentials: &C) {
        if let Err(error) = self.try_replace(key, credentials) {
            eprintln!("[accounts] {error}");
        }
    }

    pub fn update(&self, key: &str, edit: impl FnOnce(&mut Saved<C>)) {
        self.change(|saved| {
            if let Some(s) = saved.iter_mut().find(|s| s.key == key) {
                edit(s);
            }
        });
    }

    pub fn try_add(&self, credentials: C) -> Result<String, String> {
        let key = new_key();
        self.try_change(|saved| {
            saved.push(Saved {
                key: key.clone(),
                email: None,
                credentials,
            });
            Ok(())
        })?;
        Ok(key)
    }

    pub fn try_replace(&self, key: &str, credentials: &C) -> Result<(), String> {
        self.try_update(key, |saved| saved.credentials = credentials.clone())
    }

    pub fn try_update(&self, key: &str, edit: impl FnOnce(&mut Saved<C>)) -> Result<(), String> {
        self.try_change(|saved| {
            let saved = saved
                .iter_mut()
                .find(|saved| saved.key == key)
                .ok_or("The account was removed")?;
            edit(saved);
            Ok(())
        })
    }

    pub fn try_remove(&self, key: &str) -> Result<(), String> {
        self.try_remove_many(&[key.to_owned()])
    }

    pub fn try_remove_many(&self, keys: &[String]) -> Result<(), String> {
        self.try_change(|saved| {
            saved.retain(|saved| !keys.contains(&saved.key));
            Ok(())
        })
    }

    fn change(&self, edit: impl FnOnce(&mut Vec<Saved<C>>)) {
        if let Err(error) = self.try_change(|saved| {
            edit(saved);
            Ok(())
        }) {
            eprintln!("[accounts] {error}");
        }
    }

    fn try_change(
        &self,
        edit: impl FnOnce(&mut Vec<Saved<C>>) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut saved = self.saved.lock().unwrap();
        let mut next = saved.clone();
        edit(&mut next)?;
        self.write(&next)?;
        *saved = next;
        Ok(())
    }

    fn write(&self, saved: &[Saved<C>]) -> Result<(), String> {
        write_json(&self.path, &json!({ "accounts": saved }))
    }
}

pub(crate) fn write_json(path: &std::path::Path, value: &impl Serialize) -> Result<(), String> {
    use std::io::Write;
    let body = serde_json::to_vec_pretty(value).map_err(|_| "Couldn't save the data")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|_| "Couldn't create the data folder")?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", new_key()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&tmp)
        .map_err(|_| "Couldn't save the data")?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(&body)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    let _ = std::fs::remove_file(tmp);
    result.map_err(|_| "Couldn't save the data".into())
}

/// One account's view of the shared file, as subscription plugins expect a store per account.
pub struct AccountStore<C> {
    pub file: Arc<AccountsFile<C>>,
    pub key: String,
}

fn new_key() -> String {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("os random");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("nabu-accounts-test-{}", new_key()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn fallible_updates_are_atomic_and_survive_reload() {
        let dir = TestDir::new();
        let path = dir.0.join("accounts.json");
        let file = AccountsFile::<String>::load(path.clone(), None);
        let key = file.try_add("old".into()).unwrap();
        file.try_update(&key, |saved| {
            saved.credentials = "new".into();
            saved.email = Some("label".into());
        })
        .unwrap();
        let loaded = AccountsFile::<String>::load(path, None);
        assert_eq!(loaded.credentials(&key).as_deref(), Some("new"));
        assert_eq!(loaded.list()[0].email.as_deref(), Some("label"));
        file.try_remove(&key).unwrap();
        assert!(file.list().is_empty());
    }

    #[test]
    fn failed_write_leaves_memory_and_disk_unchanged() {
        let dir = TestDir::new();
        let path = dir.0.join("accounts.json");
        let file = AccountsFile::<String>::load(path.clone(), None);
        let key = file.try_add("old".into()).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(file.try_replace(&key, &"new".into()).is_err());
        assert!(file.try_remove(&key).is_err());
        assert!(file.try_add("extra".into()).is_err());
        assert_eq!(file.credentials(&key).as_deref(), Some("old"));
        assert_eq!(file.list().len(), 1);
        assert!(path.is_dir());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
    }

    #[test]
    fn missing_account_is_not_recreated_by_a_nonce_write() {
        let dir = TestDir::new();
        let file = AccountsFile::<String>::load(dir.0.join("accounts.json"), None);
        assert!(file.try_replace("removed", &"nonce".into()).is_err());
        assert!(file.list().is_empty());
    }
}
