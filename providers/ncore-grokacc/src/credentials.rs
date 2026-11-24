use std::fmt;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use ncore::rt::{BoxFuture, MaybeSend, MaybeSync, boxed};
use serde::{Deserialize, Serialize};

pub(crate) const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const AUTH_SCOPE: &str = "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828";

/// OAuth session for one Grok account. Expiry is in Unix milliseconds.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: u64,
    pub user_id: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl Credentials {
    /// Imports the official OAuth entry from host-supplied Grok auth.json bytes.
    pub fn from_grok_json(bytes: &[u8]) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct Entry {
            key: String,
            refresh_token: Option<String>,
            expires_at: String,
            user_id: String,
        }

        let mut scopes: std::collections::HashMap<String, serde_json::Value> =
            serde_json::from_slice(bytes).map_err(|_| "invalid Grok auth.json")?;
        let entry: Entry = serde_json::from_value(
            scopes
                .remove(AUTH_SCOPE)
                .ok_or("Grok OAuth session is missing")?,
        )
        .map_err(|_| "invalid Grok OAuth session")?;
        let expires_at = chrono::DateTime::parse_from_rfc3339(&entry.expires_at)
            .ok()
            .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
            .ok_or("invalid Grok OAuth expiry")?;
        let credentials = Self {
            access_token: entry.key,
            refresh_token: entry.refresh_token,
            expires_at,
            user_id: entry.user_id,
        };
        credentials.validate()?;
        Ok(credentials)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if !header_token(&self.access_token) || !header_token(&self.user_id) {
            return Err("invalid Grok access token or user id".into());
        }
        Ok(())
    }
}

pub(crate) fn header_token(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| (33..=126).contains(&b))
}

pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// Use one store per account. The host coordinates durable saves across processes.
pub trait CredentialStore: MaybeSend + MaybeSync {
    fn load(&self) -> BoxFuture<'_, Result<Credentials, String>>;
    fn save<'a>(&'a self, credentials: &'a Credentials) -> BoxFuture<'a, Result<(), String>>;
}

#[derive(Debug)]
pub struct MemoryStore(Mutex<Credentials>);

impl MemoryStore {
    pub fn new(credentials: Credentials) -> Self {
        Self(Mutex::new(credentials))
    }
}

impl CredentialStore for MemoryStore {
    fn load(&self) -> BoxFuture<'_, Result<Credentials, String>> {
        boxed(async { Ok(self.0.lock().unwrap().clone()) })
    }

    fn save<'a>(&'a self, credentials: &'a Credentials) -> BoxFuture<'a, Result<(), String>> {
        boxed(async move {
            *self.0.lock().unwrap() = credentials.clone();
            Ok(())
        })
    }
}
