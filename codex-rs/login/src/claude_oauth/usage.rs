//! The Claude subscription's usage, read from `GET /api/oauth/usage`.
//!
//! Ported from OpenCodex `providers/quota/anthropic-usage-windows.ts` (which rows count as the
//! 5-hour and weekly windows), `providers/quota-wire.ts` (number and timestamp
//! normalisation) and `providers/anthropic-reset-grants.ts` (the reset-grant block, upstream
//! program `cedar_ember`). The percentages in this body are already 0-100 (the rate-limit
//! *headers* carry 0-1 fractions instead).
//!
//! Parsing never invents a limit: an absent window stays absent, and a malformed reset-grant
//! block reads as "no reset credit information", never as "zero resets".

use super::consts;
use crate::AuthRouteConfig;
use crate::account_pool::AccountUsage;
use crate::account_pool::ResetCredit;
use crate::account_pool::ResetGrant;
use crate::account_pool::UsageWindow;
use crate::auth::default_client::create_raw_auth_client;
use serde_json::Value;
use std::time::Duration;

const USAGE_TIMEOUT: Duration = Duration::from_secs(10);
const FIVE_HOUR_MINUTES: i64 = 300;
const WEEKLY_MINUTES: i64 = 10_080;
/// Windows a grant can clear; others upstream may add are dropped.
const CLEARABLE: [&str; 3] = ["five_hour", "seven_day", "seven_day_overage_included"];

/// Reads the account's usage with its own access token. The error text never contains the token
/// or the response body.
pub async fn fetch_usage(
    route: &AuthRouteConfig,
    access: &str,
    now: i64,
) -> Result<AccountUsage, String> {
    let base = consts::usage_url();
    let with_grants = if base.contains('?') {
        format!("{base}&{}", consts::USAGE_QUERY)
    } else {
        format!("{base}?{}", consts::USAGE_QUERY)
    };
    match get(route, &with_grants, access).await {
        Ok(body) => Ok(parse_usage(&body, now)),
        // The grant query is an extra; fall back to the plain read when the server balks at it.
        Err(Failure::Other) => get(route, &base, access)
            .await
            .map(|body| parse_usage(&body, now))
            .map_err(Failure::message),
        Err(failure) => Err(failure.message()),
    }
}

pub(super) enum Failure {
    Auth,
    RateLimited,
    Other,
}

impl Failure {
    pub(super) fn message(self) -> String {
        match self {
            Self::Auth => "the usage request was refused (sign in again)",
            Self::RateLimited => "the usage request was rate limited",
            Self::Other => "the usage request failed",
        }
        .to_string()
    }
}

pub(super) async fn get(route: &AuthRouteConfig, url: &str, access: &str) -> Result<Value, Failure> {
    let client = create_raw_auth_client(url, route).map_err(|_| Failure::Other)?;
    let request = client
        .get(url)
        .header("Authorization", format!("Bearer {access}"))
        .header("Accept", "application/json")
        .header("User-Agent", consts::ACCOUNT_USER_AGENT)
        .header("anthropic-beta", consts::OAUTH_BETA)
        .send();
    let response = tokio::time::timeout(USAGE_TIMEOUT, request)
        .await
        .map_err(|_| Failure::Other)?
        .map_err(|_| Failure::Other)?;
    let status = response.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(Failure::Auth);
    }
    if status.as_u16() == 429 {
        return Err(Failure::RateLimited);
    }
    if !status.is_success() {
        return Err(Failure::Other);
    }
    let text = response.text().await.map_err(|_| Failure::Other)?;
    serde_json::from_str(&text).map_err(|_| Failure::Other)
}

/// Reads the reset-grant block fresh (the spend gate). A malformed or missing block is a failure,
/// never "no grants".
pub(super) async fn fetch_credit(
    route: &AuthRouteConfig,
    access: &str,
) -> Result<ParsedCredit, Failure> {
    let base = consts::usage_url();
    let url = if base.contains('?') {
        format!("{base}&{}", consts::USAGE_QUERY)
    } else {
        format!("{base}?{}", consts::USAGE_QUERY)
    };
    let body = get(route, &url, access).await?;
    body.get("cedar_ember")
        .and_then(parse_reset_credit)
        .ok_or(Failure::Other)
}

