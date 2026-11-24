use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::credentials::CheckIn;

/// Exact nonnegative decimal text. Numeric helpers are for approximate display only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Amount(#[serde(deserialize_with = "deserialize_amount")] String);

impl Amount {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        let mut parts = value.split('.');
        let integer = parts.next().unwrap_or_default();
        let fraction = parts.next();
        if integer.is_empty()
            || !integer.bytes().all(|byte| byte.is_ascii_digit())
            || fraction.is_some_and(|part| {
                part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit())
            })
            || parts.next().is_some()
            || !value.parse::<f64>().is_ok_and(f64::is_finite)
        {
            return Err("invalid DeepAI amount".into());
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_f64(&self) -> Option<f64> {
        self.0.parse::<f64>().ok().filter(|value| value.is_finite())
    }

    pub(crate) fn from_value(value: &Value) -> Result<Self, String> {
        match value {
            Value::String(text) => Self::new(text.clone()),
            Value::Number(number) => {
                let numeric = number.as_f64().ok_or("invalid DeepAI amount")?;
                if !numeric.is_finite() || numeric < 0.0 {
                    return Err("invalid DeepAI amount".into());
                }
                let text = number.to_string();
                Self::new(expand_decimal(text.trim_start_matches('-'))?)
            }
            _ => Err("invalid DeepAI amount".into()),
        }
    }
}

fn deserialize_amount<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    Amount::from_value(&value)
        .map(|amount| amount.0)
        .map_err(serde::de::Error::custom)
}

fn expand_decimal(value: &str) -> Result<String, String> {
    let Some((coefficient, exponent)) = value.split_once(['e', 'E']) else {
        return Ok(value.into());
    };
    let exponent: i32 = exponent.parse().map_err(|_| "invalid DeepAI amount")?;
    let integer_length = coefficient.find('.').unwrap_or(coefficient.len());
    let digits = coefficient.replace('.', "");
    let point = integer_length as i64 + exponent as i64;
    if point <= 0 {
        Ok(format!("0.{}{}", "0".repeat((-point) as usize), digits))
    } else if point as usize >= digits.len() {
        Ok(format!(
            "{}{}",
            digits,
            "0".repeat(point as usize - digits.len())
        ))
    } else {
        let point = point as usize;
        Ok(format!("{}.{}", &digits[..point], &digits[point..]))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Allowance {
    pub enabled: bool,
    pub balance: Option<Amount>,
    pub granted: Option<Amount>,
    pub refresh_at: Option<String>,
    pub lane_version: Option<u32>,
}

pub(crate) fn parse_allowance(payload: &Value) -> Result<Allowance, String> {
    let object = payload.as_object().ok_or("invalid DeepAI allowance")?;
    let enabled = object
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or("invalid DeepAI allowance")?;
    let amount = |name| match object.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => Amount::from_value(value)
            .map(Some)
            .map_err(|_| "invalid DeepAI allowance".to_string()),
    };
    let balance = amount("balance")?;
    let granted = amount("granted")?;
    if enabled && (balance.is_none() || granted.is_none()) {
        return Err("invalid DeepAI allowance".into());
    }
    let refresh_at = match object.get("refresh_at") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_str().ok_or("invalid DeepAI allowance")?.into()),
    };
    let lane_version = match object.get("lane_version") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let version = value.as_u64().or_else(|| {
                let number = value.as_f64()?;
                (number.is_finite()
                    && number >= 0.0
                    && number <= u32::MAX as f64
                    && number.fract() == 0.0)
                    .then_some(number as u64)
            });
            Some(
                version
                    .and_then(|version| u32::try_from(version).ok())
                    .ok_or("invalid DeepAI allowance")?,
            )
        }
    };
    Ok(Allowance {
        enabled,
        balance,
        granted,
        refresh_at,
        lane_version,
    })
}

pub(crate) fn parse_check_in(payload: &Value, now_ms: u64) -> Result<Option<CheckIn>, String> {
    let object = payload.as_object().ok_or("invalid DeepAI check-in state")?;
    let value = match object.get("check_in") {
        None | Some(Value::Null) => return Ok(None),
        Some(value) => value,
    };
    let check_in = CheckIn {
        nonce: value
            .get("nonce")
            .and_then(Value::as_str)
            .ok_or("invalid DeepAI check-in state")?
            .into(),
        seconds: value
            .get("seconds")
            .and_then(Value::as_f64)
            .ok_or("invalid DeepAI check-in state")?,
        checked_at: now_ms,
    };
    check_in.validate()?;
    Ok(Some(check_in))
}
