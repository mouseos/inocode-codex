//! Theos: the daily credit for the models that consume it. `GET {BASE}/theos` gives the balance;
//! its `check_in` (nonce, seconds) is kept and sent back on the next read.
//!
//! As in the original (DeepAI Code 0.3.35, `checkInLock`), the reads of `/theos` go out one at a
//! time within the process, and the check-in lives in memory only (a new process starts a new
//! one, as the original does on each launch).
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

#[derive(Clone, Debug, PartialEq)]
struct CheckIn {
    nonce: String,
    /// As the server wrote it (the original compares the number as it is, fractions included).
    seconds: f64,
    received: Instant,
}

impl CheckIn {
    fn is_valid(&self) -> bool {
        self.received.elapsed().as_secs_f64() < self.seconds
    }
}

#[derive(Default)]
struct Account {
    last_attempt: Option<Instant>,
    last_failed: bool,
    check_in: Option<CheckIn>,
    /// When a server last answered like one that has no check-in (an older lane).
    legacy_at: Option<Instant>,
    balance: Option<Balance>,
    last_error: Option<DeepAiError>,
}

pub struct TheosClient {
    base_url: String,
    /// `None`: read `deepai.clientVersion` at each beacon.
    version: Option<String>,
    started: Instant,
    accounts: Mutex<HashMap<String, Account>>,
    /// The original's `checkInLock` (one permit): one read of `/theos` at a time, so two calls
    /// never start or renew a check-in side by side.
    check_in_lock: tokio::sync::Mutex<()>,
}

/// An answer of the server: the result, and whether its body carried a check-in.
type Answer = (Result<Balance, DeepAiError>, bool);

