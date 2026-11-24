use std::fmt;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ncore::rt::{BoxFuture, MaybeSend, MaybeSync, boxed};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// One Start Plan account. Serialization contains the account secret.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub zcode_jwt: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials").finish_non_exhaustive()
    }
}

impl Credentials {
    pub fn new(zcode_jwt: impl Into<String>) -> Result<Self, String> {
        let credentials = Self {
            zcode_jwt: zcode_jwt.into(),
        };
        credentials.claims()?;
        Ok(credentials)
    }

    /// Imports host-supplied native JSON. Encrypted values require the native cipher secret.
    pub fn from_zcode_json(bytes: &[u8], cipher_secret: Option<&str>) -> Result<Self, String> {
        let value: Value =
            serde_json::from_slice(bytes).map_err(|_| "invalid ZCode credentials JSON")?;
        let token = value["zcodejwttoken"].as_str().ok_or("missing ZCode JWT")?;
        Self::new(decrypt(token, cipher_secret)?)
    }

    pub fn expires_at(&self) -> Option<u64> {
        self.claims().ok()?["exp"]
            .as_u64()
            .map(|n| n.saturating_mul(1000))
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        self.claims()?;
        if self.expires_at().is_some_and(|expiry| expiry <= unix_ms()) {
            return Err("ZCode session expired; reconnect the account".into());
        }
        Ok(())
    }

    fn claims(&self) -> Result<Value, String> {
        let parts: Vec<_> = self.zcode_jwt.split('.').collect();
        if parts.len() != 3
            || parts.iter().any(|part| {
                part.is_empty()
                    || !part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_=".contains(&b))
            })
        {
            return Err("invalid ZCode JWT".into());
        }
        let decoded = URL_SAFE_NO_PAD
            .decode(parts[1].trim_end_matches('='))
            .map_err(|_| "invalid ZCode JWT payload")?;
        let claims: Value =
            serde_json::from_slice(&decoded).map_err(|_| "invalid ZCode JWT payload")?;
        if !claims.is_object() {
            return Err("invalid ZCode JWT payload".into());
        }
        Ok(claims)
    }
}

fn decrypt(value: &str, secret: Option<&str>) -> Result<String, String> {
    let Some(encrypted) = value.strip_prefix("enc:v1:") else {
        return Ok(value.into());
    };
    let secret = secret
        .filter(|s| !s.is_empty())
        .ok_or("encrypted ZCode credentials need a cipher secret")?;
    let parts = encrypted
        .split('.')
        .map(|s| URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "invalid ZCode encrypted credential")?;
    if parts.len() != 3 || parts[0].len() != 12 || parts[1].len() != 16 {
        return Err("invalid ZCode encrypted credential".into());
    }
    let key = Sha256::digest(secret.as_bytes());
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| "invalid credential cipher key")?;
    let mut ciphertext = parts[2].clone();
    ciphertext.extend_from_slice(&parts[1]);
    let plain = cipher
        .decrypt(Nonce::from_slice(&parts[0]), ciphertext.as_ref())
        .map_err(|_| "ZCode credential decrypt failed")?;
    String::from_utf8(plain).map_err(|_| "invalid decrypted ZCode credential".into())
}

pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// The host owns durable persistence. Bind each store to one account.
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
