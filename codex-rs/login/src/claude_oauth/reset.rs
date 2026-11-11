//! Spending a banked usage-limit reset on a Claude subscription.
//!
//! Ported from OpenCodex `providers/anthropic-reset-grants.ts` (the claim),
//! `providers/anthropic-reset-grant-ledger.ts` (the journal) and
//! `server/management/anthropic-reset-grant-routes.ts` (the order of the steps):
//!
//! - read:   `GET  /api/oauth/usage?cedar_ember=1&skip_spend=1`  (the spend gate, on a fresh read)
//! - org:    `GET  /api/oauth/profile`                           (`organization.uuid`)
//! - claim:  `POST /api/organizations/{org}/reset_rate_limits`
//!           body `{program:"cedar_ember", grant_id, request_id}`
//!
//! One confirmation is one operation. Its id is also the upstream `request_id`, so retrying the
//! same operation is the same claim to Anthropic. The journal keeps a claim that got no readable
//! answer *open*: a second claim for the same grant is refused for ten minutes, and only an
//! explicit retry with the same operation id may resend it. Nothing here logs, and no upstream
//! body, token or organization id leaves through an error.

use super::consts;
use super::usage::Failure;
use super::usage::fetch_credit;
use crate::AuthRouteConfig;
use crate::auth::default_client::create_raw_auth_client;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

const CLAIM_TIMEOUT: Duration = Duration::from_secs(25);
const PROGRAM: &str = "cedar_ember";
/// A second attempt inside the lease is refused (the first may still be in flight).
const LEASE_MS: i64 = 90_000;
/// The claim's request id is reused for this long; afterwards the attempt is expired.
const RETRY_WINDOW_MS: i64 = 600_000;
const RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const MAX_OPERATIONS: usize = 256;
const CLEARABLE: [&str; 3] = ["five_hour", "seven_day", "seven_day_overage_included"];
const RESULTS: [&str; 6] = [
    "reset",
    "already_used",
    "not_limited",
    "cooldown",
    "ineligible",
    "unavailable",
];

static LEDGER_LOCK: Mutex<()> = Mutex::new(());

/// Why a spend did not produce an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConsumeFailure {
    /// The token was refused: sign in again.
    AuthFailed,
    /// The grant status could not be read (network, status, malformed block).
    UpstreamUnavailable,
    /// The grant cannot be spent right now; the reason is a fixed word
    /// (`ineligible`, `unknown_grant`, `paused`, `not_usable`, `exhausted`, `not_limited`,
    /// `no_grant`).
    NotUsable(&'static str),
    /// The grant id or operation id is malformed.
    InvalidInput,
    /// This operation is still being processed.
    InFlight,
    /// An earlier claim for this grant has no answer yet; retry that operation instead.
    UnresolvedPrior { operation_id: String },
    /// The operation can no longer be retried; read the account again.
    Expired,
    /// The operation id belongs to another account or grant.
    IdentityMismatch,
    /// The claim may have run, but no answer could be read. Retry with the same operation id.
    UnknownOutcome { operation_id: String },
    /// The answer arrived but could not be recorded; its outcome is unknown here.
    JournalWriteFailed { operation_id: String },
    /// The journal is unreadable or full; nothing was spent.
    LedgerUnavailable,
}

/// A terminal answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settled {
    /// `reset`, `already_used`, `not_limited`, `cooldown`, `ineligible`, `unavailable`, or one of
    /// the two refusals that prove nothing was spent: `rate_limited`, `auth_error`.
    pub code: String,
    /// The journal already held this operation's answer.
    pub replayed: bool,
    pub resets_left: Option<i64>,
    pub cleared: Vec<String>,
    pub grant_id: String,
    pub operation_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
    account_id: String,
    grant_id: String,
    org_digest: String,
    settled: bool,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    resets_left: Option<i64>,
    #[serde(default)]
    cleared: Vec<String>,
    attempts: u32,
    lease_until: i64,
    created_at: i64,
    updated_at: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct Journal {
    #[serde(default)]
    operations: BTreeMap<String, Record>,
}

/// An operation with no answer yet that can still be retried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingOperation {
    pub operation_id: String,
    pub grant_id: String,
    pub retryable_until_ms: i64,
}

enum Begin {
    Execute,
    Replay(Record),
}

struct Identity<'a> {
    operation_id: &'a str,
    account_id: &'a str,
    grant_id: &'a str,
    org_digest: &'a str,
}

