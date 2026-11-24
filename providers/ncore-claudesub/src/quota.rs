use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime};

use ncore::rt::header;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::api::CallError;

fn percentage<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    let n = Option::<f64>::deserialize(d)?;
    Ok(n.filter(|n| n.is_finite() && *n >= 0.0))
}

fn limits<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Limit>, D::Error> {
    Ok(Option::<Vec<Limit>>::deserialize(d)?.unwrap_or_default())
}

/// OAuth usage numbers are percentages used, not fractions or token/message counts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Window {
    #[serde(default, deserialize_with = "percentage")]
    pub utilization: Option<f64>,
    pub resets_at: Option<String>,
    pub locked_reason: Option<String>,
}

impl Window {
    pub fn remaining_percent(&self) -> Option<f64> {
        self.utilization.map(|v| (100.0 - v).clamp(0.0, 100.0))
    }
}

/// Extended/scoped windows are preserved without hardcoding model names.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Limit {
    pub kind: Option<String>,
    pub group: Option<String>,
    #[serde(default, deserialize_with = "percentage")]
    pub percent: Option<f64>,
    pub severity: Option<String>,
    pub resets_at: Option<String>,
    pub scope: Option<Value>,
    pub is_active: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtraUsage {
    pub is_enabled: Option<bool>,
    pub disabled_reason: Option<String>,
    pub monthly_limit: Option<f64>,
    pub used_credits: Option<f64>,
    pub currency: Option<String>,
    pub decimal_places: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    #[serde(default, deserialize_with = "limits")]
    pub limits: Vec<Limit>,
    pub extra_usage: Option<ExtraUsage>,
    /// Includes nullable seven_day_* windows and future fields. Unknown is not zero.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Account {
    pub has_claude_pro: Option<bool>,
    pub has_claude_max: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Organization {
    pub organization_type: Option<String>,
    pub billing_type: Option<String>,
    pub rate_limit_tier: Option<String>,
    pub seat_tier: Option<String>,
    /// e.g. canceled can coexist with paid access; do not infer token invalidity from it.
    pub subscription_status: Option<String>,
    pub subscription_created_at: Option<String>,
    pub has_extra_usage_enabled: Option<bool>,
}

/// Selected plan information only; account names, email and UUIDs are not retained.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub account: Account,
    #[serde(default)]
    pub organization: Organization,
}

impl Profile {
    pub fn plan(&self) -> Option<&str> {
        if self.account.has_claude_max == Some(true) {
            Some("max")
        } else if self.account.has_claude_pro == Some(true) {
            Some("pro")
        } else {
            self.organization.organization_type.as_deref()
        }
    }
}

/// A successful monitor result, optionally stale after a failed refresh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot<T> {
    pub value: T,
    pub fetched_at: SystemTime,
    pub stale: bool,
    pub refresh_error: Option<String>,
}

pub(crate) struct Cache<T> {
    value: Option<Snapshot<T>>,
    expires: Option<Instant>,
    retry_at: Option<Instant>,
    error: Option<String>,
}

impl<T> Default for Cache<T> {
    fn default() -> Self {
        Self {
            value: None,
            expires: None,
            retry_at: None,
            error: None,
        }
    }
}

impl<T: Clone> Cache<T> {
    pub fn current(&self) -> Option<Result<Snapshot<T>, ncore::Error>> {
        let now = Instant::now();
        if self.expires.is_some_and(|t| t > now) {
            return self.value.clone().map(Ok);
        }
        if self.retry_at.is_some_and(|t| t > now) {
            return Some(self.stale());
        }
        None
    }

    fn stale(&self) -> Result<Snapshot<T>, ncore::Error> {
        match &self.value {
            Some(value) => {
                let mut value = value.clone();
                value.stale = true;
                value.refresh_error = self.error.clone();
                Ok(value)
            }
            None => Err(crate::api::fail(
                self.error
                    .as_deref()
                    .unwrap_or("monitor temporarily unavailable"),
            )),
        }
    }

    pub fn update(
        &mut self,
        result: Result<T, CallError>,
        ttl: Duration,
    ) -> Result<Snapshot<T>, ncore::Error> {
        match result {
            Ok(value) => {
                let snapshot = Snapshot {
                    value,
                    fetched_at: SystemTime::now(),
                    stale: false,
                    refresh_error: None,
                };
                self.value = Some(snapshot.clone());
                self.expires = Instant::now().checked_add(ttl);
                self.retry_at = None;
                self.error = None;
                Ok(snapshot)
            }
            Err(e) => {
                self.expires = None;
                self.retry_at = Instant::now().checked_add(e.retry_after);
                self.error = Some(crate::api::message(&e.error));
                self.stale()
            }
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HeaderWindow {
    /// Normalized percentage. Header fractions (0.13) become percentages (13).
    pub used_percent: Option<f64>,
    pub resets_at: Option<u64>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeaderLimits {
    pub five_hour: HeaderWindow,
    pub seven_day: HeaderWindow,
    pub status: Option<String>,
    pub overage_status: Option<String>,
    pub overage_disabled_reason: Option<String>,
    pub observed_at: SystemTime,
}

impl HeaderLimits {
    pub(crate) fn parse(headers: &[(String, String)], previous: Option<Self>) -> Option<Self> {
        if !headers.iter().any(|(k, _)| {
            k.to_ascii_lowercase()
                .starts_with("anthropic-ratelimit-unified-")
        }) {
            return previous;
        }
        let mut out = previous.unwrap_or(Self {
            five_hour: HeaderWindow::default(),
            seven_day: HeaderWindow::default(),
            status: None,
            overage_status: None,
            overage_disabled_reason: None,
            observed_at: SystemTime::now(),
        });
        for (prefix, window) in [("5h", &mut out.five_hour), ("7d", &mut out.seven_day)] {
            let get = |suffix| {
                header(
                    headers,
                    &format!("anthropic-ratelimit-unified-{prefix}-{suffix}"),
                )
            };
            if let Some(n) = get("utilization")
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|n| n.is_finite() && *n >= 0.0 && *n <= f64::MAX / 100.0)
            {
                window.used_percent = Some(n * 100.0);
            }
            if let Some(n) = get("reset").and_then(|s| s.parse().ok()) {
                window.resets_at = Some(n);
            }
            if let Some(s) = get("status") {
                window.status = Some(s.into());
            }
        }
        for (suffix, target) in [
            ("status", &mut out.status),
            ("overage-status", &mut out.overage_status),
            ("overage-disabled-reason", &mut out.overage_disabled_reason),
        ] {
            if let Some(s) = header(headers, &format!("anthropic-ratelimit-unified-{suffix}")) {
                *target = Some(s.into());
            }
        }
        out.observed_at = SystemTime::now();
        Some(out)
    }
}
