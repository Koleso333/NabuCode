use std::fmt;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ncore::rt::{BoxFuture, MaybeSend, MaybeSync, boxed};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One account; expiry is Unix milliseconds. Serialization contains secrets.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: u64,
    pub account_id: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl Credentials {
    /// Imports bytes supplied by the host; never reads Codex's files itself.
    pub fn from_codex_json(bytes: &[u8]) -> Result<Self, String> {
        let value: Value =
            serde_json::from_slice(bytes).map_err(|_| "invalid Codex credentials JSON")?;
        let tokens = value.get("tokens").unwrap_or(&value);
        let access = tokens["access_token"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or("missing Codex OAuth access token")?;
        let access_claims = claims(access);
        let id_claims = tokens["id_token"].as_str().and_then(claims);
        let account = tokens["account_id"]
            .as_str()
            .or_else(|| account_id(access_claims.as_ref()))
            .or_else(|| account_id(id_claims.as_ref()))
            .filter(|s| !s.trim().is_empty())
            .ok_or("missing ChatGPT account id")?;
        let expiry = access_claims
            .as_ref()
            .and_then(|v| v["exp"].as_u64())
            .map(|n| n.saturating_mul(1000))
            .ok_or("missing access token expiry")?;
        let credentials = Self {
            access_token: access.into(),
            refresh_token: tokens["refresh_token"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned),
            expires_at: expiry,
            account_id: account.into(),
        };
        credentials.validate()?;
        Ok(credentials)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if !header_value(&self.access_token) || !header_value(&self.account_id) {
            return Err("invalid ChatGPT access token or account id".into());
        }
        Ok(())
    }

    pub(crate) fn valid(&self) -> bool {
        self.expires_at > unix_ms().saturating_add(60_000)
    }
}

fn header_value(value: &str) -> bool {
    !value.trim().is_empty() && value.bytes().all(|b| (33..=126).contains(&b))
}

// Claims are metadata from host-supplied credentials, not proof of authentication.
pub(crate) fn claims(token: &str) -> Option<Value> {
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(parts[1].trim_end_matches('='))
            .ok()?,
    )
    .ok()
}

pub(crate) fn account_id(value: Option<&Value>) -> Option<&str> {
    value?["https://api.openai.com/auth"]["chatgpt_account_id"].as_str()
}

pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// Bind to one account. Save atomically; coordinate refresh across processes externally.
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