fn read_journal(path: &Path) -> Result<Journal, ConsumeFailure> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|_| ConsumeFailure::LedgerUnavailable),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Journal::default()),
        Err(_) => Err(ConsumeFailure::LedgerUnavailable),
    }
}

fn write_journal(path: &Path, journal: &mut Journal, now: i64) -> Result<(), ()> {
    journal
        .operations
        .retain(|_, record| !record.settled || now - record.updated_at < RETENTION_MS);
    let text = serde_json::to_string(journal).map_err(|_| ())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|_| ())?;
    }
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, text).map_err(|_| ())?;
    std::fs::rename(&temp, path).map_err(|_| ())
}

fn with_lock<R>(body: impl FnOnce() -> R) -> R {
    let _guard = LEDGER_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    body()
}

fn begin(path: &Path, identity: &Identity<'_>, now: i64) -> Result<Begin, ConsumeFailure> {
    with_lock(|| {
        let mut journal = read_journal(path)?;
        if let Some(existing) = journal.operations.get_mut(identity.operation_id) {
            if existing.account_id != identity.account_id
                || existing.grant_id != identity.grant_id
                || existing.org_digest != identity.org_digest
            {
                return Err(ConsumeFailure::IdentityMismatch);
            }
            if existing.settled {
                return Ok(Begin::Replay(existing.clone()));
            }
            if existing.lease_until > now {
                return Err(ConsumeFailure::InFlight);
            }
            if now - existing.created_at >= RETRY_WINDOW_MS {
                return Err(ConsumeFailure::Expired);
            }
            existing.attempts += 1;
            existing.lease_until = now + LEASE_MS;
            existing.updated_at = now;
            write_journal(path, &mut journal, now).map_err(|()| ConsumeFailure::LedgerUnavailable)?;
            return Ok(Begin::Execute);
        }
        for (operation_id, record) in &journal.operations {
            if !record.settled
                && record.account_id == identity.account_id
                && record.grant_id == identity.grant_id
                && record.org_digest == identity.org_digest
                && now - record.created_at < RETRY_WINDOW_MS
            {
                return Err(ConsumeFailure::UnresolvedPrior {
                    operation_id: operation_id.clone(),
                });
            }
        }
        if journal.operations.len() >= MAX_OPERATIONS {
            return Err(ConsumeFailure::LedgerUnavailable);
        }
        journal.operations.insert(
            identity.operation_id.to_string(),
            Record {
                account_id: identity.account_id.to_string(),
                grant_id: identity.grant_id.to_string(),
                org_digest: identity.org_digest.to_string(),
                settled: false,
                code: None,
                resets_left: None,
                cleared: Vec::new(),
                attempts: 1,
                lease_until: now + LEASE_MS,
                created_at: now,
                updated_at: now,
            },
        );
        write_journal(path, &mut journal, now).map_err(|()| ConsumeFailure::LedgerUnavailable)?;
        Ok(Begin::Execute)
    })
}

/// Records the terminal answer. The first settlement wins and is what is returned.
fn settle(
    path: &Path,
    operation_id: &str,
    code: &str,
    resets_left: Option<i64>,
    cleared: &[String],
    now: i64,
) -> Result<Record, ()> {
    with_lock(|| {
        let mut journal = read_journal(path).map_err(|_| ())?;
        let record = journal.operations.get_mut(operation_id).ok_or(())?;
        if record.settled {
            return Ok(record.clone());
        }
        record.settled = true;
        record.code = Some(code.to_string());
        record.resets_left = resets_left;
        record.cleared = cleared.to_vec();
        record.lease_until = 0;
        record.updated_at = now;
        let settled = record.clone();
        write_journal(path, &mut journal, now)?;
        Ok(settled)
    })
}

/// Ends this process's attempt on an unknown outcome so an explicit same-id retry does not wait
/// out the whole lease. The record stays open.
fn release(path: &Path, operation_id: &str, now: i64) {
    with_lock(|| {
        let Ok(mut journal) = read_journal(path) else { return };
        if let Some(record) = journal.operations.get_mut(operation_id)
            && !record.settled
        {
            record.lease_until = now;
            record.updated_at = now;
            let _ = write_journal(path, &mut journal, now);
        }
    });
}

