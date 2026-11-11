//! `inocode/accounts/*` and `inocode/providers/*`: the native account pool.
//! Contract: ref/docs/16-account-pool-contract.md.
//!
//! There is one pool per provider group: `openai` (ChatGPT sign-ins and OpenAI API keys) and
//! `claude` (Claude OAuth sign-ins and Claude API keys). The pools are process-wide
//! (`codex_login::account_pool`). This file is the JSON-RPC face: it converts types, drives
//! "add an account" sign-ins, and forwards pool events to clients as notifications. E-mail
//! addresses and credentials never leave through here.

use super::InocodeResult;
use super::unknown_method;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use crate::error_code::invalid_request;
use crate::outgoing_message::OutgoingMessageSender;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeAccount;
use codex_app_server_protocol::InocodeAccountKind;
use codex_app_server_protocol::InocodeAccountLoginMethod;
use codex_app_server_protocol::InocodeAccountPolicy;
use codex_app_server_protocol::InocodeAccountSelection;
use codex_app_server_protocol::InocodeAccountState;
use codex_app_server_protocol::InocodeAccountSwitchReason;
use codex_app_server_protocol::InocodeAccountResetCredit;
use codex_app_server_protocol::InocodeAccountResetGrant;
use codex_app_server_protocol::InocodeResetCreditStatus;
use codex_app_server_protocol::InocodeAccountsResetCreditConsumeParams;
use codex_app_server_protocol::InocodeAccountsResetCreditConsumeResponse;
use codex_app_server_protocol::InocodeResetCreditFailure;
use codex_app_server_protocol::InocodeResetCreditFailureReason;
use codex_app_server_protocol::InocodeAccountUsage;
use codex_app_server_protocol::InocodeAccountUsageWindow;
use codex_app_server_protocol::InocodeAccountWindowKind;
use codex_app_server_protocol::InocodeAccountsAddApiKeyResponse;
use codex_app_server_protocol::InocodeAccountsListResponse;
use codex_app_server_protocol::InocodeAccountsLoginCancelResponse;
use codex_app_server_protocol::InocodeAccountsLoginCompleteParams;
use codex_app_server_protocol::InocodeAccountsLoginCompleteResponse;
use codex_app_server_protocol::InocodeAccountsLoginCompletedNotification;
use codex_app_server_protocol::InocodeAccountsLoginStartParams;
use codex_app_server_protocol::InocodeAccountsLoginStartResponse;
use codex_app_server_protocol::InocodeAccountsPolicyReadResponse;
use codex_app_server_protocol::InocodeAccountsPolicyWriteResponse;
use codex_app_server_protocol::InocodeAccountsReauthParams;
use codex_app_server_protocol::InocodeAccountsReauthResponse;
use codex_app_server_protocol::InocodeAccountsRemoveResponse;
use codex_app_server_protocol::InocodeAccountsRenameResponse;
use codex_app_server_protocol::InocodeAccountsSetEnabledResponse;
use codex_app_server_protocol::InocodeAccountsSetPriorityResponse;
use codex_app_server_protocol::InocodeAccountsSwitchedNotification;
use codex_app_server_protocol::InocodeAccountsUpdatedNotification;
use codex_app_server_protocol::InocodeAccountsUseResponse;
use codex_app_server_protocol::InocodeApiKeyOrder;
use codex_app_server_protocol::InocodeProvider;
use codex_app_server_protocol::InocodeProviderWire;
use codex_app_server_protocol::InocodeProvidersListResponse;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ServerNotification;
use codex_core::config::Config;
use codex_login::AuthManager;
use codex_login::AuthManagerConfig;
use codex_login::CodexAuth;
use codex_login::ServerOptions;
use codex_login::account_pool;
use codex_login::account_pool::AccountKind;
use codex_login::account_pool::AccountPolicy;
use codex_login::account_pool::AccountPool;
use codex_login::account_pool::AccountState;
use codex_login::account_pool::AccountUsage;
use codex_login::account_pool::AccountView;
use codex_login::account_pool::ApiKeyOrder;
use codex_login::account_pool::OpenAiChatgptProvider;
use codex_login::account_pool::PoolEnv;
use codex_login::account_pool::PoolEvent;
use codex_login::account_pool::SelectionStrategy;
use codex_login::account_pool::SwitchReason;
use codex_login::account_pool::UsageFetcher;
use codex_login::account_pool::WindowKind;
use codex_login::claude_oauth;
use codex_login::complete_device_code_login;
use codex_login::oauth_client_id;
use codex_login::request_device_code;
use codex_login::run_login_server;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const USAGE_REFRESH_INTERVAL: Duration = Duration::from_secs(300);
/// A listing re-reads a Claude sign-in's usage when the last read is at least this old.
const CLAUDE_USAGE_MIN_AGE_SECS: i64 = 60;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const OPENAI: &str = "openai";
const CLAUDE: &str = "claude";
const CLAUDE_DIR: &str = "accounts-claude";

enum PendingLogin {
    Browser(codex_login::ShutdownHandle),
    Device(CancellationToken),
    /// Claude sign-in with a local listener.
    ClaudeListener(Arc<AtomicBool>),
    /// Claude sign-in waiting for the user to paste `code#state`.
    ClaudeCode {
        pending: claude_oauth::Pending,
        alias: String,
        expected: Option<String>,
    },
}

struct Runtime {
    outgoing: Arc<OutgoingMessageSender>,
    chatgpt_base_url: String,
    http_client_factory: codex_http_client::HttpClientFactory,
    logins: Mutex<HashMap<String, PendingLogin>>,
}

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

fn runtime() -> Result<&'static Runtime, JSONRPCErrorError> {
    RUNTIME
        .get()
        .ok_or_else(|| internal_error("the account pool is not running"))
}

fn pool_of(group: &str) -> Result<Arc<AccountPool>, JSONRPCErrorError> {
    account_pool::global_for(group)
        .ok_or_else(|| internal_error("the account pool is not ready yet"))
}

fn pool() -> Result<Arc<AccountPool>, JSONRPCErrorError> {
    pool_of(OPENAI)
}

fn all_pools() -> Vec<Arc<AccountPool>> {
    [OPENAI, CLAUDE]
        .iter()
        .filter_map(|group| account_pool::global_for(group))
        .collect()
}

fn all_accounts() -> Vec<InocodeAccount> {
    all_pools()
        .iter()
        .flat_map(|pool| pool.list())
        .map(to_account)
        .collect()
}

/// The pool that holds `id`.
fn pool_with(id: &str) -> Result<Arc<AccountPool>, JSONRPCErrorError> {
    all_pools()
        .into_iter()
        .find(|pool| pool.list().iter().any(|view| view.id == id))
        .ok_or_else(|| invalid_request("unknown account"))
}