/// Turns a usage body into the pool's usage view.
pub fn parse_usage(body: &Value, now: i64) -> AccountUsage {
    AccountUsage {
        five_hour: window(body, "session", "five_hour", FIVE_HOUR_MINUTES),
        weekly: window(body, "weekly_all", "seven_day", WEEKLY_MINUTES),
        updated_at: Some(now),
        reset_credit: body
            .get("cedar_ember")
            .and_then(parse_reset_credit)
            .map(|parsed| parsed.credit),
    }
}

/// One window. A `limits` row of the right kind supersedes the legacy top-level field (even a
/// malformed one); a null legacy `five_hour` only counts when a row says the session is active.
fn window(body: &Value, kind: &str, legacy: &str, minutes: i64) -> Option<UsageWindow> {
    let rows = body.get("limits").and_then(Value::as_array);
    let row = rows.and_then(|rows| {
        rows.iter()
            .find(|row| row.get("kind").and_then(Value::as_str) == Some(kind))
    });
    let (source, field) = match row {
        Some(row) => {
            let active = row.get("is_active").and_then(Value::as_bool);
            let legacy_is_null = body.get(legacy).is_some_and(Value::is_null);
            if kind == "session" && legacy_is_null && active != Some(true) {
                return None;
            }
            (row, "percent")
        }
        None => (body.get(legacy).filter(|v| v.is_object())?, "utilization"),
    };
    let percent = finite_number(source.get(field)?)?.clamp(0.0, 100.0);
    Some(UsageWindow {
        used_percent: percent,
        window_minutes: Some(minutes),
        resets_at: source.get("resets_at").and_then(reset_seconds),
    })
}

/// A number, or a string holding one.
fn finite_number(value: &Value) -> Option<f64> {
    let number = match value {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) if !s.trim().is_empty() => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    number.is_finite().then_some(number)
}

/// A reset time as unix seconds: an ISO-8601 string, or an epoch number in seconds or
/// milliseconds. Zero and negative values are sentinels, not clocks.
fn reset_seconds(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => epoch_seconds(n.as_f64()?),
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            if let Ok(number) = text.parse::<f64>() {
                return epoch_seconds(number);
            }
            let parsed = chrono::DateTime::parse_from_rfc3339(text).ok()?.timestamp();
            (parsed > 0).then_some(parsed)
        }
        _ => None,
    }
}

fn epoch_seconds(value: f64) -> Option<i64> {
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let seconds = if value > 10_000_000_000.0 { value / 1000.0 } else { value };
    Some(seconds as i64)
}

fn optional_timestamp(value: Option<&Value>) -> Result<Option<i64>, ()> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => chrono::DateTime::parse_from_rfc3339(text.trim())
            .map(|t| Some(t.timestamp()))
            .map_err(|_| ()),
        Some(_) => Err(()),
    }
}

fn optional_bool(value: Option<&Value>, fallback: bool) -> Result<bool, ()> {
    match value {
        None | Some(Value::Null) => Ok(fallback),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(()),
    }
}

fn count(value: Option<&Value>) -> Option<i64> {
    value?.as_u64().and_then(|n| i64::try_from(n).ok())
}

/// Display text from upstream: control characters stripped, whitespace collapsed, bounded.
fn safe_label(value: Option<&Value>) -> String {
    let Some(text) = value.and_then(Value::as_str) else {
        return String::new();
    };
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect()
}

fn parse_grant(value: &Value) -> Option<ResetGrant> {
    let object = value.as_object()?;
    let id = object.get("id")?.as_str()?;
    let id_ok = !id.is_empty()
        && id.len() <= 40
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !id_ok {
        return None;
    }
    let total = count(object.get("resets_total"))?;
    let left = count(object.get("resets_left"))?;
    if left > total {
        return None;
    }
    optional_timestamp(object.get("starts_at")).ok()?;
    let ends_at = optional_timestamp(object.get("ends_at")).ok()?;
    let clears = match object.get("clears") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => CLEARABLE
            .iter()
            .filter(|window| items.iter().any(|item| item.as_str() == Some(**window)))
            .map(|window| (*window).to_string())
            .collect(),
        Some(_) => return None,
    };
    let paused = optional_bool(object.get("paused"), false).ok()?;
    // A grant that does not say it is usable is not offered for spending.
    let usable_now = optional_bool(object.get("usable_now"), false).ok()?;
    optional_bool(object.get("use_requires_limit"), true).ok()?;
    Some(ResetGrant {
        id: id.to_string(),
        label: safe_label(object.get("label")),
        resets_left: left,
        resets_total: total,
        ends_at,
        paused,
        usable_now,
        clears,
    })
}

