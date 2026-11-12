//! Theos: the daily credit for the models that consume it. `GET {BASE}/theos` gives the balance;
//! its `check_in` (nonce, seconds) is kept and sent back on the next read.
//!
//! The beacon sent with the check-in is `v=<version>;up=<seconds>;pi=<0|1>`. `up` is the seconds
//! this process has been running and `pi` is whether a model request went out within the
//! check-in's interval, both real values. `focus` (the window being in front) is added only when
//! the host knows it, and InoCode does not, so it is left out. `v` is `deepai.clientVersion`:
//! DEEPAI_CODE_VERSION (the DeepAI Code version the official spec names) unless it is set (`set_client_version` or the environment variable
//! `INOCODE_DEEPAI_CLIENT_VERSION`). If the server does not accept the beacon, the error is
//! shown as it is.

use crate::errors::DeepAiError;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

static LAST_MODEL_CALL: Mutex<Option<Instant>> = Mutex::new(None);

/// Records that a model request went out (the `pi` part of the beacon).
pub fn note_model_call() {
    *LAST_MODEL_CALL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
}

/// The `v=` of the beacon by default: the DeepAI Code version of the official spec (the user decided
/// to follow the spec and take responsibility for it; change it with `deepai.clientVersion`).
const DEEPAI_CODE_VERSION: &str = "0.3.35";

static CLIENT_VERSION: Mutex<Option<String>> = Mutex::new(None);

/// Sets `deepai.clientVersion`, the `v=` of the beacon.
pub fn set_client_version(version: &str) {
    *CLIENT_VERSION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(version.to_string());
}

fn client_version() -> String {
    CLIENT_VERSION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .or_else(|| std::env::var("INOCODE_DEEPAI_CLIENT_VERSION").ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_graphic() && b != b';'))
        .unwrap_or_else(|| DEEPAI_CODE_VERSION.to_string())
}

