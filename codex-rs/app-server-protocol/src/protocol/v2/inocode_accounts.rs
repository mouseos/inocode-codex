//! Experimental `inocode/accounts/*` extension APIs: the native account pool
//! (several ChatGPT accounts and OpenAI API keys, switched by policy).
//! Contract: ref/docs/16-account-pool-contract.md. Design background: ref/docs/15-account-pool-and-providers.md (option B).
//! Account e-mail addresses and credentials are never part of any of these types.

use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

/// Which kind of credential an entry holds. Other providers plug in here later.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeAccountKind {
    Chatgpt,
    ApiKey,
    /// A Claude (Pro/Max) OAuth sign-in.
    ClaudeOauth,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeAccountState {
    Active,
    Cooldown,
    Disabled,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeAccountWindowKind {
    FiveHour,
    Weekly,
    /// The higher of the two.
    Both,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeAccountSelection {
    /// Lowest usage first.
    Quota,
    RoundRobin,
    /// Highest priority first; move on only when it is unusable.
    FillFirst,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeApiKeyOrder {
    /// API keys are used only when every ChatGPT account is over the threshold or cooling down.
    AfterChatgpt,
    /// API keys are used before ChatGPT accounts.
    BeforeChatgpt,
    /// API keys and ChatGPT accounts are ranked together.
    Mixed,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeAccountSwitchReason {
    Threshold,
    LimitReached,
    Manual,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountUsageWindow {
    /// 0-100.
    pub used_percent: f64,
    #[ts(type = "number | null")]
    pub window_minutes: Option<i64>,
    /// Unix seconds.
    #[ts(type = "number | null")]
    pub resets_at: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountUsage {
    pub five_hour: Option<InocodeAccountUsageWindow>,
    pub weekly: Option<InocodeAccountUsageWindow>,
    /// Unix seconds of the last refresh; `null` when never fetched.
    #[ts(type = "number | null")]
    pub updated_at: Option<i64>,
    /// The banked usage-limit resets. Never `null`: `status` says whether they could be read.
    pub reset_credit: InocodeAccountResetCredit,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum InocodeResetCreditStatus {
    /// Read. Zero resets is still `ok` (`resetsLeft: 0`).
    Ok,
    /// The account is outside the reset-credit scheme (API keys).
    None,
    /// The endpoint does not offer it for this account or plan.
    Unavailable,
    /// The read failed. The fields below are the last good value when `stale` is true.
    Error,
}

/// `eligible` and the fields after it are present when `status` is `ok`, and also when it is
/// `error` with `stale: true` (the last good value). Otherwise they are absent.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountResetCredit {
    pub status: InocodeResetCreditStatus,
    /// Why, for `none`, `unavailable` and `error`: a short word such as `block_missing`,
    /// `block_null`, `block_unreadable`, `grant_query_refused`, `http_404`, `http_403`,
    /// `auth_refused`, `rate_limited`, `request_failed`, `timeout`, `token_unavailable`,
    /// `api_key`, `not_read`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub reason: Option<String>,
    /// The fields below are the last good value, kept because the latest read failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub stale: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub eligible: Option<bool>,
    /// The account is at a usage limit right now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub at_limit: Option<bool>,
    /// Resets left across the grants that are not paused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "number", optional = nullable)]
    pub resets_left: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "number", optional = nullable)]
    pub resets_total: Option<i64>,
    /// A reset can be spent right now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub usable_now: Option<bool>,
    /// Unix seconds; the soonest end among grants with resets left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "number | null", optional = nullable)]
    pub ends_at: Option<i64>,
    /// Unix seconds; no reset can be spent before this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "number | null", optional = nullable)]
    pub cooldown_until: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub grants: Option<Vec<InocodeAccountResetGrant>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountResetGrant {
    pub id: String,
    pub label: String,
    #[ts(type = "number")]
    pub resets_left: i64,
    #[ts(type = "number")]
    pub resets_total: i64,
    /// Unix seconds; `null` when the grant does not expire.
    #[ts(type = "number | null")]
    pub ends_at: Option<i64>,
    pub paused: bool,
    pub usable_now: bool,
    /// The windows the grant clears: `five_hour`, `seven_day`, `seven_day_overage_included`.
    pub clears: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsResetCreditHistoryParams {
    pub id: String,
    /// At most this many entries, newest first. Omitted: all.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum InocodeResetCreditHistoryKind {
    /// A grant was seen for the first time (`ts` is when it was noticed, not when it was given).
    Granted,
    /// A spend made from this app: it worked, was refused, or its outcome is not known.
    Used,
    /// A grant went down or away and this app's records do not account for it.
    UsedElsewhere,
    Expired,
    /// A usage window fell by a lot: the limit was reset.
    QuotaReset,
    /// The credit could not be read.
    Error,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum InocodeResetCreditHistorySource {
    /// Seen by comparing two reads of the usage.
    Observed,
    /// This app's own spend records.
    Journal,
    /// The server's own listing said so.
    Endpoint,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeResetCreditHistoryEntry {
    /// Unix seconds: when it happened if known (an expiry is dated by its deadline), otherwise
    /// when it was noticed.
    #[ts(type = "number")]
    pub ts: i64,
    pub kind: InocodeResetCreditHistoryKind,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub grant_id: Option<String>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub label: Option<String>,
    /// Display text (Japanese).
    pub detail: String,
    pub source: InocodeResetCreditHistorySource,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsResetCreditHistoryResponse {
    /// Newest first.
    pub entries: Vec<InocodeResetCreditHistoryEntry>,
}

/// One pool entry. There is deliberately no e-mail address: identify by `alias` and `plan`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccount {
    pub id: String,
    pub kind: InocodeAccountKind,
    /// Provider group the entry belongs to: `openai` or `claude`.
    pub provider: String,
    /// User-chosen name.
    pub alias: String,
    /// The account's e-mail address, from the sign-in (ChatGPT: ID token; Claude: profile).
    /// `null` when unknown. Local only; shown on the account screen.
    pub email: Option<String>,
    /// The owner's display name; `null` when unknown.
    pub name: Option<String>,
    /// Plan name for ChatGPT accounts (for example `plus`, `pro`); `null` for API keys.
    pub plan: Option<String>,
    /// `null` for API keys (no usage concept) or before the first fetch.
    pub usage: Option<InocodeAccountUsage>,
    pub state: InocodeAccountState,
    /// Unix seconds; set while `state` is `cooldown`.
    #[ts(type = "number | null")]
    pub cooldown_until: Option<i64>,
    /// Larger is used earlier.
    pub priority: i32,
    /// Unix seconds.
    #[ts(type = "number | null")]
    pub last_used_at: Option<i64>,
    /// True for the entry used by the next request.
    pub in_use: bool,
    /// True for the legacy `auth.json` login shown as the default account.
    pub is_default: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountPolicy {
    /// Switch before a window would exceed this percentage. 0-100, default 90.
    pub threshold_percent: f64,
    pub window: InocodeAccountWindowKind,
    pub selection: InocodeAccountSelection,
    pub api_key_order: InocodeApiKeyOrder,
    /// Cooldown in seconds when a limit error carries no reset time. Default 60.
    #[ts(type = "number")]
    pub default_cooldown_seconds: i64,
    /// Master switch for threshold/limit-based switching. Default true.
    pub auto_switch: bool,
}

// ---------------------------------------------------------------- methods

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsListParams {
    /// Refresh usage from the backend before answering. Default false.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub refresh: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsListResponse {
    pub accounts: Vec<InocodeAccount>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsAddApiKeyParams {
    pub alias: String,
    pub api_key: String,
    /// `openai` (default) or `claude`.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub provider: Option<String>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub priority: Option<i32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsAddApiKeyResponse {
    pub account: InocodeAccount,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsRemoveParams {
    pub id: String,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsRemoveResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsRenameParams {
    pub id: String,
    pub alias: String,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsRenameResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsSetPriorityParams {
    pub id: String,
    pub priority: i32,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsSetPriorityResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsSetEnabledParams {
    pub id: String,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsSetEnabledResponse {}

/// Manual switch. Takes effect from the next request of every thread.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsUseParams {
    pub id: String,
    /// Thread shown in the resulting `inocode/accounts/switched` notification.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub thread_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsUseResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeAccountLoginMethod {
    Browser,
    DeviceCode,
    /// Claude only: the provider shows `code#state`; hand it back with `login/complete`.
    OauthCode,
}

/// Signs in again to an account whose credentials expired or were refused. Answered like
/// `login/start`; completion arrives as `inocode/accounts/loginCompleted`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsReauthParams {
    pub id: String,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub method: Option<InocodeAccountLoginMethod>,
}

/// The same shape as [`InocodeAccountsLoginStartResponse`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsReauthResponse {
    pub login_id: String,
    pub auth_url: Option<String>,
    pub verification_url: Option<String>,
    pub user_code: Option<String>,
    pub code_entry: bool,
}

impl From<InocodeAccountsLoginStartResponse> for InocodeAccountsReauthResponse {
    fn from(value: InocodeAccountsLoginStartResponse) -> Self {
        Self {
            login_id: value.login_id,
            auth_url: value.auth_url,
            verification_url: value.verification_url,
            user_code: value.user_code,
            code_entry: value.code_entry,
        }
    }
}

/// Adds a ChatGPT account (never replaces the current login). Same flow as `account/login/start`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsLoginStartParams {
    pub alias: String,
    /// `openai` (default) or `claude`.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub provider: Option<String>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub method: Option<InocodeAccountLoginMethod>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsLoginStartResponse {
    pub login_id: String,
    /// Browser method: open this URL.
    pub auth_url: Option<String>,
    /// Device-code method.
    pub verification_url: Option<String>,
    pub user_code: Option<String>,
    /// True when the client must show a code box and call `login/complete`.
    pub code_entry: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsLoginCompleteParams {
    pub login_id: String,
    /// What the user pasted: `code#state`, the final redirect URL, or the bare code.
    pub code: String,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsLoginCompleteResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeProviderWire {
    Responses,
    Chat,
    Anthropic,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeProvider {
    pub id: String,
    pub name: String,
    pub wire: InocodeProviderWire,
    /// Which kinds of account can be added.
    pub auth_kinds: Vec<InocodeAccountKind>,
    pub account_count: u32,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeProvidersListParams {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeProvidersListResponse {
    pub providers: Vec<InocodeProvider>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsLoginCancelParams {
    pub login_id: String,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsLoginCancelResponse {}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsPolicyReadParams {
    /// `openai` (default) or `claude`: each provider has its own policy.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub provider: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsPolicyReadResponse {
    pub policy: InocodeAccountPolicy,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsPolicyWriteParams {
    pub policy: InocodeAccountPolicy,
    /// `openai` (default) or `claude`.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub provider: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsPolicyWriteResponse {
    pub policy: InocodeAccountPolicy,
}

// ---------------------------------------------------------- notifications

/// The account list changed (added, removed, usage refreshed, state changed...).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsUpdatedNotification {
    pub accounts: Vec<InocodeAccount>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsSwitchedNotification {
    /// Alias of the account used before; `null` when none.
    pub from_alias: Option<String>,
    pub to_alias: String,
    pub reason: InocodeAccountSwitchReason,
    pub thread_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsLoginCompletedNotification {
    pub login_id: String,
    pub success: bool,
    /// The new entry's id on success.
    pub account_id: Option<String>,
    pub error: Option<String>,
}

/// Spends one banked usage-limit reset of the account `id` (Claude subscription or ChatGPT).
/// API-key accounts have none: the answer is `ok: false` with reason `unsupported`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsResetCreditConsumeParams {
    pub id: String,
    /// The grant to spend (`usage.resetCredit.grants[].id`). Omitted: the server's suggestion,
    /// or the first one that can be spent.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub grant_id: Option<String>,
    /// Identifies this confirmation. Omitted: a new one is made. Pass the `operationId` of an
    /// earlier answer that said `retryable: true` to resend that very spend (it is the same
    /// request to the provider, so it cannot be spent twice).
    #[serde(default)]
    #[ts(optional = nullable)]
    pub operation_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/", rename_all = "camelCase")]
pub enum InocodeResetCreditFailureReason {
    /// No reset can be spent right now (none left, not at a limit, paused, not eligible).
    NotUsable,
    /// The reset itself has expired.
    Expired,
    /// The provider says a reset was used recently; try later.
    Cooldown,
    /// This reset was already used.
    AlreadyUsed,
    /// The provider could not be reached or gave no readable answer.
    NetworkError,
    /// Sign in to this account again.
    AuthFailed,
    /// The provider is rate limiting; try again shortly.
    RateLimited,
    /// A spend for this account is still being processed or waits for its answer.
    InProgress,
    /// This kind of account has no resets (API keys).
    Unsupported,
    /// The local record of the spend could not be read or written.
    JournalError,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeResetCreditFailure {
    pub reason: InocodeResetCreditFailureReason,
    /// A fixed word with more detail (for example `not_limited`, `paused`, `exhausted`,
    /// `no_grant`, `unknown_grant`, `ineligible`), or `null`.
    pub detail: Option<String>,
    /// Resending with the same `operationId` is safe and may succeed (the earlier attempt got no
    /// answer). When `false`, nothing was spent, or the answer is final.
    pub retryable: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsResetCreditConsumeResponse {
    /// A reset was used (now, or by this very operation earlier: see `replayed`).
    pub ok: bool,
    /// The provider's code (`reset`, `already_used`, `cooldown`, `no_credit`, ...), or `null`
    /// when there was no answer.
    pub code: Option<String>,
    /// The answer comes from the local record of an earlier attempt with this `operationId`.
    pub replayed: bool,
    /// Use this to retry (when `failure.retryable`).
    pub operation_id: String,
    pub grant_id: Option<String>,
    pub failure: Option<InocodeResetCreditFailure>,
    /// The account's usage after the spend (re-read), as in `accounts/list`.
    pub usage: Option<InocodeAccountUsage>,
    /// Shortcut for `usage.resetCredit`.
    pub reset_credit: Option<InocodeAccountResetCredit>,
}
