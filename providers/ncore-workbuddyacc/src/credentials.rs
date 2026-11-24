use std::fmt;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use ncore::rt::{BoxFuture, MaybeSend, MaybeSync, boxed};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: u64,
    pub refresh_expires_at: Option<u64>,
    pub uid: String,
    pub domain: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl Credentials {
    /// The host supplies native JSON and, for sym-v1 fields, the derived 32-byte key.
    pub fn from_workbuddy_json(bytes: &[u8], key: Option<&[u8; 32]>) -> Result<Self, String> {
        let v: Value =
            serde_json::from_slice(bytes).map_err(|_| "invalid WorkBuddy session JSON")?;
        let auth = &v["auth"];
        let credentials = Self {
            access_token: open_field(&auth["accessToken"], key)?,
            refresh_token: if auth["refreshToken"].is_null() {
                None
            } else {
                Some(open_field(&auth["refreshToken"], key)?)
            },
            expires_at: auth["expiresAt"].as_u64().ok_or("missing access expiry")?,
            refresh_expires_at: auth["refreshExpiresAt"].as_u64(),
            uid: v["account"]["uid"]
                .as_str()
                .ok_or("missing account uid")?
                .into(),
            domain: auth["domain"]
                .as_str()
                .ok_or("missing account domain")?
                .into(),
        };
        credentials.validate()?;
        Ok(credentials)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.domain != "www.workbuddy.ai" {
            return Err("WorkBuddyAcc requires an international workbuddy.ai account".into());
        }
        for value in std::iter::once(&self.access_token)
            .chain(self.refresh_token.iter())
            .chain(std::iter::once(&self.uid))
        {
            if value.is_empty() || !value.bytes().all(|b| (33..=126).contains(&b)) {
                return Err("invalid WorkBuddy credentials".into());
            }
        }
        Ok(())
    }

    pub(crate) fn valid(&self) -> bool {
        self.expires_at > unix_ms().saturating_add(60_000)
    }
}

fn prefixed(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value.as_bytes());
}

fn open_field(value: &Value, key: Option<&[u8; 32]>) -> Result<String, String> {
    if let Some(text) = value.as_str() {
        return Ok(text.into());
    }
    if value["$wbEncrypted"] != 1 || value.get("scheme").is_some() {
        return Err(
            "unsupported WorkBuddy credential field; supply decoded credentials from the host"
                .into(),
        );
    }
    let key = key.ok_or("encrypted credentials require a host-supplied field key")?;
    let raw = STANDARD
        .decode(value["envelope"].as_str().ok_or("missing envelope")?)
        .map_err(|_| "invalid envelope base64")?;
    let envelope: Value = serde_json::from_slice(&raw).map_err(|_| "invalid envelope JSON")?;
    let id = Sha256::digest(key)
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    if envelope["keyId"] != id || envelope["suite"] != 1 {
        return Err("credential key mismatch or unsupported suite".into());
    }
    let decode = |name: &str| {
        STANDARD
            .decode(envelope[name].as_str().unwrap_or(""))
            .map_err(|_| "invalid encrypted field")
    };
    let nonce = decode("nonce")?;
    let tag = decode("authTag")?;
    if nonce.len() != 12 || tag.len() != 16 {
        return Err("invalid encrypted field lengths".into());
    }
    let mut ciphertext = decode("ciphertext")?;
    ciphertext.extend_from_slice(&tag);
    let mut aad = b"WB-AAD\0\x01".to_vec();
    prefixed(&mut aad, "WBEV1");
    prefixed(&mut aad, "sym-v1");
    aad.extend_from_slice(&1u32.to_be_bytes());
    prefixed(&mut aad, &id);
    aad.extend_from_slice(&[2, 0, 0]);
    let plain = Aes256Gcm::new_from_slice(key)
        .map_err(|_| "invalid key")?
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| "credential authentication failed")?;
    String::from_utf8(plain).map_err(|_| "invalid credential encoding".into())
}

pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

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