/// A read of the balance is repeated at the earliest after this long.
const MIN_INTERVAL: Duration = Duration::from_secs(30);
/// A read the user asks for is repeated at the earliest after this long.
const MIN_MANUAL_INTERVAL: Duration = Duration::from_secs(5);
/// After a failed read, the next automatic read waits this long.
const FAILURE_INTERVAL: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Balance {
    pub enabled: bool,
    /// What is left; a number as text (`^\d+(\.\d+)?$`).
    pub balance: Option<String>,
    /// What the day grants.
    pub granted: Option<String>,
    /// When the allowance comes back, as the server wrote it.
    pub refresh_at: Option<String>,
    pub lane_version: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CheckIn {
    nonce: String,
    seconds: u64,
    received: Instant,
}

#[derive(Default)]
struct Account {
    last_attempt: Option<Instant>,
    last_failed: bool,
    check_in: Option<CheckIn>,
    balance: Option<Balance>,
    last_error: Option<DeepAiError>,
}

pub struct TheosClient {
    base_url: String,
    /// `None`: read `deepai.clientVersion` at each beacon.
    version: Option<String>,
    started: Instant,
    accounts: Mutex<HashMap<String, Account>>,
}

impl TheosClient {
    pub fn new(base_url: &str, inocode_version: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            version: Some(inocode_version.to_string()),
            started: Instant::now(),
            accounts: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Account>> {
        self.accounts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The balance last read for this key, without any request.
    pub fn cached(&self, api_key: &str) -> Option<Balance> {
        self.lock()
            .get(&crate::display_id(api_key))
            .and_then(|account| account.balance.clone())
    }

    /// The error of the last failed read for this key, if the last read failed.
    pub fn last_error(&self, api_key: &str) -> Option<DeepAiError> {
        self.lock()
            .get(&crate::display_id(api_key))
            .filter(|account| account.last_failed)
            .and_then(|account| account.last_error.clone())
    }

    /// The `X-DeepAI-Code-Beacon` value.
    pub fn beacon(&self) -> String {
        format!(
            "v={};up={};pi={}",
            self.version.clone().unwrap_or_else(client_version),
            self.started.elapsed().as_secs(),
            u8::from(self.recent_model_call())
        )
    }

    /// A model request went out within the last check-in interval (600 seconds when none is
    /// known yet).
    fn recent_model_call(&self) -> bool {
        let window = self
            .lock()
            .values()
            .filter_map(|account| account.check_in.as_ref().map(|c| c.seconds))
            .max()
            .unwrap_or(600);
        LAST_MODEL_CALL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|at| at.elapsed() < Duration::from_secs(window))
    }

    /// Reads the balance. Automatic reads are at least 30 seconds apart (5 minutes after a
    /// failure); a read the user asks for (`manual`) is at least 5 seconds apart. Inside the
    /// interval the earlier answer is returned and nothing is sent.
    pub async fn read(&self, api_key: &str, manual: bool) -> Result<Balance, DeepAiError> {
        let id = crate::display_id(api_key);
        let (check_in, held) = {
            let accounts = self.lock();
            let account = accounts.get(&id);
            let interval = if manual {
                MIN_MANUAL_INTERVAL
            } else if account.is_some_and(|a| a.last_failed) {
                FAILURE_INTERVAL
            } else {
                MIN_INTERVAL
            };
            let held = account
                .filter(|a| a.last_attempt.is_some_and(|t| t.elapsed() < interval))
                .map(|a| match (&a.balance, &a.last_error, a.last_failed) {
                    (_, Some(error), true) => Err(error.clone()),
                    (Some(balance), _, _) => Ok(balance.clone()),
                    _ => Err(DeepAiError::unreachable("no earlier answer")),
                });
            (account.and_then(|a| a.check_in.clone()), held)
        };
        if let Some(answer) = held {
            return answer;
        }
        let result = self.send(api_key, check_in).await;
        let mut accounts = self.lock();
        let account = accounts.entry(id).or_default();
        account.last_attempt = Some(Instant::now());
        match &result {
            Ok((balance, new_check_in)) => {
                account.last_failed = false;
                account.last_error = None;
                account.balance = Some(balance.clone());
                if let Some(new_check_in) = new_check_in {
                    account.check_in = Some(new_check_in.clone());
                }
            }
            Err((error, new_check_in)) => {
                account.last_failed = true;
                account.last_error = Some(error.clone());
                if let Some(new_check_in) = new_check_in {
                    account.check_in = Some(new_check_in.clone());
                }
            }
        }
        result.map(|(balance, _)| balance).map_err(|(error, _)| error)
    }

    /// Before a model call: reads again when the check-in is past half of its interval.
    pub async fn ensure_checked_in(&self, api_key: &str) {
        let due = {
            let accounts = self.lock();
            accounts
                .get(&crate::display_id(api_key))
                .and_then(|account| account.check_in.as_ref())
                .is_some_and(|check_in| {
                    check_in.received.elapsed() > Duration::from_secs(check_in.seconds / 2)
                })
        };
        if due {
            let _ = self.read(api_key, /*manual*/ false).await;
        }
    }

    async fn send(
        &self,
        api_key: &str,
        check_in: Option<CheckIn>,
    ) -> Result<(Balance, Option<CheckIn>), (DeepAiError, Option<CheckIn>)> {
        let client = crate::http_client().map_err(|e| (DeepAiError::unreachable(&e), None))?;
        let mut request = client
            .get(format!("{}/theos", self.base_url))
            .header(crate::headers::API_KEY, api_key)
            .header("Content-Type", "application/json");
        for (name, value) in crate::headers::common(None) {
            request = request.header(name, value);
        }
        if let Some(check_in) = &check_in {
            request = request
                .header(crate::headers::CHECK_IN, &check_in.nonce)
                .header(crate::headers::BEACON, self.beacon());
        }
        let response = request.send().await.map_err(|error| {
            (
                DeepAiError::unreachable(&error.without_url().to_string()),
                None,
            )
        })?;
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        let value: Option<Value> = serde_json::from_str(&body).ok();
        let new_check_in = value.as_ref().and_then(parse_check_in);
        if !(200..300).contains(&status) {
            return Err((DeepAiError::from_response(status, &body), new_check_in));
        }
        let Some(value) = value else {
            return Err((
                DeepAiError::unreachable("the balance answer was not JSON"),
                new_check_in,
            ));
        };
        Ok((parse_balance(&value), new_check_in))
    }
}

fn amount(value: &Value, name: &str) -> Option<String> {
    let text = match value.get(name)? {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => return None,
    };
    crate::catalog::theos_number(&text).map(|_| text)
}

pub(crate) fn parse_balance(value: &Value) -> Balance {
    Balance {
        enabled: value.get("enabled").and_then(Value::as_bool).unwrap_or(false),
        balance: amount(value, "balance"),
        granted: amount(value, "granted"),
        refresh_at: value
            .get("refresh_at")
            .and_then(Value::as_str)
            .map(str::to_string),
        lane_version: value.get("lane_version").and_then(Value::as_i64),
    }
}

/// `check_in: {nonce, seconds}` at the top level of the body, or inside `error`.
fn parse_check_in(value: &Value) -> Option<CheckIn> {
    let node = value
        .get("check_in")
        .or_else(|| value.get("error").and_then(|e| e.get("check_in")))?;
    let nonce = node.get("nonce")?.as_str()?.to_string();
    let seconds = node.get("seconds").and_then(Value::as_u64)?;
    (!nonce.is_empty()).then(|| CheckIn {
        nonce,
        seconds,
        received: Instant::now(),
    })
}

/// Whether the account is on Pro, from `GET {origin}/get_usage_summary` (the API key goes in the
/// header). Only an answer with `is_authenticated: true` and a boolean `is_pro` counts; anything
/// else is `None` (unknown), never a guess.
pub async fn is_pro(origin: &str, api_key: &str) -> Option<bool> {
    let client = crate::http_client().ok()?;
    let mut request = client
        .get(format!("{}/get_usage_summary", origin.trim_end_matches('/')))
        .header(crate::headers::API_KEY, api_key);
    for (name, value) in crate::headers::common(None) {
        request = request.header(name, value);
    }
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let value: Value = serde_json::from_str(&response.text().await.ok()?).ok()?;
    if value.get("is_authenticated").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    value.get("is_pro").and_then(Value::as_bool)
}

/// The process-wide client, aimed at the real endpoint.
pub fn global() -> &'static TheosClient {
    static CLIENT: OnceLock<TheosClient> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let mut client = TheosClient::new(&crate::base_url(), "");
        client.version = None;
        client
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::header;
    use wiremock::matchers::header_exists;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    #[tokio::test]
    async fn a_read_sends_the_key_and_lane_and_returns_the_balance() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/theos"))
            .and(header("api-key", "k-1"))
            .and(header("X-DeepAI-Code-Theos", "4"))
            .and(header_exists("X-DeepAI-Code-Instance"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "enabled": true, "balance": "150.5", "granted": "200", "refresh_at": "2026-11-10T00:00:00Z",
                "lane_version": 4
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = TheosClient::new(&server.uri(), "9.9.9");
        let balance = client.read("k-1", false).await.expect("balance");
        assert_eq!(balance.balance.as_deref(), Some("150.5"));
        assert_eq!(balance.granted.as_deref(), Some("200"));
        assert!(balance.enabled);
        // Inside 30 seconds nothing is sent again (the mock expects exactly one request).
        let again = client.read("k-1", false).await.expect("cached");
        assert_eq!(again, balance);
        assert_eq!(client.cached("k-1"), Some(balance));
    }

    #[tokio::test]
    async fn the_check_in_is_kept_and_returned_with_a_beacon_of_facts() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/theos"))
            .and(wiremock::matchers::header_exists("X-DeepAI-Code-Check-In"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "enabled": true, "balance": "1", "granted": "2"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/theos"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "enabled": true, "balance": "1", "granted": "2",
                "check_in": {"nonce": "0123456789abcdef0123456789abcdef", "seconds": 600}
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        let client = TheosClient::new(&server.uri(), "1.2.3");
        client.read("k", false).await.expect("first");
        // A manual read after the 5 second interval would be needed; reuse the state directly.
        {
            let mut accounts = client.lock();
            let account = accounts.get_mut(&crate::display_id("k")).expect("account");
            account.last_attempt = None;
        }
        client.read("k", true).await.expect("second carries the check-in");
        let beacon = client.beacon();
        assert!(beacon.starts_with("v=1.2.3;up="), "{beacon}");
        assert!(beacon.ends_with(";pi=0") || beacon.ends_with(";pi=1"), "{beacon}");
        assert!(!beacon.contains("focus"), "focus is only sent when it is known");
    }

    #[tokio::test]
    async fn an_error_answer_keeps_status_and_code_and_may_carry_a_check_in() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/theos"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": {"message": "Sign in", "type": "auth_required", "code": "auth_required"},
                "check_in": {"nonce": "abc", "seconds": 60}
            })))
            .mount(&server)
            .await;
        let client = TheosClient::new(&server.uri(), "1");
        let error = client.read("bad", false).await.expect_err("refused");
        assert_eq!(error.status, 401);
        assert_eq!(error.code.as_deref(), Some("auth_required"));
        assert_eq!(client.last_error("bad"), Some(error));
        let accounts = client.lock();
        assert!(accounts[&crate::display_id("bad")].check_in.is_some());
    }

    #[tokio::test]
    async fn pro_is_known_only_from_an_authenticated_answer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/get_usage_summary"))
            .and(header("api-key", "good"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"is_authenticated": true, "is_pro": true}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/get_usage_summary"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"is_authenticated": false, "is_pro": false}),
            ))
            .mount(&server)
            .await;
        assert_eq!(is_pro(&server.uri(), "good").await, Some(true));
        assert_eq!(is_pro(&server.uri(), "other").await, None);
    }

    #[test]
    fn amounts_must_have_the_documented_shape() {
        let balance = parse_balance(&serde_json::json!({"enabled": true, "balance": "x", "granted": 200}));
        assert_eq!(balance.balance, None);
        assert_eq!(balance.granted.as_deref(), Some("200"));
    }
}
