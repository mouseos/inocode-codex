//! Plain data types of the account pool. No credentials and no e-mail addresses live here.

use serde::Deserialize;
use serde::Serialize;

/// Which kind of credential an entry holds. A new provider adds a variant (and an
/// [`super::AccountProvider`]); nothing else in the pool is provider specific.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AccountKind {
    Chatgpt,
    ApiKey,
    /// A Claude (Pro/Max) OAuth sign-in.
    ClaudeOauth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AccountState {
    Active,
    Cooldown,
    Disabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WindowKind {
    FiveHour,
    Weekly,
    Both,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SelectionStrategy {
    Quota,
    RoundRobin,
    FillFirst,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApiKeyOrder {
    AfterChatgpt,
    BeforeChatgpt,
    Mixed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SwitchReason {
    Threshold,
    LimitReached,
    Manual,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    pub used_percent: f64,
    pub window_minutes: Option<i64>,
    /// Unix seconds.
    pub resets_at: Option<i64>,
}

impl UsageWindow {
    /// A window that has already reset counts as unused.
    fn effective_percent(&self, now: i64) -> f64 {
        match self.resets_at {
            Some(reset) if reset <= now => 0.0,
            _ => self.used_percent,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountUsage {
    pub five_hour: Option<UsageWindow>,
    pub weekly: Option<UsageWindow>,
    /// Unix seconds of the last refresh.
    pub updated_at: Option<i64>,
    /// A banked usage-limit reset (Claude subscriptions); `None` when the account has none or it
    /// has not been read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_credit: Option<ResetCredit>,
    /// How the last read of the reset credit went. `None` only when no read has reported yet
    /// (for example usage that came from rate-limit headers alone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_status: Option<ResetStatus>,
}

/// Whether the reset credit could be read, and why not when it could not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ResetStatusKind {
    /// Read; `reset_credit` holds it (zero resets is still `Ok`).
    Ok,
    /// The account is outside the reset-credit scheme (API keys).
    None,
    /// The endpoint does not offer it for this account or plan.
    Unavailable,
    /// The read failed (network, refusal, unreadable answer). `reset_credit` may hold the last
    /// good value, then `stale` is set.
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetStatus {
    pub kind: ResetStatusKind,
    /// A short machine word saying why (never a response body or a token).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `reset_credit` is the last good value, kept because the latest read failed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
}

impl ResetStatus {
    pub fn ok() -> Self {
        Self { kind: ResetStatusKind::Ok, reason: None, stale: false }
    }

    pub fn unavailable(reason: &str) -> Self {
        Self { kind: ResetStatusKind::Unavailable, reason: Some(reason.to_string()), stale: false }
    }

    pub fn error(reason: &str) -> Self {
        Self { kind: ResetStatusKind::Error, reason: Some(reason.to_string()), stale: false }
    }
}

/// One banked usage-limit reset grant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetGrant {
    pub id: String,
    pub label: String,
    pub resets_left: i64,
    pub resets_total: i64,
    /// Unix seconds; `None` when the grant does not expire.
    pub ends_at: Option<i64>,
    pub paused: bool,
    /// Whether this grant can be spent right now.
    pub usable_now: bool,
    /// The windows the grant clears (`five_hour`, `seven_day`, ...).
    pub clears: Vec<String>,
}

/// What the account has banked: resets that clear the 5-hour and weekly windows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetCredit {
    pub eligible: bool,
    pub at_limit: bool,
    /// Resets left across the grants that are not paused.
    pub resets_left: i64,
    pub resets_total: i64,
    /// Whether a reset can be spent right now.
    pub usable_now: bool,
    /// Unix seconds; the soonest end among grants with resets left.
    pub ends_at: Option<i64>,
    /// Unix seconds.
    pub cooldown_until: Option<i64>,
    pub grants: Vec<ResetGrant>,
}

impl AccountUsage {
    /// Builds a usage view from the backend's two windows. A window of at most six hours is the
    /// short window; anything longer is the weekly one. When the lengths are unknown the first
    /// window is the short one.
    pub fn from_windows(
        primary: Option<UsageWindow>,
        secondary: Option<UsageWindow>,
        now: i64,
    ) -> Self {
        let mut usage = Self {
            updated_at: Some(now),
            ..Self::default()
        };
        for (index, window) in [primary, secondary].into_iter().enumerate() {
            let Some(window) = window else { continue };
            let short = match window.window_minutes {
                Some(minutes) => minutes <= 360,
                None => index == 0,
            };
            if short && usage.five_hour.is_none() {
                usage.five_hour = Some(window);
            } else if usage.weekly.is_none() {
                usage.weekly = Some(window);
            } else {
                usage.five_hour = Some(window);
            }
        }
        usage
    }

    /// From the backend's rate-limit snapshot (response headers or the usage endpoint).
    pub fn from_snapshot(snapshot: &codex_protocol::protocol::RateLimitSnapshot, now: i64) -> Self {
        let window = |w: &codex_protocol::protocol::RateLimitWindow| UsageWindow {
            used_percent: w.used_percent,
            window_minutes: w.window_minutes,
            resets_at: w.resets_at,
        };
        Self::from_windows(
            snapshot.primary.as_ref().map(window),
            snapshot.secondary.as_ref().map(window),
            now,
        )
    }

    /// The percentage the policy cares about, or `None` when nothing is known.
    pub fn effective_percent(&self, window: WindowKind, now: i64) -> Option<f64> {
        let five = self.five_hour.as_ref().map(|w| w.effective_percent(now));
        let week = self.weekly.as_ref().map(|w| w.effective_percent(now));
        match window {
            WindowKind::FiveHour => five,
            WindowKind::Weekly => week,
            WindowKind::Both => match (five, week) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AccountPolicy {
    /// Switch before a window would exceed this percentage. 0-100.
    pub threshold_percent: f64,
    pub window: WindowKind,
    pub selection: SelectionStrategy,
    pub api_key_order: ApiKeyOrder,
    /// Cooldown when a limit error carries no reset time.
    pub default_cooldown_seconds: i64,
    /// Switch on threshold and on limit errors. Off means "use the selected one until told otherwise".
    pub auto_switch: bool,
}

impl Default for AccountPolicy {
    fn default() -> Self {
        Self {
            threshold_percent: 90.0,
            window: WindowKind::Both,
            selection: SelectionStrategy::Quota,
            api_key_order: ApiKeyOrder::AfterChatgpt,
            default_cooldown_seconds: 60,
            auto_switch: true,
        }
    }
}

/// Non-secret facts about one entry; stored next to its credentials.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountMeta {
    pub id: String,
    pub kind: AccountKind,
    pub alias: String,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub cooldown_until: Option<i64>,
    #[serde(default)]
    pub last_used_at: Option<i64>,
    #[serde(default)]
    pub usage: Option<AccountUsage>,
    #[serde(default)]
    pub added_at: i64,
    /// Which provider group the entry belongs to (`openai`, `claude`).
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Local only. Shown to the owner on the account screen; never sent anywhere else.
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_provider() -> String {
    "openai".to_string()
}

/// What a client sees of one entry.
#[derive(Clone, Debug, PartialEq)]
pub struct AccountView {
    pub id: String,
    pub kind: AccountKind,
    pub alias: String,
    pub plan: Option<String>,
    pub usage: Option<AccountUsage>,
    pub state: AccountState,
    pub cooldown_until: Option<i64>,
    pub priority: i32,
    pub last_used_at: Option<i64>,
    pub in_use: bool,
    pub is_default: bool,
    pub provider: String,
    pub email: Option<String>,
    pub name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchEvent {
    pub from_alias: Option<String>,
    pub to_alias: String,
    pub reason: SwitchReason,
    pub thread_id: Option<String>,
}

impl SwitchEvent {
    /// The one conversation line that says where the work went and why.
    pub fn summary(&self) -> String {
        let why = match self.reason {
            SwitchReason::Threshold => "使用量がしきい値に近いため",
            SwitchReason::LimitReached => "上限に達したため",
            SwitchReason::Manual => "手動",
        };
        match &self.from_alias {
            Some(from) => format!("アカウントを「{from}」から「{}」に切り替えました({why})", self.to_alias),
            None => format!("アカウント「{}」を使います({why})", self.to_alias),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PoolEvent {
    /// The list changed; re-read it with `list`.
    Updated,
    Switched(SwitchEvent),
    LoginCompleted {
        login_id: String,
        result: Result<String, String>,
    },
}