impl TheosClient {
    pub fn new(base_url: &str, inocode_version: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            version: Some(inocode_version.to_string()),
            started: Instant::now(),
            accounts: Mutex::new(HashMap::new()),
            check_in_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Whether this key has a check-in that is still valid: a read then continues it instead of
    /// starting a new one.
    pub fn has_valid_check_in(&self, api_key: &str) -> bool {
        self.lock()
            .get(&crate::display_id(api_key))
            .and_then(|account| account.check_in.as_ref())
            .is_some_and(CheckIn::is_valid)
    }

    /// The endpoint in use: the one given, or (for the process-wide client) the real one, or the
    /// loopback test origin, looked up at each request.
    fn base(&self) -> String {
        if self.base_url.is_empty() {
            crate::base_url()
        } else {
            self.base_url.clone()
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

    /// The `X-DeepAI-Code-Beacon` value: `v=<version>;up=<seconds>;pi=<0|1>`. `focus` is left
    /// out because InoCode does not know whether a window is in front.
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
            .reduce(f64::max)
            .unwrap_or(600.0);
        LAST_MODEL_CALL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|at| at.elapsed().as_secs_f64() < window)
    }

    /// The earlier answer while the next read is not due yet: automatic reads are at least 30
    /// seconds apart (5 minutes after a failure), a read the user asks for (`manual`) 5 seconds.
    fn held(&self, id: &str, manual: bool) -> Option<Result<Balance, DeepAiError>> {
        let accounts = self.lock();
        let account = accounts.get(id);
        let interval = if manual {
            MIN_MANUAL_INTERVAL
        } else if account.is_some_and(|a| a.last_failed) {
            FAILURE_INTERVAL
        } else {
            MIN_INTERVAL
        };
        account
            .filter(|a| a.last_attempt.is_some_and(|t| t.elapsed() < interval))
            .map(|a| match (&a.balance, &a.last_error, a.last_failed) {
                (_, Some(error), true) => Err(error.clone()),
                (Some(balance), _, _) => Ok(balance.clone()),
                _ => Err(DeepAiError::unreachable("no earlier answer")),
            })
    }

    /// Reads the balance. Inside the interval (see `held`) the earlier answer is returned and
    /// nothing is sent. The read waits its turn behind any other read of `/theos`; a read that
    /// finished while this one waited is shared instead of sending another (the original's
    /// status read joins the one in flight).
    pub async fn read(&self, api_key: &str, manual: bool) -> Result<Balance, DeepAiError> {
        let id = crate::display_id(api_key);
        if let Some(answer) = self.held(&id, manual) {
            return answer;
        }
        let _turn = self.check_in_lock.lock().await;
        if let Some(answer) = self.held(&id, manual) {
            return answer;
        }
        self.request(api_key).await.0
    }

    /// Before every model call, the original client's `ensureCheckedIn`, run whole under the
    /// check-in lock: nothing to do while the check-in is younger than half of its interval (or
    /// the server is of an older lane); otherwise `/theos` is read, and the call goes ahead only
    /// when a valid check-in stands afterwards. A failed read still lets the call go ahead when
    /// the earlier check-in is valid and the failure is no answer at all, a 5xx, or an answer
    /// that carried a check-in. Calls that waited for the lock see the check-in the first one
    /// got and send nothing.
    pub async fn ensure_checked_in(&self, api_key: &str) -> Result<(), DeepAiError> {
        let _turn = self.check_in_lock.lock().await;
        let id = crate::display_id(api_key);
        let state = |client: &Self| {
            let accounts = client.lock();
            let account = accounts.get(&id);
            let check_in = account.and_then(|a| a.check_in.as_ref());
            (
                check_in.map(|c| (c.seconds, c.received.elapsed().as_secs_f64())),
                account
                    .and_then(|a| a.legacy_at)
                    .is_some_and(|at| at.elapsed() < Duration::from_secs(30)),
            )
        };
        let (check_in, legacy) = state(self);
        if legacy || check_in.is_some_and(|(seconds, age)| age < seconds / 2.0) {
            return Ok(());
        }
        let (result, carried_check_in) = self.request(api_key).await;
        let (check_in, legacy) = state(self);
        let valid_mark = check_in.is_some_and(|(seconds, age)| age < seconds);
        match result {
            Err(error) => {
                if valid_mark && (error.status == 0 || error.status >= 500 || carried_check_in) {
                    Ok(())
                } else {
                    Err(error)
                }
            }
            Ok(_) if valid_mark || legacy => Ok(()),
            Ok(_) => Err(DeepAiError::unreachable(
                "the server did not return a check-in",
            )),
        }
    }

    /// One `GET /theos` (no spacing check; callers hold the check-in lock). The state of the
    /// account follows the answer: the balance, the error, the check-in (from a success or a
    /// refusal), the older-lane mark.
    async fn request(&self, api_key: &str) -> Answer {
        let id = crate::display_id(api_key);
        let check_in = self.lock().get(&id).and_then(|a| a.check_in.clone());
        let (result, value) = self.send(api_key, check_in).await;
        let new_check_in = value.as_ref().and_then(parse_check_in);
        let carried = new_check_in.is_some();
        let legacy = new_check_in.is_none() && value.as_ref().is_some_and(is_older_lane);
        let mut accounts = self.lock();
        let account = accounts.entry(id).or_default();
        account.last_attempt = Some(Instant::now());
        if let Some(new_check_in) = new_check_in {
            account.check_in = Some(new_check_in);
        } else if legacy {
            account.legacy_at = Some(Instant::now());
        }
        match &result {
            Ok(balance) => {
                account.last_failed = false;
                account.last_error = None;
                account.balance = Some(balance.clone());
            }
            Err(error) => {
                account.last_failed = true;
                account.last_error = Some(error.clone());
            }
        }
        (result, carried)
    }

    async fn send(
        &self,
        api_key: &str,
        check_in: Option<CheckIn>,
    ) -> (Result<Balance, DeepAiError>, Option<Value>) {
        let client = match crate::http_client() {
            Ok(client) => client,
            Err(error) => return (Err(DeepAiError::unreachable(&error)), None),
        };
        let mut request = client
            .get(format!("{}/theos", self.base()))
            .timeout(Duration::from_secs(30))
            .header(crate::headers::API_KEY, api_key)
            .header("Content-Type", "application/json");
        for (name, value) in crate::headers::common(None) {
            request = request.header(name, value);
        }
        // The beacon goes with every read; the nonce only once the server has issued one.
        request = request.header(crate::headers::BEACON, self.beacon());
        if let Some(check_in) = &check_in {
            request = request.header(crate::headers::CHECK_IN, &check_in.nonce);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                return (
                    Err(DeepAiError::unreachable(&error.without_url().to_string())),
                    None,
                );
            }
        };
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        let value: Option<Value> = serde_json::from_str(&body).ok();
        if !(200..300).contains(&status) {
            return (Err(DeepAiError::from_response(status, &body)), value);
        }
        match &value {
            Some(value) => (Ok(parse_balance(value)), Some(value.clone())),
            None => (
                Err(DeepAiError::unreachable("the balance answer was not JSON")),
                None,
            ),
        }
    }
}