fn known_grant(path: &Path, operation_id: &str) -> Result<Option<String>, ConsumeFailure> {
    with_lock(|| {
        Ok(read_journal(path)?
            .operations
            .get(operation_id)
            .map(|record| record.grant_id.clone()))
    })
}

/// The open operation for this account that can still be retried, if any.
pub fn pending_operation(path: &Path, account_id: &str, now: i64) -> Option<PendingOperation> {
    with_lock(|| {
        let journal = read_journal(path).ok()?;
        journal
            .operations
            .iter()
            .filter(|(_, record)| {
                !record.settled
                    && record.account_id == account_id
                    && record.created_at + RETRY_WINDOW_MS > now
            })
            .max_by_key(|(_, record)| record.created_at)
            .map(|(operation_id, record)| PendingOperation {
                operation_id: operation_id.clone(),
                grant_id: record.grant_id.clone(),
                retryable_until_ms: record.created_at + RETRY_WINDOW_MS,
            })
    })
}

fn valid_grant_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 40
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn is_organization_uuid(text: &str) -> bool {
    let parts: Vec<&str> = text.split('-').collect();
    let lengths = [8, 4, 4, 4, 12];
    parts.len() == 5
        && parts
            .iter()
            .zip(lengths)
            .all(|(part, len)| part.len() == len && part.chars().all(|c| c.is_ascii_hexdigit()))
}

async fn organization_uuid(route: &AuthRouteConfig, access: &str) -> Result<String, ConsumeFailure> {
    let body = super::usage::get(route, &consts::profile_url(), access)
        .await
        .map_err(|failure| match failure {
            Failure::Auth => ConsumeFailure::AuthFailed,
            _ => ConsumeFailure::UpstreamUnavailable,
        })?;
    let uuid = body
        .get("organization")
        .and_then(|org| org.get("uuid"))
        .and_then(Value::as_str)
        .filter(|uuid| is_organization_uuid(uuid))
        .ok_or(ConsumeFailure::UpstreamUnavailable)?;
    Ok(uuid.to_ascii_lowercase())
}

fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// The claim's answer, or why there is none.
enum Claim {
    Answer {
        code: String,
        resets_left: Option<i64>,
        cleared: Vec<String>,
    },
    /// May have run upstream without an answer we can read.
    Unknown,
}

async fn claim(
    route: &AuthRouteConfig,
    access: &str,
    organization: &str,
    grant_id: &str,
    request_id: &str,
) -> Claim {
    let url = format!(
        "{}/api/organizations/{organization}/reset_rate_limits",
        consts::api_origin()
    );
    let Ok(client) = create_raw_auth_client(&url, route) else {
        // Nothing was sent.
        return Claim::Answer {
            code: "unavailable".to_string(),
            resets_left: None,
            cleared: Vec::new(),
        };
    };
    let body = json!({"program": PROGRAM, "grant_id": grant_id, "request_id": request_id});
    let request = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("User-Agent", consts::ACCOUNT_USER_AGENT)
        .header("anthropic-beta", consts::OAUTH_BETA)
        .header("Authorization", format!("Bearer {access}"))
        .body(body.to_string())
        .send();
    let Ok(Ok(response)) = tokio::time::timeout(CLAIM_TIMEOUT, request).await else {
        return Claim::Unknown;
    };
    let status = response.status().as_u16();
    // Refusals that prove the claim did not run, exactly as the Claude Code client maps them.
    if status == 429 {
        return Claim::Answer {
            code: "rate_limited".to_string(),
            resets_left: None,
            cleared: Vec::new(),
        };
    }
    if status == 401 || status == 403 {
        return Claim::Answer {
            code: "auth_error".to_string(),
            resets_left: None,
            cleared: Vec::new(),
        };
    }
    if !response.status().is_success() {
        return Claim::Unknown;
    }
    let Ok(text) = response.text().await else {
        return Claim::Unknown;
    };
    let Ok(answer) = serde_json::from_str::<Value>(&text) else {
        return Claim::Unknown;
    };
    let Some(code) = answer
        .get("result")
        .and_then(Value::as_str)
        .filter(|code| RESULTS.contains(code))
    else {
        return Claim::Unknown;
    };
    let cleared = match answer.get("cleared") {
        Some(Value::Array(items)) => CLEARABLE
            .iter()
            .filter(|window| items.iter().any(|item| item.as_str() == Some(**window)))
            .map(|window| (*window).to_string())
            .collect(),
        _ => Vec::new(),
    };
    Claim::Answer {
        code: code.to_string(),
        resets_left: answer
            .get("resets_left")
            .and_then(Value::as_u64)
            .and_then(|n| i64::try_from(n).ok()),
        cleared,
    }
}

