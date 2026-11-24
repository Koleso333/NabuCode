use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use serde_json::Value;

const DAY_MS: u64 = 86_400_000;
const DEFAULT_FREE_LIMIT: u64 = 600_000;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Billing {
    pub credit_usage_percent: Option<f64>,
    pub current_period: Option<UsagePeriod>,
    pub prepaid_balance: Option<Cents>,
    pub is_unified_billing_user: Option<bool>,
    pub on_demand_used: Option<Cents>,
    pub on_demand_cap: Option<Cents>,
    pub monthly_limit: Option<Cents>,
    pub used: Option<Cents>,
    pub billing_period_end: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsagePeriod {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Cents {
    #[serde(default)]
    pub val: i64,
}

impl Billing {
    pub fn used_percent(&self) -> Option<f64> {
        self.credit_usage_percent
            .or_else(|| {
                let limit = self.monthly_limit.as_ref()?.val.unsigned_abs();
                let used = self.used.as_ref()?.val.unsigned_abs();
                (limit > 0).then(|| used as f64 / limit as f64 * 100.0)
            })
            .map(|percent| percent.clamp(0.0, 100.0))
    }

    pub fn resets_at(&self) -> Option<&str> {
        self.current_period
            .as_ref()
            .and_then(|period| period.end.as_deref())
            .or(self.billing_period_end.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FreeQuota {
    pub used_tokens: u64,
    pub limit_tokens: u64,
    pub remaining_percent: f64,
    pub estimated: bool,
}

/// Per-account usage history. Hosts can persist this alongside account metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FreeUsage {
    entries: VecDeque<(u64, u64)>,
    reported: Option<Reported>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Reported {
    at: u64,
    used: u64,
    limit: u64,
    local: u64,
}

impl FreeUsage {
    pub(crate) fn reported_exhausted(&mut self, now: u64) -> bool {
        self.reported.as_ref().is_some_and(|report| {
            now.saturating_sub(report.at) < DAY_MS && report.used >= report.limit
        }) && self.snapshot(now).remaining_percent <= 0.0
    }

    fn prune(&mut self, now: u64) {
        self.entries
            .retain(|&(at, _)| now.saturating_sub(at) < DAY_MS);
    }

    fn local(&self) -> u64 {
        self.entries
            .iter()
            .fold(0u64, |sum, &(_, tokens)| sum.saturating_add(tokens))
    }

    pub fn snapshot(&mut self, now: u64) -> FreeQuota {
        self.prune(now);
        let local = self.local();
        let limit = self
            .reported
            .as_ref()
            .map_or(DEFAULT_FREE_LIMIT, |report| report.limit.max(1));
        let used = self
            .reported
            .as_ref()
            .filter(|report| now.saturating_sub(report.at) < DAY_MS)
            .map_or(local, |report| {
                (i128::from(local) + i128::from(report.used) - i128::from(report.local))
                    .clamp(0, i128::from(u64::MAX)) as u64
            });
        FreeQuota {
            used_tokens: used,
            limit_tokens: limit,
            remaining_percent: (1.0 - used as f64 / limit as f64).clamp(0.0, 1.0) * 100.0,
            estimated: true,
        }
    }

    pub(crate) fn record(&mut self, now: u64, tokens: u64) {
        self.prune(now);
        if tokens != 0 {
            self.entries.push_back((now, tokens));
        }
    }

    pub(crate) fn reconcile(&mut self, now: u64, error: &Value) {
        if error["code"] != "subscription:free-usage-exhausted" {
            return;
        }
        let Some((used, limit)) = error["error"]
            .as_str()
            .and_then(|message| message.split_once("tokens (actual/limit):"))
            .and_then(|(_, count)| count.split_whitespace().next())
            .and_then(|count| count.trim_end_matches('.').split_once('/'))
            .and_then(|(used, limit)| Some((used.parse::<u64>().ok()?, limit.parse::<u64>().ok()?)))
            .filter(|&(_, limit)| limit > 0)
        else {
            return;
        };
        self.prune(now);
        self.reported = Some(Reported {
            at: now,
            used,
            limit,
            local: self.local(),
        });
    }
}
