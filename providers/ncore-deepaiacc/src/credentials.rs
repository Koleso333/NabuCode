use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ncore::rt::{BoxFuture, MaybeSend, MaybeSync, boxed};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Serialization contains secrets. The host must save and reuse each account's identity.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(try_from = "StoredCredentials")]
pub struct Credentials {
    pub api_key: String,
    pub machine_uuid: String,
    pub instance: String,
    pub lane: u32,
    pub check_in: Option<CheckIn>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("check_in", &self.check_in)
            .finish_non_exhaustive()
    }
}

impl Credentials {
    /// Creates a fresh synthetic identity on every call, including for the same key.
    pub fn new(api_key: impl Into<String>) -> Result<Self, String> {
        let api_key = api_key.into();
        validate_key(&api_key)?;
        let mut uuid = [0u8; 16];
        let mut instance = [0u8; 16];
        getrandom::fill(&mut uuid).map_err(|_| "could not create DeepAI account identity")?;
        getrandom::fill(&mut instance).map_err(|_| "could not create DeepAI account identity")?;
        uuid[6] = (uuid[6] & 0x0f) | 0x40;
        uuid[8] = (uuid[8] & 0x3f) | 0x80;
        let uuid = hex(&uuid);
        let credentials = Self {
            api_key,
            machine_uuid: format!(
                "{}-{}-{}-{}-{}",
                &uuid[..8],
                &uuid[8..12],
                &uuid[12..16],
                &uuid[16..20],
                &uuid[20..]
            ),
            instance: hex(&instance),
            lane: crate::lane::DEFAULT_LANE,
            check_in: None,
        };
        credentials.validate()?;
        Ok(credentials)
    }

    /// Imports host-supplied native JSON and creates a fresh identity; save and reuse the result.
    pub fn from_deepai_json(bytes: &[u8]) -> Result<Self, String> {
        let value: Value =
            serde_json::from_slice(bytes).map_err(|_| "invalid DeepAI credentials JSON")?;
        let api_key = value
            .get("apiKey")
            .and_then(Value::as_str)
            .ok_or("missing DeepAI account key")?;
        Self::new(api_key)
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_key(&self.api_key)?;
        if !is_v4_uuid(&self.machine_uuid)
            || !is_lower_hex(&self.instance, 32)
            || !(1..=crate::lane::MAX_LANE).contains(&self.lane)
        {
            return Err("invalid DeepAI account identity".into());
        }
        if let Some(check_in) = &self.check_in {
            check_in.validate()?;
        }
        Ok(())
    }

    pub fn account_id(&self) -> String {
        hex(&Sha256::digest(self.api_key.as_bytes()))
    }

    pub fn machine_digest(&self) -> String {
        hex(&Sha256::digest(
            self.machine_uuid.to_ascii_lowercase().as_bytes(),
        ))
    }
}

#[derive(Deserialize)]
struct StoredCredentials {
    #[serde(alias = "apiKey")]
    api_key: String,
    #[serde(alias = "machineUuid")]
    machine_uuid: String,
    instance: String,
    #[serde(default, deserialize_with = "stored_lane")]
    lane: Option<u32>,
    #[serde(alias = "checkIn")]
    check_in: Option<CheckIn>,
}

fn stored_lane<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<u32>, D::Error> {
    u32::deserialize(deserializer).map(Some)
}

impl TryFrom<StoredCredentials> for Credentials {
    type Error = String;

    fn try_from(value: StoredCredentials) -> Result<Self, Self::Error> {
        let unknown_lane = value.lane.is_none();
        let mut credentials = Self {
            api_key: value.api_key,
            machine_uuid: value.machine_uuid,
            instance: value.instance,
            lane: value.lane.unwrap_or(crate::lane::DEFAULT_LANE),
            check_in: value.check_in,
        };
        credentials.validate()?;
        if unknown_lane {
            credentials.check_in = None;
        }
        Ok(credentials)
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(try_from = "StoredCheckIn")]
pub struct CheckIn {
    pub nonce: String,
    pub seconds: f64,
    pub checked_at: u64,
}

impl fmt::Debug for CheckIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CheckIn")
            .field("seconds", &self.seconds)
            .field("checked_at", &self.checked_at)
            .finish_non_exhaustive()
    }
}

impl CheckIn {
    pub fn validate(&self) -> Result<(), String> {
        if !is_lower_hex(&self.nonce, 32)
            || !self.seconds.is_finite()
            || self.seconds <= 0.0
            || Duration::try_from_secs_f64(self.seconds).is_err()
            || self.checked_at == 0
        {
            return Err("invalid DeepAI check-in state".into());
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct StoredCheckIn {
    nonce: String,
    seconds: f64,
    #[serde(alias = "checkedAt")]
    checked_at: u64,
}

impl TryFrom<StoredCheckIn> for CheckIn {
    type Error = String;

    fn try_from(value: StoredCheckIn) -> Result<Self, Self::Error> {
        let check_in = Self {
            nonce: value.nonce,
            seconds: value.seconds,
            checked_at: value.checked_at,
        };
        check_in.validate()?;
        Ok(check_in)
    }
}

fn validate_key(value: &str) -> Result<(), String> {
    if value.is_empty() || !value.bytes().all(|byte| (33..=126).contains(&byte)) {
        return Err("invalid DeepAI account key".into());
    }
    Ok(())
}

fn is_v4_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
        && bytes[14] == b'4'
        && matches!(bytes[19], b'8' | b'9' | b'a' | b'b' | b'A' | b'B')
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
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
        boxed(async {
            let credentials = self
                .0
                .lock()
                .map_err(|_| "DeepAI credential store poisoned")?;
            credentials.validate()?;
            Ok(credentials.clone())
        })
    }

    fn save<'a>(&'a self, credentials: &'a Credentials) -> BoxFuture<'a, Result<(), String>> {
        boxed(async move {
            credentials.validate()?;
            *self
                .0
                .lock()
                .map_err(|_| "DeepAI credential store poisoned")? = credentials.clone();
            Ok(())
        })
    }
}