fn group_of(provider: Option<&str>) -> Result<&'static str, JSONRPCErrorError> {
    match provider.unwrap_or(OPENAI) {
        OPENAI => Ok(OPENAI),
        CLAUDE => Ok(CLAUDE),
        other => Err(invalid_params(format!("unknown provider `{other}`"))),
    }
}

/// Everything the pool needs from the configuration.
struct PoolInputs {
    env: PoolEnv,
    chatgpt_base_url: String,
    http_client_factory: codex_http_client::HttpClientFactory,
}

fn pool_inputs(config: &Config) -> PoolInputs {
    PoolInputs {
        env: PoolEnv {
            codex_home: config.codex_home().to_path_buf(),
            mode: config.cli_auth_credentials_store_mode(),
            keyring: config.auth_keyring_backend_kind(),
            chatgpt_base_url: Some(config.chatgpt_base_url()),
            auth_route_config: config.auth_route_config(),
        },
        chatgpt_base_url: config.chatgpt_base_url(),
        http_client_factory: config.http_client_factory(),
    }
}

async fn open_with(inputs: PoolInputs, auth_manager: Arc<AuthManager>) -> Arc<AccountPool> {
    let fetcher = usage_fetcher(inputs.chatgpt_base_url, inputs.http_client_factory);
    let pool = AccountPool::open(inputs.env, auth_manager).await;
    pool.register_provider(Arc::new(OpenAiChatgptProvider::new(Some(fetcher))));
    pool
}

/// Opens the stored pool with ChatGPT usage reading wired in, without installing it process-wide
/// (for the `inocode accounts` commands).
pub async fn open_pool(config: &Config, auth_manager: Arc<AuthManager>) -> Arc<AccountPool> {
    open_with(pool_inputs(config), auth_manager).await
}

/// The Claude group's pool, without installing it process-wide.
pub async fn open_claude_pool(config: &Config) -> Arc<AccountPool> {
    AccountPool::open_group(pool_inputs(config).env, CLAUDE, CLAUDE_DIR).await
}