/// A parsed reset-grant block plus what a spend needs and the credit does not show.
pub(super) struct ParsedCredit {
    pub credit: ResetCredit,
    /// Per grant: the server says it must be at a limit to be used.
    requires_limit: Vec<bool>,
    /// Per grant: the server's own `usable_now`.
    upstream_usable: Vec<bool>,
    /// The grant the server suggests spending first.
    pub next_grant_id: Option<String>,
}

impl ParsedCredit {
    /// Why a grant cannot be spent right now, or `None` when it can (OpenCodex
    /// `anthropicResetGrantBlocker`).
    pub(super) fn blocker(&self, grant_id: &str) -> Option<&'static str> {
        if !self.credit.eligible {
            return Some("ineligible");
        }
        let Some(index) = self.credit.grants.iter().position(|g| g.id == grant_id) else {
            return Some("unknown_grant");
        };
        let grant = &self.credit.grants[index];
        if grant.paused {
            return Some("paused");
        }
        if !self.upstream_usable[index] {
            return Some("not_usable");
        }
        if grant.resets_left <= 0 {
            return Some("exhausted");
        }
        if self.requires_limit[index] && !self.credit.at_limit {
            return Some("not_limited");
        }
        None
    }
}

/// The `cedar_ember` block. One malformed grant, a duplicate id, or a bad timestamp rejects the
/// whole block (fail closed).
pub(super) fn parse_reset_credit(block: &Value) -> Option<ParsedCredit> {
    let object = block.as_object()?;
    let eligible = object.get("eligible")?.as_bool()?;
    let at_limit = optional_bool(object.get("at_limit"), false).ok()?;
    let cooldown_until = optional_timestamp(object.get("cooldown_until")).ok()?;
    let raw = match object.get("grants") {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(items)) => items.as_slice(),
        Some(_) => return None,
    };
    let mut grants: Vec<ResetGrant> = Vec::new();
    let mut requires_limit: Vec<bool> = Vec::new();
    let mut upstream_usable: Vec<bool> = Vec::new();
    for item in raw {
        let grant = parse_grant(item)?;
        if grants.iter().any(|g| g.id == grant.id) {
            return None;
        }
        requires_limit.push(optional_bool(item.get("use_requires_limit"), true).ok()?);
        upstream_usable.push(grant.usable_now);
        grants.push(grant);
    }
    let next_grant_id = object
        .get("next_grant_id")
        .and_then(Value::as_str)
        .filter(|id| grants.iter().any(|g| g.id == *id))
        .map(str::to_string);
    // From here on a grant's `usable_now` means "can be spent right now".
    for (index, grant) in grants.iter_mut().enumerate() {
        grant.usable_now = eligible
            && !grant.paused
            && upstream_usable[index]
            && grant.resets_left > 0
            && (!requires_limit[index] || at_limit);
    }
    let usable_now = grants.iter().any(|g| g.usable_now);
    let resets_left = grants
        .iter()
        .filter(|g| !g.paused)
        .map(|g| g.resets_left)
        .sum();
    let resets_total = grants.iter().map(|g| g.resets_total).sum();
    let ends_at = grants
        .iter()
        .filter(|g| g.resets_left > 0)
        .filter_map(|g| g.ends_at)
        .min();
    Some(ParsedCredit {
        credit: ResetCredit {
            eligible,
            at_limit,
            resets_left,
            resets_total,
            usable_now,
            ends_at,
            cooldown_until,
            grants,
        },
        requires_limit,
        upstream_usable,
        next_grant_id,
    })
}

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;