/// An answer that says the server is on an older lane or has the check-in switched off:
/// `enabled` is a boolean and it is false, or `lane_version` is missing or below this lane's.
fn is_older_lane(value: &Value) -> bool {
    let Some(enabled) = value.get("enabled").and_then(Value::as_bool) else {
        return false;
    };
    let lane: i64 = crate::LANE_VERSION.parse().unwrap_or(0);
    !enabled
        || match value.get("lane_version") {
            None => true,
            Some(version) => version.as_f64().is_some_and(|v| v < lane as f64),
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

/// `check_in: {nonce, seconds}` at the top level of the body. The nonce is 32 lower-case hex
/// digits and `seconds` a number above zero, as the original checks.
fn parse_check_in(value: &Value) -> Option<CheckIn> {
    let node = value.get("check_in")?;
    let nonce = node.get("nonce")?.as_str()?;
    let seconds = node.get("seconds")?.as_f64().filter(|s| *s > 0.0)?;
    let valid = nonce.len() == 32
        && nonce
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    valid.then(|| CheckIn {
        nonce: nonce.to_string(),
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

/// Formerly kept the check-ins in `dir/deepai-checkin.json` between runs. The original keeps the
/// check-in in memory only (a new process starts a new one), so this does nothing now; it stays
/// until its caller (`app-server/src/inocode/accounts.rs`) drops the call.
pub fn set_state_dir(_dir: &std::path::Path) {}

/// The process-wide client, aimed at the real endpoint.
pub fn global() -> &'static TheosClient {
    static CLIENT: OnceLock<TheosClient> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let mut client = TheosClient::new("", "");
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
                "check_in": {"nonce": "0123456789abcdef0123456789abcdef", "seconds": 60}
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

    #[tokio::test]
    async fn a_model_call_needs_a_valid_check_in_and_renews_it_only_past_half_of_its_interval() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/theos"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "enabled": true, "lane_version": 4, "balance": "1", "granted": "2",
                "check_in": {"nonce": "0123456789abcdef0123456789abcdef", "seconds": 600}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = TheosClient::new(&server.uri(), "1");
        client.ensure_checked_in("k").await.expect("first check-in");
        // Younger than half of 600 seconds: nothing is sent (the mock expects one request).
        client.ensure_checked_in("k").await.expect("still valid");
    }

    #[tokio::test]
    async fn without_a_check_in_the_call_is_refused_unless_the_server_is_of_an_older_lane() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/theos"))
            .and(header("api-key", "old"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"enabled": true, "balance": "1", "granted": "2"}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/theos"))
            .and(header("api-key", "silent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!(
                {"enabled": true, "lane_version": 4, "balance": "1", "granted": "2"}
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/theos"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": {"message": "Sign in", "type": "auth_required", "code": "auth_required"}
            })))
            .mount(&server)
            .await;
        let client = TheosClient::new(&server.uri(), "1");
        // No lane_version: an older lane, which has no check-in to wait for.
        client.ensure_checked_in("old").await.expect("older lane");
        // The current lane that issues no check-in: the call does not go ahead.
        let silent = client.ensure_checked_in("silent").await.expect_err("no check-in");
        assert_eq!(silent.status, 0);
        // A refusal is the refusal (the caller shows it, with its code and status).
        let refused = client.ensure_checked_in("bad").await.expect_err("refused");
        assert_eq!((refused.status, refused.code.as_deref()), (401, Some("auth_required")));
    }

    #[test]
    fn check_ins_and_older_lanes_are_read_as_the_original_reads_them() {
        let nonce = "0123456789abcdef0123456789abcdef";
        let ok = serde_json::json!({"check_in": {"nonce": nonce, "seconds": 90}});
        assert!(parse_check_in(&ok).is_some());
        for bad in [
            serde_json::json!({"check_in": {"nonce": "ABCDEF0123456789ABCDEF0123456789", "seconds": 90}}),
            serde_json::json!({"check_in": {"nonce": "abc", "seconds": 90}}),
            serde_json::json!({"check_in": {"nonce": nonce, "seconds": 0}}),
            serde_json::json!({"check_in": {"nonce": nonce}}),
            serde_json::json!({"error": {"check_in": {"nonce": nonce, "seconds": 90}}}),
        ] {
            assert!(parse_check_in(&bad).is_none(), "{bad}");
        }
        assert!(is_older_lane(&serde_json::json!({"enabled": false, "lane_version": 4})));
        assert!(is_older_lane(&serde_json::json!({"enabled": true})));
        assert!(is_older_lane(&serde_json::json!({"enabled": true, "lane_version": 3})));
        assert!(!is_older_lane(&serde_json::json!({"enabled": true, "lane_version": 4})));
        assert!(!is_older_lane(&serde_json::json!({"balance": "1"})));
    }

    /// Answers `/theos` with a check-in after `DELAY`, and notes when each request came in.
    struct Recorder {
        arrivals: std::sync::Arc<Mutex<Vec<Instant>>>,
    }

    const DELAY: Duration = Duration::from_millis(200);

    impl wiremock::Respond for Recorder {
        fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
            self.arrivals
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(Instant::now());
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "enabled": true, "lane_version": 4, "balance": "1", "granted": "2",
                    "check_in": {"nonce": "0123456789abcdef0123456789abcdef", "seconds": 600}
                }))
                .set_delay(DELAY)
        }
    }

    async fn recording_server() -> (MockServer, std::sync::Arc<Mutex<Vec<Instant>>>) {
        let server = MockServer::start().await;
        let arrivals = std::sync::Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("GET"))
            .and(path("/theos"))
            .respond_with(Recorder {
                arrivals: arrivals.clone(),
            })
            .mount(&server)
            .await;
        (server, arrivals)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn simultaneous_model_calls_start_one_check_in_and_the_rest_reuse_it() {
        let (server, arrivals) = recording_server().await;
        let client = std::sync::Arc::new(TheosClient::new(&server.uri(), "1"));
        let calls: Vec<_> = (0..8)
            .map(|_| {
                let client = client.clone();
                tokio::spawn(async move { client.ensure_checked_in("k").await })
            })
            .collect();
        for call in calls {
            call.await.expect("task").expect("checked in");
        }
        assert_eq!(server.received_requests().await.expect("recorded").len(), 1);
        let first = &server.received_requests().await.expect("recorded")[0];
        assert!(
            !first.headers.contains_key("X-DeepAI-Code-Check-In"),
            "the first check-in has no nonce to send"
        );
        assert_eq!(arrivals.lock().expect("arrivals").len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reads_of_theos_go_out_one_at_a_time_even_for_different_keys_and_callers() {
        let (server, arrivals) = recording_server().await;
        let client = std::sync::Arc::new(TheosClient::new(&server.uri(), "1"));
        let mut calls = Vec::new();
        for n in 0..3 {
            let model = client.clone();
            calls.push(tokio::spawn(async move {
                model.ensure_checked_in(&format!("m{n}")).await.map(|_| ())
            }));
            let status = client.clone();
            calls.push(tokio::spawn(async move {
                status.read(&format!("s{n}"), false).await.map(|_| ())
            }));
        }
        for call in calls {
            call.await.expect("task").expect("answered");
        }
        let mut arrivals = arrivals.lock().expect("arrivals").clone();
        assert_eq!(arrivals.len(), 6, "one read per key");
        arrivals.sort();
        for pair in arrivals.windows(2) {
            // Each read starts only after the previous answer (which takes DELAY) came back.
            assert!(
                pair[1] - pair[0] >= DELAY - Duration::from_millis(20),
                "two reads overlapped: {:?} apart",
                pair[1] - pair[0]
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_status_read_that_waited_behind_another_shares_its_answer() {
        let (server, _arrivals) = recording_server().await;
        let client = std::sync::Arc::new(TheosClient::new(&server.uri(), "1"));
        let calls: Vec<_> = (0..4)
            .map(|_| {
                let client = client.clone();
                tokio::spawn(async move { client.read("k", false).await })
            })
            .collect();
        for call in calls {
            call.await.expect("task").expect("balance");
        }
        assert_eq!(server.received_requests().await.expect("recorded").len(), 1);
        assert!(client.has_valid_check_in("k"));
    }

    #[test]
    fn the_interval_is_kept_as_the_server_wrote_it() {
        let value = serde_json::json!({"check_in": {"nonce": "0123456789abcdef0123456789abcdef", "seconds": 0.5}});
        let check_in = parse_check_in(&value).expect("check-in");
        assert_eq!(check_in.seconds, 0.5);
        std::thread::sleep(Duration::from_millis(600));
        assert!(!check_in.is_valid(), "half a second has passed");
    }

    #[test]
    fn amounts_must_have_the_documented_shape() {
        let balance = parse_balance(&serde_json::json!({"enabled": true, "balance": "x", "granted": 200}));
        assert_eq!(balance.balance, None);
        assert_eq!(balance.granted.as_deref(), Some("200"));
    }
}
