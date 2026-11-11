//! The history of an account's banked usage-limit resets.
//!
//! The servers do not keep one: Anthropic's usage endpoint shows only the grants as they are now,
//! and ChatGPT's list shows credits with a status but no timeline. So the pool records what it
//! sees, as it sees it: every usage read is compared with the one before, and a change that means
//! something becomes a line in `reset-history.jsonl` next to the account's metadata. The file is
//! append-only; nothing in this module rewrites or removes a line. It holds no token and no
//! organisation id.
//!
//! The observation rules follow OpenCodex `quota/reset-detector.ts` (a window that fell is a reset
//! only with a deadline that passed or a drop that is not a rolling window's decay) and
//! `quota/reset-seen-store.ts` (every event has an idempotence key, so seeing it twice records it
//! once).

use super::types::AccountUsage;
use super::types::ResetCredit;
use super::types::ResetGrant;
use super::types::ResetStatusKind;
use super::types::UsageWindow;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

/// Below this many points a fall in usage is rounding or a correction, not a reset.
const MIN_SURPRISE_DROP_PERCENT: f64 = 5.0;
/// A spend this much older than the observation that saw the credit fall is not "ours" any more.
const OWN_SPEND_SLACK_SECS: i64 = 120;

static APPEND_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HistoryKind {
    /// A grant was seen for the first time.
    Granted,
    /// A spend made from this app (success, refusal, or an outcome that is not known).
    Used,
    /// A grant went down or away and this app's records do not account for it.
    UsedElsewhere,
    Expired,
    /// A usage window fell by a lot: the limit was reset.
    QuotaReset,
    /// The credit could not be read.
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HistorySource {
    /// Seen by comparing two reads.
    Observed,
    /// This app's own spend records.
    Journal,
    /// The server's own listing said so (an ended or used credit it still lists).
    Endpoint,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// Unix seconds: when it happened if known, otherwise when it was noticed.
    pub ts: i64,
    pub kind: HistoryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub detail: String,
    pub source: HistorySource,
    /// Idempotence key: the same event seen again is not recorded again. Not sent to clients.
    #[serde(default)]
    pub key: String,
}

fn entry(
    ts: i64,
    kind: HistoryKind,
    source: HistorySource,
    grant: Option<&ResetGrant>,
    detail: String,
    key: String,
) -> HistoryEntry {
    HistoryEntry {
        ts,
        kind,
        grant_id: grant.map(|grant| grant.id.clone()),
        label: grant
            .map(|grant| grant.label.clone())
            .filter(|label| !label.is_empty()),
        detail,
        source,
        key,
    }
}

/// Every line of the history file. A line that does not parse is skipped, never rewritten.
pub fn read(path: &Path) -> Vec<HistoryEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Appends the entries whose key is not in the file yet.
pub fn append(path: &Path, entries: &[HistoryEntry]) {
    if entries.is_empty() {
        return;
    }
    let _guard = APPEND_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut seen: HashSet<String> = read(path)
        .into_iter()
        .map(|existing| existing.key)
        .filter(|key| !key.is_empty())
        .collect();
    let mut text = String::new();
    for entry in entries {
        if !entry.key.is_empty() && !seen.insert(entry.key.clone()) {
            continue;
        }
        if let Ok(line) = serde_json::to_string(entry) {
            text.push_str(&line);
            text.push('\n');
        }
    }
    if text.is_empty() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(text.as_bytes());
    }
}

/// What the difference between two reads means, recorded into `path`.
pub fn observe(path: &Path, previous: Option<&AccountUsage>, next: &AccountUsage, now: i64) {
    let mut found = Vec::new();
    found.extend(credit_changes(path, previous, next, now));
    found.extend(window_resets(previous, next, now));
    found.extend(read_errors(previous, next, now));
    append(path, &found);
}