/// Spends one reset for the account that owns `access`.
///
/// `grant_id` of `None` picks the server's suggested grant (or the first spendable one). A retry
/// of an operation that got no answer passes the same `operation_id` (and no gate re-read).
pub async fn consume(
    route: &AuthRouteConfig,
    access: &str,
    journal: &Path,
    account_id: &str,
    grant_id: Option<&str>,
    operation_id: &str,
    now_ms: i64,
) -> Result<Settled, ConsumeFailure> {
    if !valid_request_id(operation_id) || grant_id.is_some_and(|id| !valid_grant_id(id)) {
        return Err(ConsumeFailure::InvalidInput);
    }
    let organization = organization_uuid(route, access).await?;
    let org_digest = digest(&organization);
    let journal: PathBuf = journal.to_path_buf();

    // A new operation must pass the spend gate on a fresh read. A same-id retry of an open
    // record skips it: that attempt was already gated, and upstream answers a repeated request
    // id for itself.
    let recorded = known_grant(&journal, operation_id)?;
    let grant_id = match recorded {
        Some(recorded) => {
            if grant_id.is_some_and(|given| given != recorded) {
                return Err(ConsumeFailure::IdentityMismatch);
            }
            recorded
        }
        None => {
            let parsed = fetch_credit(route, access).await.map_err(|failure| match failure {
                Failure::Auth => ConsumeFailure::AuthFailed,
                _ => ConsumeFailure::UpstreamUnavailable,
            })?;
            let chosen = match grant_id {
                Some(id) => id.to_string(),
                None => parsed
                    .next_grant_id
                    .clone()
                    .filter(|id| parsed.blocker(id).is_none())
                    .or_else(|| {
                        parsed
                            .credit
                            .grants
                            .iter()
                            .find(|g| parsed.blocker(&g.id).is_none())
                            .map(|g| g.id.clone())
                    })
                    .ok_or_else(|| {
                        // Say why the first grant is blocked when there is one.
                        match parsed.credit.grants.first() {
                            Some(first) => ConsumeFailure::NotUsable(
                                parsed.blocker(&first.id).unwrap_or("no_grant"),
                            ),
                            None if !parsed.credit.eligible => ConsumeFailure::NotUsable("ineligible"),
                            None => ConsumeFailure::NotUsable("no_grant"),
                        }
                    })?,
            };
            if let Some(reason) = parsed.blocker(&chosen) {
                return Err(ConsumeFailure::NotUsable(reason));
            }
            chosen
        }
    };

    let identity = Identity {
        operation_id,
        account_id,
        grant_id: &grant_id,
        org_digest: &org_digest,
    };
    match begin(&journal, &identity, now_ms)? {
        Begin::Replay(record) => {
            return Ok(Settled {
                code: record.code.unwrap_or_else(|| "unavailable".to_string()),
                replayed: true,
                resets_left: record.resets_left,
                cleared: record.cleared,
                grant_id,
                operation_id: operation_id.to_string(),
            });
        }
        Begin::Execute => {}
    }

    match claim(route, access, &organization, &grant_id, operation_id).await {
        Claim::Unknown => {
            release(&journal, operation_id, now_ms);
            Err(ConsumeFailure::UnknownOutcome {
                operation_id: operation_id.to_string(),
            })
        }
        Claim::Answer {
            code,
            resets_left,
            cleared,
        } => {
            // Report what the journal holds: an overlapping attempt may have settled first.
            let settled = settle(&journal, operation_id, &code, resets_left, &cleared, now_ms)
                .map_err(|()| ConsumeFailure::JournalWriteFailed {
                    operation_id: operation_id.to_string(),
                })?;
            let replayed =
                settled.code.as_deref() != Some(code.as_str()) || settled.resets_left != resets_left;
            Ok(Settled {
                code: settled.code.unwrap_or(code),
                replayed,
                resets_left: settled.resets_left,
                cleared: if replayed { Vec::new() } else { settled.cleared },
                grant_id,
                operation_id: operation_id.to_string(),
            })
        }
    }
}

#[cfg(test)]
#[path = "reset_tests.rs"]
mod tests;