/// Opens the pools for this app-server and starts forwarding their events. Call once at start-up.
pub(crate) fn start(
    outgoing: Arc<OutgoingMessageSender>,
    auth_manager: Arc<AuthManager>,
    config: &Config,
) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let inputs = pool_inputs(config);
    if RUNTIME
        .set(Runtime {
            outgoing: Arc::clone(&outgoing),
            chatgpt_base_url: inputs.chatgpt_base_url.clone(),
            http_client_factory: inputs.http_client_factory.clone(),
            logins: Mutex::new(HashMap::new()),
        })
        .is_err()
    {
        return;
    }
    let claude_env = inputs.env.clone();
    handle.spawn(async move {
        let pool = open_with(inputs, auth_manager).await;
        let claude = AccountPool::open_group(claude_env, CLAUDE, CLAUDE_DIR).await;
        account_pool::install_global_for(OPENAI, Some(Arc::clone(&pool)));
        account_pool::install_global_for(CLAUDE, Some(Arc::clone(&claude)));
        pool.spawn_refresh_loop(USAGE_REFRESH_INTERVAL);
        claude.spawn_refresh_loop(USAGE_REFRESH_INTERVAL);
        for source in [pool, claude] {
            let mut events = source.subscribe();
            let outgoing = Arc::clone(&outgoing);
            tokio::spawn(async move {
                loop {
                    match events.recv().await {
                        Ok(PoolEvent::Updated) => {
                            outgoing
                                .send_server_notification(ServerNotification::InocodeAccountsUpdated(
                                    InocodeAccountsUpdatedNotification {
                                        accounts: all_accounts(),
                                    },
                                ))
                                .await;
                        }
                        Ok(PoolEvent::Switched(event)) => {
                            outgoing
                                .send_server_notification(ServerNotification::InocodeAccountsSwitched(
                                    InocodeAccountsSwitchedNotification {
                                        from_alias: event.from_alias,
                                        to_alias: event.to_alias,
                                        reason: to_reason(event.reason),
                                        thread_id: event.thread_id,
                                    },
                                ))
                                .await;
                        }
                        Ok(PoolEvent::LoginCompleted { .. }) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }
    });
}

/// Reads one account's usage from the ChatGPT backend with that account's own credentials.
fn usage_fetcher(
    base_url: String,
    http_client_factory: codex_http_client::HttpClientFactory,
) -> UsageFetcher {
    Arc::new(move |auth: CodexAuth| {
        let base_url = base_url.clone();
        let factory = http_client_factory.clone();
        Box::pin(async move {
            let client = codex_backend_client::Client::from_auth(base_url, &auth, factory);
            let status_read = tokio::time::timeout(
                Duration::from_secs(30),
                client.get_rate_limits_with_reset_credits(),
            )
            .await
            .map_err(|_| "the usage request timed out".to_string())?
            .map_err(|_| "the usage request failed".to_string())?;
            let snapshot = status_read
                .rate_limits
                .iter()
                .find(|snapshot| snapshot.limit_id.as_deref() == Some("codex"))
                .or_else(|| status_read.rate_limits.first())
                .ok_or_else(|| "the usage request returned no limits".to_string())?;
            let now = chrono::Utc::now().timestamp();
            let mut usage = AccountUsage::from_snapshot(snapshot, now);
            let at_limit = [&usage.five_hour, &usage.weekly]
                .into_iter()
                .flatten()
                .any(|window| window.used_percent >= 100.0);
            // The banked resets: the list endpoint, else the count the usage payload carries.
            let summary = status_read
                .rate_limit_reset_credits
                .as_ref()
                .map(|summary| summary.available_count);
            let listed = tokio::time::timeout(
                Duration::from_secs(5),
                client.list_rate_limit_reset_credits(),
            )
            .await;
            let (credit, status) = match listed {
                Ok(Ok(details)) => (
                    Some(chatgpt_reset_credit(&details, at_limit, now)),
                    account_pool::ResetStatus::ok(),
                ),
                Ok(Err(error)) => match (summary, classify_credit_error(&error.to_string())) {
                    (Some(count), _) => (
                        Some(counted_reset_credit(count, at_limit)),
                        account_pool::ResetStatus::ok(),
                    ),
                    (None, status) => (None, status),
                },
                Err(_) => match summary {
                    Some(count) => (
                        Some(counted_reset_credit(count, at_limit)),
                        account_pool::ResetStatus::ok(),
                    ),
                    None => (None, account_pool::ResetStatus::error("timeout")),
                },
            };
            usage.reset_credit = credit;
            usage.reset_status = Some(status);
            Ok(usage)
        })
    })
}

/// Why the credit list could not be read, from the request's own error text (which names the
/// HTTP status). The response body is never carried over.
fn classify_credit_error(message: &str) -> account_pool::ResetStatus {
    let code: Option<u16> = message
        .split_once("failed: ")
        .and_then(|(_, rest)| rest.get(..3))
        .and_then(|digits| digits.parse().ok());
    match code {
        Some(code @ (403 | 404)) => account_pool::ResetStatus::unavailable(&format!("http_{code}")),
        Some(code) => account_pool::ResetStatus::error(&format!("http_{code}")),
        None => account_pool::ResetStatus::error("request_failed"),
    }
}

/// A credit known only by its count (the usage payload's summary), without grant details.
fn counted_reset_credit(count: i64, at_limit: bool) -> account_pool::ResetCredit {
    account_pool::ResetCredit {
        eligible: true,
        at_limit,
        resets_left: count,
        resets_total: count,
        usable_now: count > 0,
        ends_at: None,
        cooldown_until: None,
        grants: Vec::new(),
    }
}

/// The ChatGPT backend's banked resets in the pool's shape. Each credit is one reset.
fn chatgpt_reset_credit(
    details: &codex_backend_client::RateLimitResetCreditsDetails,
    at_limit: bool,
    now: i64,
) -> account_pool::ResetCredit {
    let timestamp = |text: &str| {
        chrono::DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|t| t.timestamp())
    };
    let grants: Vec<account_pool::ResetGrant> = details
        .credits
        .iter()
        .map(|credit| {
            let ends_at = credit.expires_at.as_deref().and_then(timestamp);
            let available = credit.status == "available" && ends_at.is_none_or(|end| end > now);
            account_pool::ResetGrant {
                id: credit.id.clone(),
                label: credit
                    .title
                    .clone()
                    .or_else(|| credit.description.clone())
                    .unwrap_or_default(),
                resets_left: i64::from(credit.status == "available"),
                resets_total: 1,
                ends_at,
                paused: false,
                usable_now: available,
                clears: vec![credit.reset_type.clone()],
            }
        })
        .collect();
    let ends_at = grants
        .iter()
        .filter(|grant| grant.resets_left > 0)
        .filter_map(|grant| grant.ends_at)
        .min();
    account_pool::ResetCredit {
        eligible: true,
        at_limit,
        resets_left: details.available_count,
        resets_total: i64::try_from(grants.len()).unwrap_or(i64::MAX),
        usable_now: grants.iter().any(|grant| grant.usable_now),
        ends_at,
        cooldown_until: None,
        grants,
    }
}

// ------------------------------------------------------------ reset credit

/// What a spend came to, before it is put into the response.
struct SpendOutcome {
    ok: bool,
    code: Option<String>,
    replayed: bool,
    operation_id: String,
    grant_id: Option<String>,
    failure: Option<InocodeResetCreditFailure>,
}

fn failed(
    operation_id: &str,
    reason: InocodeResetCreditFailureReason,
    detail: Option<&str>,
    retryable: bool,
) -> SpendOutcome {
    SpendOutcome {
        ok: false,
        code: None,
        replayed: false,
        operation_id: operation_id.to_string(),
        grant_id: None,
        failure: Some(InocodeResetCreditFailure {
            reason,
            detail: detail.map(str::to_string),
            retryable,
        }),
    }
}

/// A provider's terminal code as an outcome. `reset` is the only success.
fn from_code(code: &str, replayed: bool, operation_id: &str, grant_id: Option<String>) -> SpendOutcome {
    use InocodeResetCreditFailureReason as Reason;
    let (ok, reason, detail) = match code {
        "reset" => (true, None, None),
        "already_used" | "already_redeemed" => (false, Some(Reason::AlreadyUsed), None),
        "cooldown" => (false, Some(Reason::Cooldown), None),
        "rate_limited" => (false, Some(Reason::RateLimited), None),
        "auth_error" => (false, Some(Reason::AuthFailed), None),
        "not_limited" => (false, Some(Reason::NotUsable), Some("not_limited")),
        "nothing_to_reset" => (false, Some(Reason::NotUsable), Some("nothing_to_reset")),
        "no_credit" => (false, Some(Reason::NotUsable), Some("no_credit")),
        "ineligible" => (false, Some(Reason::NotUsable), Some("ineligible")),
        _ => (false, Some(Reason::NotUsable), Some("unavailable")),
    };
    SpendOutcome {
        ok,
        code: Some(code.to_string()),
        replayed,
        operation_id: operation_id.to_string(),
        grant_id,
        failure: reason.map(|reason| InocodeResetCreditFailure {
            reason,
            detail: detail.map(str::to_string),
            retryable: false,
        }),
    }
}

async fn reset_credit_consume(
    params: InocodeAccountsResetCreditConsumeParams,
) -> Result<InocodeAccountsResetCreditConsumeResponse, JSONRPCErrorError> {
    use InocodeResetCreditFailureReason as Reason;
    let located = all_pools().into_iter().find_map(|pool| {
        let handles = pool.spend_handles(&params.id)?;
        Some((pool, handles))
    });
    let Some((pool, handles)) = located else {
        return Err(invalid_params(format!("unknown account `{}`", params.id)));
    };
    if params.grant_id.as_deref().is_some_and(str::is_empty)
        || params.operation_id.as_deref().is_some_and(str::is_empty)
    {
        return Err(invalid_params("grantId and operationId must not be empty"));
    }
    let operation_id = params
        .operation_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

    let outcome = match handles.kind {
        AccountKind::ApiKey => failed(&operation_id, Reason::Unsupported, Some("api_key"), false),
        AccountKind::ClaudeOauth => {
            let Some(claude) = handles.claude else {
                return Err(internal_error("the Claude sign-in is not loaded"));
            };
            spend_claude(&pool, &params, &claude, &operation_id).await?
        }
        AccountKind::Chatgpt => {
            spend_chatgpt(&handles.manager, params.grant_id.as_deref(), &operation_id).await?
        }
    };

    // A spend changes the windows: read them again so the answer and `accounts/updated` agree.
    if outcome.ok {
        pool.refresh_usage_of(&params.id).await;
    }
    let view = pool.list().into_iter().find(|view| view.id == params.id);
    let usage = view.and_then(|view| to_account(view).usage);
    let reset_credit = usage.as_ref().map(|usage| usage.reset_credit.clone());
    Ok(InocodeAccountsResetCreditConsumeResponse {
        ok: outcome.ok,
        code: outcome.code,
        replayed: outcome.replayed,
        operation_id: outcome.operation_id,
        grant_id: outcome.grant_id,
        failure: outcome.failure,
        usage,
        reset_credit,
    })
}

async fn spend_claude(
    pool: &AccountPool,
    params: &InocodeAccountsResetCreditConsumeParams,
    claude: &claude_oauth::ClaudeAuthManager,
    operation_id: &str,
) -> Result<SpendOutcome, JSONRPCErrorError> {
    use InocodeResetCreditFailureReason as Reason;
    use claude_oauth::ConsumeFailure as Failure;
    let Ok(access) = claude.access_token().await else {
        return Ok(failed(operation_id, Reason::AuthFailed, None, false));
    };
    let result = claude_oauth::consume_reset(
        &pool.env().auth_route_config,
        &access,
        &pool.reset_journal_path(),
        &params.id,
        params.grant_id.as_deref(),
        operation_id,
        now_ms(),
    )
    .await;
    Ok(match result {
        Ok(settled) => from_code(
            &settled.code,
            settled.replayed,
            &settled.operation_id,
            Some(settled.grant_id),
        ),
        Err(Failure::AuthFailed) => failed(operation_id, Reason::AuthFailed, None, false),
        Err(Failure::UpstreamUnavailable) => {
            failed(operation_id, Reason::NetworkError, None, false)
        }
        Err(Failure::NotUsable(word)) => {
            // "Not usable" for grants that have all ended is an expired reset.
            let now = chrono::Utc::now().timestamp();
            let credit = pool
                .list()
                .into_iter()
                .find(|view| view.id == params.id)
                .and_then(|view| view.usage)
                .and_then(|usage| usage.reset_credit);
            let expired = matches!(word, "not_usable" | "exhausted")
                && credit.is_some_and(|credit| {
                    let mut grants = credit
                        .grants
                        .iter()
                        .filter(|g| params.grant_id.as_deref().is_none_or(|id| g.id == id))
                        .peekable();
                    grants.peek().is_some() && grants.all(|g| g.ends_at.is_some_and(|end| end <= now))
                });
            if expired {
                failed(operation_id, Reason::Expired, Some(word), false)
            } else {
                failed(operation_id, Reason::NotUsable, Some(word), false)
            }
        }
        Err(Failure::InvalidInput) => {
            return Err(invalid_params("grantId or operationId is malformed"));
        }
        Err(Failure::IdentityMismatch) => {
            return Err(invalid_request(
                "this operationId belongs to another account or grant",
            ));
        }
        Err(Failure::InFlight) => failed(operation_id, Reason::InProgress, Some("in_flight"), false),
        Err(Failure::UnresolvedPrior { operation_id: prior }) => {
            // An earlier spend of this grant has no answer yet: it is the one to retry.
            failed(&prior, Reason::InProgress, Some("unresolved_prior"), true)
        }
        Err(Failure::Expired) => failed(operation_id, Reason::Expired, Some("operation_expired"), false),
        Err(Failure::UnknownOutcome { operation_id }) => {
            failed(&operation_id, Reason::NetworkError, Some("unknown_outcome"), true)
        }
        Err(Failure::JournalWriteFailed { operation_id }) => {
            failed(&operation_id, Reason::JournalError, Some("write_failed"), false)
        }
        Err(Failure::LedgerUnavailable) => {
            failed(operation_id, Reason::JournalError, Some("unavailable"), false)
        }
    })
}

/// The ChatGPT backend's own spend (`account/rateLimitResetCredit/consume`), made with this
/// account's credentials. The operation id is the backend's `redeem_request_id`, so a resend is
/// the same request.
async fn spend_chatgpt(
    manager: &AuthManager,
    credit_id: Option<&str>,
    operation_id: &str,
) -> Result<SpendOutcome, JSONRPCErrorError> {
    use InocodeResetCreditFailureReason as Reason;
    let runtime = runtime()?;
    let Some(auth) = manager.auth().await else {
        return Ok(failed(operation_id, Reason::AuthFailed, None, false));
    };
    if !auth.uses_codex_backend() {
        return Ok(failed(operation_id, Reason::Unsupported, Some("not_chatgpt"), false));
    }
    let client = codex_backend_client::Client::from_auth(
        runtime.chatgpt_base_url.clone(),
        &auth,
        runtime.http_client_factory.clone(),
    );
    let request = async {
        match credit_id {
            Some(id) => client.consume_rate_limit_reset_credit_by_id(operation_id, id).await,
            None => client.consume_rate_limit_reset_credit(operation_id).await,
        }
    };
    let response = match tokio::time::timeout(Duration::from_secs(10), request).await {
        Ok(Ok(response)) => response,
        // No readable answer: the same operation id may be resent safely.
        Ok(Err(_)) | Err(_) => {
            return Ok(failed(operation_id, Reason::NetworkError, Some("unknown_outcome"), true));
        }
    };
    use codex_backend_client::ConsumeRateLimitResetCreditCode as Code;
    let code = match response.code {
        Code::Reset => "reset",
        Code::NothingToReset => "nothing_to_reset",
        Code::NoCredit => "no_credit",
        Code::AlreadyRedeemed => "already_redeemed",
    };
    Ok(from_code(code, false, operation_id, credit_id.map(str::to_string)))
}

pub(crate) async fn handle(request: ClientRequest) -> InocodeResult {
    match request {
        ClientRequest::InocodeAccountsList { params, .. } => {
            if params.refresh.unwrap_or(false) {
                for pool in all_pools() {
                    pool.refresh_usage().await;
                }
            } else {
                // The answer below is not held up: a Claude sign-in whose usage is older than a
                // minute is re-read in the background, and `inocode/accounts/updated` follows.
                for pool in all_pools() {
                    tokio::spawn(async move {
                        pool.refresh_usage_if_stale(CLAUDE_USAGE_MIN_AGE_SECS).await;
                    });
                }
            }
            if all_pools().is_empty() {
                return Err(internal_error("the account pool is not ready yet"));
            }
            Ok(Some(
                InocodeAccountsListResponse {
                    accounts: all_accounts(),
                }
                .into(),
            ))
        }
        ClientRequest::InocodeAccountsAddApiKey { params, .. } => {
            let view = pool_of(group_of(params.provider.as_deref())?)?
                .add_api_key(&params.alias, &params.api_key, params.priority)
                .await
                .map_err(client_error)?;
            Ok(Some(
                InocodeAccountsAddApiKeyResponse {
                    account: to_account(view),
                }
                .into(),
            ))
        }
        ClientRequest::InocodeAccountsRemove { params, .. } => {
            pool_with(&params.id)?
                .remove(&params.id)
                .await
                .map_err(client_error)?;
            Ok(Some(InocodeAccountsRemoveResponse {}.into()))
        }
        ClientRequest::InocodeAccountsRename { params, .. } => {
            pool_with(&params.id)?
                .rename(&params.id, &params.alias)
                .map_err(client_error)?;
            Ok(Some(InocodeAccountsRenameResponse {}.into()))
        }
        ClientRequest::InocodeAccountsSetPriority { params, .. } => {
            pool_with(&params.id)?
                .set_priority(&params.id, params.priority)
                .map_err(client_error)?;
            Ok(Some(InocodeAccountsSetPriorityResponse {}.into()))
        }
        ClientRequest::InocodeAccountsSetEnabled { params, .. } => {
            pool_with(&params.id)?
                .set_enabled(&params.id, params.enabled)
                .map_err(client_error)?;
            Ok(Some(InocodeAccountsSetEnabledResponse {}.into()))
        }
        ClientRequest::InocodeAccountsUse { params, .. } => {
            pool_with(&params.id)?
                .use_account(&params.id, params.thread_id.as_deref())
                .map_err(client_error)?;
            Ok(Some(InocodeAccountsUseResponse {}.into()))
        }
        ClientRequest::InocodeAccountsLoginStart { params, .. } => {
            Ok(Some(login_start(params, None).await?.into()))
        }
        ClientRequest::InocodeAccountsReauth { params, .. } => {
            Ok(Some(
                InocodeAccountsReauthResponse::from(reauth(params).await?).into(),
            ))
        }
        ClientRequest::InocodeAccountsResetCreditConsume { params, .. } => {
            Ok(Some(reset_credit_consume(params).await?.into()))
        }
        ClientRequest::InocodeAccountsLoginComplete { params, .. } => {
            login_complete(params).await?;
            Ok(Some(InocodeAccountsLoginCompleteResponse {}.into()))
        }
        ClientRequest::InocodeAccountsLoginCancel { params, .. } => {
            let pending = runtime()?
                .logins
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&params.login_id);
            match pending {
                Some(PendingLogin::Browser(handle)) => handle.shutdown(),
                Some(PendingLogin::Device(token)) => token.cancel(),
                Some(PendingLogin::ClaudeListener(cancel)) => cancel.store(true, Ordering::SeqCst),
                Some(PendingLogin::ClaudeCode { .. }) => {}
                None => return Err(invalid_request("unknown login id")),
            }
            if let Ok(pool) = pool() {
                pool.discard_pending_login(&params.login_id);
            }
            Ok(Some(InocodeAccountsLoginCancelResponse {}.into()))
        }
        ClientRequest::InocodeAccountsPolicyRead { params, .. } => Ok(Some(
            InocodeAccountsPolicyReadResponse {
                policy: to_policy(pool_of(group_of(params.provider.as_deref())?)?.policy()),
            }
            .into(),
        )),
        ClientRequest::InocodeAccountsPolicyWrite { params, .. } => {
            // Each provider has its own policy; the other provider's is left alone.
            let target = pool_of(group_of(params.provider.as_deref())?)?;
            let saved = target
                .set_policy(from_policy(params.policy)?)
                .map_err(|err| invalid_params(err.to_string()))?;
            Ok(Some(
                InocodeAccountsPolicyWriteResponse {
                    policy: to_policy(saved),
                }
                .into(),
            ))
        }
        ClientRequest::InocodeProvidersList { .. } => Ok(Some(
            InocodeProvidersListResponse {
                providers: providers(),
            }
            .into(),
        )),
        other => Err(unknown_method(other.method_name())),
    }
}

fn providers() -> Vec<InocodeProvider> {
    let count = |group: &str| {
        account_pool::global_for(group)
            .map_or(0, |pool| u32::try_from(pool.list().len()).unwrap_or(u32::MAX))
    };
    vec![
        InocodeProvider {
            id: OPENAI.to_string(),
            name: "OpenAI".to_string(),
            wire: InocodeProviderWire::Responses,
            auth_kinds: vec![InocodeAccountKind::Chatgpt, InocodeAccountKind::ApiKey],
            account_count: count(OPENAI),
        },
        InocodeProvider {
            id: CLAUDE.to_string(),
            name: "Claude".to_string(),
            wire: InocodeProviderWire::Anthropic,
            auth_kinds: vec![InocodeAccountKind::ClaudeOauth, InocodeAccountKind::ApiKey],
            account_count: count(CLAUDE),
        },
    ]
}

fn client_error(err: std::io::Error) -> JSONRPCErrorError {
    invalid_request(err.to_string())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Adds an account without touching the current login: the sign-in writes into a pending
/// directory (ChatGPT) or is held in memory (Claude) until it succeeds.
/// Signs in again to an existing account; the new sign-in must be the same account.
async fn reauth(
    params: InocodeAccountsReauthParams,
) -> Result<InocodeAccountsLoginStartResponse, JSONRPCErrorError> {
    let view = pool_with(&params.id)?
        .list()
        .into_iter()
        .find(|view| view.id == params.id)
        .ok_or_else(|| invalid_request("unknown account"))?;
    let provider = match view.kind {
        _ if view.is_default => {
            return Err(invalid_request(
                "the default login is re-authenticated with account/login/start",
            ));
        }
        AccountKind::Chatgpt => OPENAI,
        AccountKind::ClaudeOauth => CLAUDE,
        AccountKind::ApiKey => {
            return Err(invalid_request(
                "an API key cannot be re-authenticated; remove it and add the new key",
            ));
        }
    };
    login_start(
        InocodeAccountsLoginStartParams {
            alias: view.alias,
            provider: Some(provider.to_string()),
            method: params.method,
        },
        Some(view.id),
    )
    .await
}

async fn login_start(
    params: InocodeAccountsLoginStartParams,
    expected: Option<String>,
) -> Result<InocodeAccountsLoginStartResponse, JSONRPCErrorError> {
    let runtime = runtime()?;
    let alias = params.alias.trim().to_string();
    if alias.is_empty() {
        return Err(invalid_params("alias must not be empty"));
    }
    let login_id = uuid::Uuid::new_v4().to_string();

    if group_of(params.provider.as_deref())? == CLAUDE {
        return claude_login_start(runtime, login_id, alias, params.method, expected).await;
    }

    let pool = pool()?;
    let env = pool.env().clone();
    let opts = ServerOptions {
        open_browser: false,
        ..ServerOptions::new(
            pool.store().pending_dir(&login_id),
            oauth_client_id(),
            /*forced_chatgpt_workspace_id*/ None,
            env.mode,
            env.keyring,
            env.auth_route_config.clone(),
        )
    };
    match params.method.unwrap_or(InocodeAccountLoginMethod::Browser) {
        InocodeAccountLoginMethod::Browser => {
            let server = run_login_server(opts)
                .map_err(|err| internal_error(format!("failed to start login server: {err}")))?;
            let auth_url = server.auth_url.clone();
            let shutdown = server.cancel_handle();
            register(runtime, &login_id, PendingLogin::Browser(shutdown.clone()));
            let id = login_id.clone();
            tokio::spawn(async move {
                let result = match tokio::time::timeout(
                    LOGIN_TIMEOUT,
                    server.block_until_done_with_callback_result(),
                )
                .await
                {
                    Ok(Ok(_)) => Ok(()),
                    Ok(Err(err)) => Err(format!("Login server error: {err}")),
                    Err(_) => {
                        shutdown.shutdown();
                        Err("Login timed out".to_string())
                    }
                };
                finish(id, alias, result, expected).await;
            });
            Ok(InocodeAccountsLoginStartResponse {
                login_id,
                auth_url: Some(auth_url),
                verification_url: None,
                user_code: None,
                code_entry: false,
            })
        }
        InocodeAccountLoginMethod::DeviceCode => {
            let device_code = request_device_code(&opts)
                .await
                .map_err(|err| internal_error(format!("failed to request device code: {err}")))?;
            let verification_url = device_code.verification_url.clone();
            let user_code = device_code.user_code.clone();
            let cancel = CancellationToken::new();
            register(runtime, &login_id, PendingLogin::Device(cancel.clone()));
            let id = login_id.clone();
            tokio::spawn(async move {
                let result = tokio::select! {
                    _ = cancel.cancelled() => Err("Login was not completed".to_string()),
                    done = complete_device_code_login(opts, device_code) => {
                        done.map_err(|err| err.to_string())
                    }
                };
                finish(id, alias, result, expected).await;
            });
            Ok(InocodeAccountsLoginStartResponse {
                login_id,
                auth_url: None,
                verification_url: Some(verification_url),
                user_code: Some(user_code),
                code_entry: false,
            })
        }
        InocodeAccountLoginMethod::OauthCode => {
            Err(invalid_params("oauthCode is only available for the claude provider"))
        }
    }
}

async fn claude_login_start(
    runtime: &'static Runtime,
    login_id: String,
    alias: String,
    method: Option<InocodeAccountLoginMethod>,
    expected: Option<String>,
) -> Result<InocodeAccountsLoginStartResponse, JSONRPCErrorError> {
    let mut code_mode = match method.unwrap_or(InocodeAccountLoginMethod::Browser) {
        InocodeAccountLoginMethod::Browser => false,
        InocodeAccountLoginMethod::OauthCode => true,
        InocodeAccountLoginMethod::DeviceCode => {
            return Err(invalid_params("the claude provider has no device-code sign-in"));
        }
    };
    // One sign-in at a time per provider: a new one cancels the one still waiting.
    supersede_claude_logins(runtime);
    // Bind before handing out the URL. If another process holds the port, fall back to the
    // paste-the-code method (the response then says `codeEntry: true`).
    let mut listener = None;
    if !code_mode {
        match claude_oauth::CallbackListener::bind().await {
            Ok(bound) => listener = Some(bound),
            Err(_) => code_mode = true,
        }
    }
    let pending = claude_oauth::begin(code_mode);
    let auth_url = pending.url.clone();
    if code_mode {
        register(
            runtime,
            &login_id,
            PendingLogin::ClaudeCode {
                pending,
                alias,
                expected,
            },
        );
        return Ok(InocodeAccountsLoginStartResponse {
            login_id,
            auth_url: Some(auth_url),
            verification_url: None,
            user_code: None,
            code_entry: true,
        });
    }
    let Some(listener) = listener else {
        return Err(internal_error("the sign-in listener is gone"));
    };
    let cancel = Arc::new(AtomicBool::new(false));
    register(runtime, &login_id, PendingLogin::ClaudeListener(Arc::clone(&cancel)));
    let id = login_id.clone();
    tokio::spawn(async move {
        let result = match listener
            .wait(pending.state.clone(), LOGIN_TIMEOUT, cancel)
            .await
        {
            Ok(code) => claude_finish(&pending, &alias, &code, expected.as_deref()).await,
            Err(err) => Err(err.to_string()),
        };
        finish_claude(id, result).await;
    });
    Ok(InocodeAccountsLoginStartResponse {
        login_id,
        auth_url: Some(auth_url),
        verification_url: None,
        user_code: None,
        code_entry: false,
    })
}

/// Exchanges the code and registers the sign-in. The error text never contains a credential.
async fn claude_finish(
    pending: &claude_oauth::Pending,
    alias: &str,
    code: &str,
    expected: Option<&str>,
) -> Result<String, String> {
    let pool = pool_of(CLAUDE).map_err(|_| "the account pool is not ready yet".to_string())?;
    let credentials = claude_oauth::exchange(
        pending,
        code,
        &pool.env().auth_route_config,
        now_ms(),
    )
    .await
    .map_err(|err| err.to_string())?;
    let profile = claude_oauth::fetch_profile(&pool.env().auth_route_config, &credentials.access).await;
    let view = pool
        .add_claude_oauth_expecting(alias, credentials, expected)
        .await
        .map_err(|err| err.to_string())?;
    let _ = pool.set_identity(&view.id, profile.email, profile.name);
    // Fill the usage bars now instead of waiting for the first request or the next refresh.
    pool.refresh_usage_of(&view.id).await;
    Ok(view.id)
}

async fn login_complete(params: InocodeAccountsLoginCompleteParams) -> Result<(), JSONRPCErrorError> {
    let runtime = runtime()?;
    let entry = runtime
        .logins
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&params.login_id);
    let Some(PendingLogin::ClaudeCode {
        pending,
        alias,
        expected,
    }) = entry
    else {
        return Err(invalid_request("this login is not waiting for a code"));
    };
    let result = claude_finish(&pending, &alias, &params.code, expected.as_deref()).await;
    let failed = result.clone().err();
    finish_claude(params.login_id, result).await;
    match failed {
        Some(message) => Err(invalid_request(message)),
        None => Ok(()),
    }
}

/// Cancels every Claude sign-in that is still waiting (listener or pasted code).
fn supersede_claude_logins(runtime: &Runtime) {
    let mut logins = runtime
        .logins
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let stale: Vec<String> = logins
        .iter()
        .filter(|(_, pending)| {
            matches!(
                pending,
                PendingLogin::ClaudeListener(_) | PendingLogin::ClaudeCode { .. }
            )
        })
        .map(|(id, _)| id.clone())
        .collect();
    for id in stale {
        if let Some(PendingLogin::ClaudeListener(cancel)) = logins.remove(&id) {
            cancel.store(true, Ordering::SeqCst);
        }
    }
}

/// Ends every sign-in that is still waiting (the client went away), so no listener keeps a port
/// or a blocking thread alive and the process can exit.
pub(crate) fn cancel_all_logins() {
    let Some(runtime) = RUNTIME.get() else { return };
    let pending: Vec<PendingLogin> = runtime
        .logins
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .drain()
        .map(|(_, pending)| pending)
        .collect();
    for login in pending {
        match login {
            PendingLogin::Browser(handle) => handle.shutdown(),
            PendingLogin::Device(cancel) => cancel.cancel(),
            PendingLogin::ClaudeListener(cancel) => cancel.store(true, Ordering::SeqCst),
            PendingLogin::ClaudeCode { .. } => {}
        }
    }
}

fn register(runtime: &Runtime, login_id: &str, pending: PendingLogin) {
    runtime
        .logins
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(login_id.to_string(), pending);
}

async fn finish_claude(login_id: String, result: Result<String, String>) {
    let Ok(runtime) = runtime() else { return };
    runtime
        .logins
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&login_id);
    notify_login(runtime, login_id, result).await;
}