fn read_errors(previous: Option<&AccountUsage>, next: &AccountUsage, now: i64) -> Vec<HistoryEntry> {
    let Some(status) = next
        .reset_status
        .as_ref()
        .filter(|status| status.kind == ResetStatusKind::Error)
    else {
        return Vec::new();
    };
    let before = previous
        .and_then(|usage| usage.reset_status.as_ref())
        .filter(|status| status.kind == ResetStatusKind::Error);
    if before.is_some_and(|before| before.reason == status.reason) {
        return Vec::new(); // the same failure, still going on
    }
    let reason = status.reason.as_deref().unwrap_or("unknown");
    vec![entry(
        now,
        HistoryKind::Error,
        HistorySource::Observed,
        None,
        format!("券の情報を取得できませんでした({reason})"),
        format!("error|{reason}|{now}"),
    )]
}

fn credit_changes(
    path: &Path,
    previous: Option<&AccountUsage>,
    next: &AccountUsage,
    now: i64,
) -> Vec<HistoryEntry> {
    // A failed or missing read says nothing about the grants.
    let Some(after) = next.reset_credit.as_ref().filter(|_| {
        next.reset_status
            .as_ref()
            .is_none_or(|status| status.kind == ResetStatusKind::Ok)
    }) else {
        return Vec::new();
    };
    let before = previous.and_then(|usage| usage.reset_credit.as_ref());
    let seen_at = previous.and_then(|usage| usage.updated_at);
    let spent_here = |grant_id: &str| spent_here_since(path, grant_id, seen_at, now);
    let mut out = Vec::new();

    for grant in &after.grants {
        let old = before.and_then(|credit| credit.grants.iter().find(|g| g.id == grant.id));
        let ended = grant.ends_at.is_some_and(|end| end <= now);
        match old {
            None if grant.resets_left > 0 => {
                let first = if before.is_none() {
                    "初めての取得で券が見えました(付与の時刻は不明)"
                } else {
                    "券の付与を観測しました"
                };
                out.push(entry(
                    now,
                    HistoryKind::Granted,
                    HistorySource::Observed,
                    Some(grant),
                    format!("{first}: 残り {}/{} 回", grant.resets_left, grant.resets_total),
                    format!("granted|{}", grant.id),
                ));
            }
            // Listed already used or ended the first time it is seen: the server said so.
            None if ended => out.push(entry(
                grant.ends_at.unwrap_or(now),
                HistoryKind::Expired,
                HistorySource::Endpoint,
                Some(grant),
                "一覧に、期限切れとして載っていました".to_string(),
                format!("expired|{}", grant.id),
            )),
            None => out.push(entry(
                now,
                HistoryKind::UsedElsewhere,
                HistorySource::Endpoint,
                Some(grant),
                "一覧に、使用済みとして載っていました(使った時刻は不明)".to_string(),
                format!("usedElsewhere|{}|listed", grant.id),
            )),
            Some(old) if grant.resets_left < old.resets_left => {
                if ended {
                    out.push(entry(
                        grant.ends_at.unwrap_or(now),
                        HistoryKind::Expired,
                        HistorySource::Observed,
                        Some(grant),
                        "期限が切れました".to_string(),
                        format!("expired|{}", grant.id),
                    ));
                } else if !spent_here(&grant.id) {
                    out.push(entry(
                        now,
                        HistoryKind::UsedElsewhere,
                        HistorySource::Observed,
                        Some(grant),
                        format!(
                            "券が減りました(残り {} → {} 回)。このアプリの記録にはありません",
                            old.resets_left, grant.resets_left
                        ),
                        format!("usedElsewhere|{}|{}", grant.id, grant.resets_left),
                    ));
                }
            }
            Some(_) if ended && grant.resets_left > 0 => out.push(entry(
                grant.ends_at.unwrap_or(now),
                HistoryKind::Expired,
                HistorySource::Observed,
                Some(grant),
                "期限が切れました".to_string(),
                format!("expired|{}", grant.id),
            )),
            Some(_) => {}
        }
    }

    // A grant that was there and is gone.
    if let Some(before) = before {
        for old in &before.grants {
            if after.grants.iter().any(|grant| grant.id == old.id) || old.resets_left <= 0 {
                continue;
            }
            if old.ends_at.is_some_and(|end| end <= now) {
                out.push(entry(
                    old.ends_at.unwrap_or(now),
                    HistoryKind::Expired,
                    HistorySource::Observed,
                    Some(old),
                    "期限が切れ、一覧から消えました".to_string(),
                    format!("expired|{}", old.id),
                ));
            } else if !spent_here(&old.id) {
                out.push(entry(
                    now,
                    HistoryKind::UsedElsewhere,
                    HistorySource::Observed,
                    Some(old),
                    "券が一覧から消えました。このアプリの記録にはありません".to_string(),
                    format!("usedElsewhere|{}|gone", old.id),
                ));
            }
        }
    }
    out
}

