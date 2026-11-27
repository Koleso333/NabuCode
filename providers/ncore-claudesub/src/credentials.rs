use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ncore::rt::{BoxFuture, MaybeSend, MaybeSync, boxed};
use serde::{Deserialize, Serialize};

/// One account's credentials. Expiry is Unix milliseconds, as in Claude Code's file.
/// Serialization contains secrets: storage and access control belong to the host.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Credentials {
    #[serde(alias = "access_token")]
    pub access_token: String,
    #[serde(default, alias = "refresh_token")]
    pub refresh_token: Option<String>,
    #[serde(alias = "expires_at")]
    pub expires_at: u64,
    #[serde(default)]
    pub subscription_type: Option<String>,
    #[serde(default)]
    pub rate_limit_tier: Option<String>,
    /// Anthropic account id; seeds the account's fake Claude Code machine.
    #[serde(default)]
    pub account_uuid: Option<String>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("access_token", &"[redacted]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[redacted]"),
            )
            .field("expires_at", &self.expires_at)
            .field("subscription_type", &self.subscription_type)
            .field("rate_limit_tier", &self.rate_limit_tier)
            .field("account_uuid", &self.account_uuid)
            .finish()
    }
}

impl Credentials {
    /// Imports either Claude Code's `claudeAiOauth` object or its whole credentials file.
    pub fn from_claude_json(bytes: &[u8]) -> Result<Self, String> {
        let mut v: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|_| "invalid credentials JSON".to_owned())?;
        if let Some(oauth) = v.get_mut("claudeAiOauth") {
            v = oauth.take();
        }
        serde_json::from_value(v).map_err(|_| "invalid Claude OAuth credentials".to_owned())
    }

    pub(crate) fn valid(&self) -> bool {
        !self.access_token.trim().is_empty() && self.expires_at > unix_ms().saturating_add(60_000)
    }
}

pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// A store is bound to one account. Implement durable, atomic replacement in the host.
/// `save` must complete before refreshed credentials are released to waiting requests.
/// Share one ClaudeSub instance per account to serialize refreshes. Other processes using
/// the same refresh token need external coordination covering the entire refresh transaction,
/// not merely separate load/save locks. Errors must not contain secrets.
pub trait CredentialStore: MaybeSend + MaybeSync {
    fn load(&self) -> BoxFuture<'_, Result<Credentials, String>>;
    fn save<'a>(&'a self, credentials: &'a Credentials) -> BoxFuture<'a, Result<(), String>>;
}

/// Non-durable store, useful for embedding and tests. Use a durable store for rotating tokens.
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