async fn notify_login(runtime: &Runtime, login_id: String, result: Result<String, String>) {
    let (success, account_id, error) = match result {
        Ok(id) => (true, Some(id), None),
        Err(message) => (false, None, Some(message)),
    };
    runtime
        .outgoing
        .send_server_notification(ServerNotification::InocodeAccountsLoginCompleted(
            InocodeAccountsLoginCompletedNotification {
                login_id,
                success,
                account_id,
                error,
            },
        ))
        .await;
}

async fn finish(
    login_id: String,
    alias: String,
    result: Result<(), String>,
    expected: Option<String>,
) {
    let Ok(runtime) = runtime() else { return };
    runtime
        .logins
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&login_id);
    let Ok(pool) = pool() else { return };
    let outcome = match result {
        Ok(()) => pool
            .finish_chatgpt_login_expecting(&login_id, &alias, expected.as_deref())
            .await
            .map(|view| view.id)
            .map_err(|err| err.to_string()),
        Err(message) => Err(message),
    };
    if outcome.is_err() {
        pool.discard_pending_login(&login_id);
    }
    notify_login(runtime, login_id, outcome).await;
}

// ------------------------------------------------------------------ conversion

fn to_account(view: AccountView) -> InocodeAccount {
    InocodeAccount {
        id: view.id,
        kind: match view.kind {
            AccountKind::Chatgpt => InocodeAccountKind::Chatgpt,
            AccountKind::ApiKey => InocodeAccountKind::ApiKey,
            AccountKind::ClaudeOauth => InocodeAccountKind::ClaudeOauth,
        },
        provider: view.provider,
        alias: view.alias,
        email: view.email,
        name: view.name,
        plan: view.plan,
        usage: view.usage.map(|usage| InocodeAccountUsage {
            five_hour: usage.five_hour.map(to_window),
            weekly: usage.weekly.map(to_window),
            updated_at: usage.updated_at,
            reset_credit: to_reset_credit(
                usage.reset_credit,
                usage.reset_status,
                view.kind == AccountKind::ApiKey,
            ),
        }),
        state: match view.state {
            AccountState::Active => InocodeAccountState::Active,
            AccountState::Cooldown => InocodeAccountState::Cooldown,
            AccountState::Disabled => InocodeAccountState::Disabled,
        },
        cooldown_until: view.cooldown_until,
        priority: view.priority,
        last_used_at: view.last_used_at,
        in_use: view.in_use,
        is_default: view.is_default,
    }
}