/// Whether this app recorded a spend of the grant since the previous read (so a fall in the grant
/// is its own doing, already in the history as a `used` entry).
fn spent_here_since(path: &Path, grant_id: &str, seen_at: Option<i64>, now: i64) -> bool {
    let since = seen_at.unwrap_or(now) - OWN_SPEND_SLACK_SECS;
    read(path).iter().any(|existing| {
        existing.kind == HistoryKind::Used
            && existing.ts >= since
            && existing
                .grant_id
                .as_deref()
                .is_none_or(|recorded| recorded == grant_id)
    })
}

fn window_resets(previous: Option<&AccountUsage>, next: &AccountUsage, now: i64) -> Vec<HistoryEntry> {
    let Some(previous) = previous else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (name, label, before, after) in [
        ("fiveHour", "5 時間", &previous.five_hour, &next.five_hour),
        ("weekly", "週", &previous.weekly, &next.weekly),
    ] {
        let (Some(before), Some(after)) = (before, after) else {
            continue;
        };
        if let Some(scheduled) = detect_reset(before, after, previous.updated_at, now) {
            let how = if scheduled {
                "期限どおりに切り替わりました"
            } else {
                "期限の前にリセットされました"
            };
            out.push(entry(
                now,
                HistoryKind::QuotaReset,
                HistorySource::Observed,
                None,
                format!(
                    "{label}の枠: 使用率 {:.0}% → {:.0}%({how})",
                    before.used_percent, after.used_percent
                ),
                format!(
                    "quotaReset|{name}|{}",
                    after.resets_at.or(before.resets_at).unwrap_or(0)
                ),
            ));
        }
    }
    out
}

/// `Some(true)` for a window that turned over at its deadline, `Some(false)` for one reset out of
/// band, `None` when nothing indicates a reset (OpenCodex `detectQuotaReset`).
fn detect_reset(
    before: &UsageWindow,
    after: &UsageWindow,
    previous_observed_at: Option<i64>,
    now: i64,
) -> Option<bool> {
    let deadline_passed = before.resets_at.is_some_and(|reset| now >= reset);
    if deadline_passed {
        if after.used_percent > before.used_percent {
            return None;
        }
        // An expired deadline alone proves nothing: a partial write can carry the old deadline.
        let fell = after.used_percent < before.used_percent;
        let advanced = matches!((after.resets_at, before.resets_at), (Some(new), Some(old)) if new > old);
        return (fell || advanced).then_some(true);
    }
    let previous_reset = before.resets_at?;
    if before.used_percent - after.used_percent < MIN_SURPRISE_DROP_PERCENT {
        return None;
    }
    // A rolling window's deadline creeps forward with the clock while nothing resets.
    if let (Some(new), Some(observed)) = (after.resets_at, previous_observed_at) {
        let elapsed = now - observed;
        let shift = new - previous_reset;
        if elapsed > 0 && shift > 0 && shift <= elapsed * 2 {
            return None;
        }
    }
    Some(false)
}

#[cfg(test)]
#[path = "reset_history_tests.rs"]
mod tests;
