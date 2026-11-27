//! One server per data folder, shared by every NabuCode window on this computer.
//! Windows find it through `server.json` and keep it alive by holding a `/hold` socket.

use std::fs::File;
use std::path::Path;
use std::time::Duration;

use tokio::sync::watch;

/// The exit code when another server already owns the data folder.
pub const BUSY: i32 = 3;
/// How long the server waits for a window after its start or after the last window let go.
const GRACE: Duration = Duration::from_secs(5);
/// Lines starting with this mark where one run of the server begins in `server.log`.
pub const START_MARK: &str = "--- nabu started";

/// The lock lives as long as the returned file; `None` when another server holds it.
pub fn lock(data: &Path) -> std::io::Result<Option<File>> {
    std::fs::create_dir_all(data)?;
    let file = File::options().create(true).truncate(false).write(true).open(data.join("server.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

pub fn announce(data: &Path, port: u16, token: &str) -> std::io::Result<()> {
    let info = serde_json::json!({ "port": port, "token": token, "protocol": nabu_protocol::PROTOCOL_VERSION });
    let part = data.join("server.json.part");
    std::fs::write(&part, info.to_string())?;
    std::fs::rename(&part, data.join("server.json"))
}

/// Counts the windows holding the server.
#[derive(Clone)]
pub struct Holds(watch::Sender<usize>);

pub struct Hold(watch::Sender<usize>);

impl Drop for Hold {
    fn drop(&mut self) {
        self.0.send_modify(|count| *count -= 1);
    }
}

impl Holds {
    pub fn new() -> Self {
        Self(watch::Sender::new(0))
    }

    pub fn take(&self) -> Hold {
        self.0.send_modify(|count| *count += 1);
        Hold(self.0.clone())
    }

    /// Returns once nobody held the server for the whole grace period.
    pub async fn wait_unheld(&self) {
        let mut count = self.0.subscribe();
        loop {
            let _ = count.wait_for(|count| *count == 0).await;
            let held = tokio::time::timeout(GRACE, count.wait_for(|count| *count > 0)).await;
            if held.is_err() {
                return;
            }
        }
    }
}

pub async fn exit_when_unheld(holds: Holds, data: std::path::PathBuf) {
    holds.wait_unheld().await;
    let _ = std::fs::remove_file(data.join("server.json"));
    nabu_server::chrome_host::withdraw();
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn waits_the_grace_after_the_last_hold() {
        let holds = Holds::new();
        let hold = holds.take();
        let waiting = tokio::spawn({
            let holds = holds.clone();
            async move { holds.wait_unheld().await }
        });
        tokio::time::sleep(GRACE * 3).await;
        assert!(!waiting.is_finished());
        drop(hold);
        tokio::time::sleep(GRACE / 2).await;
        let again = holds.take();
        tokio::time::sleep(GRACE * 3).await;
        assert!(!waiting.is_finished(), "a window came back within the grace");
        drop(again);
        tokio::time::sleep(GRACE + Duration::from_millis(10)).await;
        assert!(waiting.is_finished());
    }

    #[test]
    fn second_lock_on_the_same_folder_is_busy() {
        let data = std::env::temp_dir().join(format!("nabu-lock-{}", std::process::id()));
        let first = lock(&data).unwrap();
        assert!(first.is_some());
        assert!(lock(&data).unwrap().is_none());
        drop(first);
        assert!(lock(&data).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&data);
    }
}