/// The reset credit as the GUI sees it: always an object, with a status that says whether the
/// numbers can be trusted.
fn to_reset_credit(
    credit: Option<account_pool::ResetCredit>,
    status: Option<account_pool::ResetStatus>,
    api_key: bool,
) -> InocodeAccountResetCredit {
    use account_pool::ResetStatusKind as Kind;
    let (kind, reason, stale) = match status {
        Some(status) => (status.kind, status.reason, status.stale),
        None if api_key => (Kind::None, Some("api_key".to_string()), false),
        // A credit read earlier, with no word since: it is the last thing the endpoint said.
        None if credit.is_some() => (Kind::Ok, None, false),
        None => (Kind::Error, Some("not_read".to_string()), false),
    };
    let status = match kind {
        Kind::Ok => InocodeResetCreditStatus::Ok,
        Kind::None => InocodeResetCreditStatus::None,
        Kind::Unavailable => InocodeResetCreditStatus::Unavailable,
        Kind::Error => InocodeResetCreditStatus::Error,
    };
    // Numbers go out for a good read, and for a failed one only as the stale last good value.
    let credit = credit.filter(|_| matches!(kind, Kind::Ok) || (matches!(kind, Kind::Error) && stale));
    let mut out = InocodeAccountResetCredit {
        status,
        reason,
        stale: stale && credit.is_some(),
        eligible: None,
        at_limit: None,
        resets_left: None,
        resets_total: None,
        usable_now: None,
        ends_at: None,
        cooldown_until: None,
        grants: None,
    };
    if let Some(credit) = credit {
        out.eligible = Some(credit.eligible);
        out.at_limit = Some(credit.at_limit);
        out.resets_left = Some(credit.resets_left);
        out.resets_total = Some(credit.resets_total);
        out.usable_now = Some(credit.usable_now);
        out.ends_at = credit.ends_at;
        out.cooldown_until = credit.cooldown_until;
        out.grants = Some(
            credit
                .grants
                .into_iter()
                .map(|grant| InocodeAccountResetGrant {
                    id: grant.id,
                    label: grant.label,
                    resets_left: grant.resets_left,
                    resets_total: grant.resets_total,
                    ends_at: grant.ends_at,
                    paused: grant.paused,
                    usable_now: grant.usable_now,
                    clears: grant.clears,
                })
                .collect(),
        );
    }
    out
}

