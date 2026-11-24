use std::io::ErrorKind;
use std::path::PathBuf;

use ncore::rt::{BoxFuture, boxed};
use ncore_deepaiacc::{LaneStore, MAX_LANE};
use serde::{Deserialize, Serialize};

use crate::accounts_file::write_json;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedLane {
    lane: u32,
}

pub(super) struct FileLaneStore {
    path: PathBuf,
}

impl FileLaneStore {
    pub(super) fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl LaneStore for FileLaneStore {
    fn load(&self) -> BoxFuture<'_, Result<Option<u32>, String>> {
        boxed(async {
            let raw = match std::fs::read(&self.path) {
                Ok(raw) => raw,
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
                Err(_) => return Err("Couldn't read the saved DeepAI version".into()),
            };
            let value: serde_json::Value = serde_json::from_slice(&raw)
                .map_err(|_| "Couldn't read the saved DeepAI version")?;
            if !value.is_object() {
                return Err("Couldn't read the saved DeepAI version".into());
            }
            let saved: SavedLane = serde_json::from_slice(&raw)
                .map_err(|_| "Couldn't read the saved DeepAI version")?;
            if !(1..=MAX_LANE).contains(&saved.lane) {
                return Err("Couldn't read the saved DeepAI version".into());
            }
            Ok(Some(saved.lane))
        })
    }

    fn save(&self, lane: u32) -> BoxFuture<'_, Result<(), String>> {
        boxed(async move {
            if !(1..=MAX_LANE).contains(&lane) {
                return Err("Couldn't save the DeepAI version".into());
            }
            write_json(&self.path, &SavedLane { lane })
                .map_err(|_| "Couldn't save the DeepAI version".into())
        })
    }
}