fn to_window(window: account_pool::UsageWindow) -> InocodeAccountUsageWindow {
    InocodeAccountUsageWindow {
        used_percent: window.used_percent,
        window_minutes: window.window_minutes,
        resets_at: window.resets_at,
    }
}

fn to_reason(reason: SwitchReason) -> InocodeAccountSwitchReason {
    match reason {
        SwitchReason::Threshold => InocodeAccountSwitchReason::Threshold,
        SwitchReason::LimitReached => InocodeAccountSwitchReason::LimitReached,
        SwitchReason::Manual => InocodeAccountSwitchReason::Manual,
    }
}

fn to_policy(policy: AccountPolicy) -> InocodeAccountPolicy {
    InocodeAccountPolicy {
        threshold_percent: policy.threshold_percent,
        window: match policy.window {
            WindowKind::FiveHour => InocodeAccountWindowKind::FiveHour,
            WindowKind::Weekly => InocodeAccountWindowKind::Weekly,
            WindowKind::Both => InocodeAccountWindowKind::Both,
        },
        selection: match policy.selection {
            SelectionStrategy::Quota => InocodeAccountSelection::Quota,
            SelectionStrategy::RoundRobin => InocodeAccountSelection::RoundRobin,
            SelectionStrategy::FillFirst => InocodeAccountSelection::FillFirst,
        },
        api_key_order: match policy.api_key_order {
            ApiKeyOrder::AfterChatgpt => InocodeApiKeyOrder::AfterChatgpt,
            ApiKeyOrder::BeforeChatgpt => InocodeApiKeyOrder::BeforeChatgpt,
            ApiKeyOrder::Mixed => InocodeApiKeyOrder::Mixed,
        },
        default_cooldown_seconds: policy.default_cooldown_seconds,
        auto_switch: policy.auto_switch,
    }
}

fn from_policy(policy: InocodeAccountPolicy) -> Result<AccountPolicy, JSONRPCErrorError> {
    Ok(AccountPolicy {
        threshold_percent: policy.threshold_percent,
        window: match policy.window {
            InocodeAccountWindowKind::FiveHour => WindowKind::FiveHour,
            InocodeAccountWindowKind::Weekly => WindowKind::Weekly,
            InocodeAccountWindowKind::Both => WindowKind::Both,
        },
        selection: match policy.selection {
            InocodeAccountSelection::Quota => SelectionStrategy::Quota,
            InocodeAccountSelection::RoundRobin => SelectionStrategy::RoundRobin,
            InocodeAccountSelection::FillFirst => SelectionStrategy::FillFirst,
        },
        api_key_order: match policy.api_key_order {
            InocodeApiKeyOrder::AfterChatgpt => ApiKeyOrder::AfterChatgpt,
            InocodeApiKeyOrder::BeforeChatgpt => ApiKeyOrder::BeforeChatgpt,
            InocodeApiKeyOrder::Mixed => ApiKeyOrder::Mixed,
        },
        default_cooldown_seconds: policy.default_cooldown_seconds,
        auto_switch: policy.auto_switch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn policy_round_trips_through_the_wire_type() {
        let policy = AccountPolicy::default();
        let wire = to_policy(policy.clone());
        assert_eq!(wire.threshold_percent, 90.0);
        assert_eq!(from_policy(wire).expect("policy"), policy);
    }

    #[test]
    fn an_account_on_the_wire_carries_no_credentials_or_address() {
        let view = AccountView {
            id: "apikey-3f9c1a".to_string(),
            kind: AccountKind::ApiKey,
            alias: "予備キー".to_string(),
            plan: None,
            usage: None,
            state: AccountState::Active,
            cooldown_until: None,
            priority: 0,
            last_used_at: None,
            in_use: false,
            is_default: false,
            provider: "openai".to_string(),
            email: None,
            name: None,
        };
        let json = serde_json::to_string(&to_account(view)).expect("json");
        assert!(!json.contains("sk-") && !json.contains('@'), "{json}");
        assert!(json.contains("\"kind\":\"apiKey\""));
        assert!(json.contains("\"provider\":\"openai\""));
        assert!(json.contains("\"email\":null") && json.contains("\"name\":null"));
    }

    #[test]
    fn unknown_providers_are_rejected_and_known_ones_resolve() {
        assert_eq!(group_of(None).expect("default"), OPENAI);
        assert_eq!(group_of(Some("claude")).expect("claude"), CLAUDE);
        assert!(group_of(Some("muse")).is_err());
    }

    #[tokio::test]
    async fn the_methods_answer_and_never_return_the_key() {
        use codex_app_server_protocol::InocodeAccountsAddApiKeyParams;
        use codex_app_server_protocol::InocodeAccountsListParams;
        use codex_app_server_protocol::InocodeAccountsPolicyReadParams;
        use codex_app_server_protocol::InocodeAccountsRenameParams;
        use codex_app_server_protocol::InocodeAccountsUseParams;
        use codex_app_server_protocol::RequestId;

        let home = tempfile::tempdir().expect("tempdir");
        let primary =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("sk-test-FAKE-handler-main"));
        let env = PoolEnv {
            codex_home: home.path().to_path_buf(),
            mode: codex_login::AuthCredentialsStoreMode::File,
            keyring: Default::default(),
            chatgpt_base_url: None,
            auth_route_config: codex_login::test_support::transport_default_auth_route_config(),
        };
        account_pool::install_global(Some(AccountPool::open(env, primary).await));
        let id = |n| RequestId::Integer(n);

        let added = handle(ClientRequest::InocodeAccountsAddApiKey {
            request_id: id(1),
            params: InocodeAccountsAddApiKeyParams {
                alias: "予備キー".to_string(),
                api_key: "sk-test-FAKE-handler-extra".to_string(),
                priority: Some(2),
                provider: None,
            },
        })
        .await
        .expect("add")
        .expect("payload");
        let key_id = serde_json::to_value(&added).expect("json")["account"]["id"]
            .as_str()
            .expect("id")
            .to_string();

        handle(ClientRequest::InocodeAccountsRename {
            request_id: id(2),
            params: InocodeAccountsRenameParams {
                id: key_id.clone(),
                alias: "改名".to_string(),
            },
        })
        .await
        .expect("rename");
        handle(ClientRequest::InocodeAccountsUse {
            request_id: id(3),
            params: InocodeAccountsUseParams {
                id: key_id.clone(),
                thread_id: Some("thr_1".to_string()),
            },
        })
        .await
        .expect("use");

        let list = handle(ClientRequest::InocodeAccountsList {
            request_id: id(4),
            params: InocodeAccountsListParams { refresh: None },
        })
        .await
        .expect("list")
        .expect("payload");
        let json = serde_json::to_string(&list).expect("json");
        assert!(!json.contains("sk-test-FAKE"), "{json}");
        assert!(!json.contains('@'), "{json}");
        let value: serde_json::Value = serde_json::from_str(&json).expect("value");
        let accounts = value["accounts"].as_array().expect("accounts");
        assert_eq!(accounts.len(), 2);
        let used = accounts.iter().find(|a| a["alias"] == "改名").expect("renamed");
        assert_eq!(used["inUse"], true);
        assert_eq!(used["kind"], "apiKey");

        let policy = handle(ClientRequest::InocodeAccountsPolicyRead {
            request_id: id(5),
            params: InocodeAccountsPolicyReadParams { provider: None },
        })
        .await
        .expect("policy")
        .expect("payload");
        let policy = serde_json::to_value(&policy).expect("json");
        assert_eq!(policy["policy"]["thresholdPercent"], 90.0);
        assert_eq!(policy["policy"]["selection"], "quota");

        // Unknown ids are client errors, not crashes.
        assert!(
            handle(ClientRequest::InocodeAccountsRename {
                request_id: id(6),
                params: InocodeAccountsRenameParams {
                    id: "nope".to_string(),
                    alias: "x".to_string(),
                },
            })
            .await
            .is_err()
        );
        account_pool::install_global(None);
    }
}
