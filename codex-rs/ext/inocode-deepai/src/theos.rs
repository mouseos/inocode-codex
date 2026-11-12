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
    /// Where the check-ins are kept between runs (`deepai-checkin.json`), once it is set.
    state_file: Mutex<Option<std::path::PathBuf>>,
    loaded: std::sync::atomic::AtomicBool,
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
            state_file: Mutex::new(None),
            loaded: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Keeps the check-ins in `file` between runs. A check-in that is still valid when the app
    /// starts again is carried on instead of starting a new one: the server counts how often a
    /// check-in is started (a new one without a valid nonce) per day and refuses past a limit
    /// (HTTP 403 `check_in_refused`), so a restart must not start another one needlessly.
    pub fn set_state_file(&self, file: std::path::PathBuf) {
        *self
            .state_file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(file);
    }

    fn state_path(&self) -> Option<std::path::PathBuf> {
        self.state_file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Reads the kept check-ins once (those still valid).
    fn load_once(&self) {
        use std::sync::atomic::Ordering;
        let Some(path) = self.state_path() else {
            return;
        };
        if self.loaded.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut accounts = self.lock();
        for (id, entry) in map {
            let nonce = entry.get("nonce").and_then(Value::as_str).unwrap_or_default();
            let seconds = entry.get("seconds").and_then(Value::as_u64).unwrap_or(0);
            let at = entry.get("receivedAt").and_then(Value::as_u64).unwrap_or(0);
            let age = now.saturating_sub(at);
            if nonce.len() != 32 || seconds == 0 || age >= seconds {
                continue;
            }
            let account = accounts.entry(id).or_default();
            if account.check_in.is_none() {
                account.check_in = Some(CheckIn {
                    nonce: nonce.to_string(),
                    seconds,
                    received: Instant::now()
                        .checked_sub(Duration::from_secs(age))
                        .unwrap_or_else(Instant::now),
                });
            }
        }
    }

    fn save(&self) {
        let Some(path) = self.state_path() else {
            return;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let entries: serde_json::Map<String, Value> = self
            .lock()
            .iter()
            .filter_map(|(id, account)| {
                let check_in = account.check_in.as_ref()?;
                Some((
                    id.clone(),
                    serde_json::json!({
                        "nonce": check_in.nonce,
                        "seconds": check_in.seconds,
                        "receivedAt": now.saturating_sub(check_in.received.elapsed().as_secs()),
                    }),
                ))
            })
            .collect();
        if let Ok(text) = serde_json::to_string(&Value::Object(entries)) {
            let temp = path.with_extension("json.tmp");
            if std::fs::write(&temp, text).is_ok() {
                let _ = std::fs::rename(&temp, &path);
            }
        }
    }

    /// Whether this key has a check-in that is still valid: a read then continues it instead of
    /// starting a new one.
    pub fn has_valid_check_in(&self, api_key: &str) -> bool {
        self.load_once();
        self.lock()
            .get(&crate::display_id(api_key))
            .and_then(|account| account.check_in.as_ref())
            .is_some_and(|check_in| check_in.received.elapsed().as_secs() < check_in.seconds)
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
        self.load_once();
        let id = crate::display_id(api_key);
        let held = {
            let accounts = self.lock();
            let account = accounts.get(&id);
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
        };
        match held {
            Some(answer) => answer,
            None => self.request(api_key).await.0,
        }
    }

    /// Before every model call, the original client's `ensureCheckedIn`: nothing to do while the
    /// check-in is younger than half of its interval (or the server is of an older lane);
    /// otherwise `/theos` is read, and the call goes ahead only when a valid check-in stands
    /// afterwards. A failed read still lets the call go ahead when the earlier check-in is valid
    /// and the failure is no answer at all, a 5xx, or an answer that carried a check-in.
    pub async fn ensure_checked_in(&self, api_key: &str) -> Result<(), DeepAiError> {
        self.load_once();
        let id = crate::display_id(api_key);
        let state = |client: &Self| {
            let accounts = client.lock();
            let account = accounts.get(&id);
            let check_in = account.and_then(|a| a.check_in.as_ref());
            (
                check_in.map(|c| (c.seconds, c.received.elapsed())),
                account
                    .and_then(|a| a.legacy_at)
                    .is_some_and(|at| at.elapsed() < Duration::from_secs(30)),
            )
        };
        let (check_in, legacy) = state(self);
        if legacy
            || check_in.is_some_and(|(seconds, age)| age.as_secs_f64() < seconds as f64 / 2.0)
        {
            return Ok(());
        }
        let (result, carried_check_in) = self.request(api_key).await;
        let (check_in, legacy) = state(self);
        let valid_mark = check_in.is_some_and(|(seconds, age)| age.as_secs() < seconds);
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

    /// One `GET /theos` (no spacing check). The state of the account follows the answer: the
    /// balance, the error, the check-in (from a success or a refusal), the older-lane mark.
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
        let kept = new_check_in.is_some();
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
        drop(accounts);
        if kept {
            self.save();
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
        seconds: seconds.ceil() as u64,
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

/// Keeps the check-ins under `dir` (`deepai-checkin.json`) between runs; call once at start-up.
pub fn set_state_dir(dir: &std::path::Path) {
    global().set_state_file(dir.join("deepai-checkin.json"));
}

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

    #[tokio::test]
    async fn a_valid_check_in_survives_a_restart_and_is_carried_on_instead_of_starting_a_new_one() {
        let server = MockServer::start().await;
        let nonce = "0123456789abcdef0123456789abcdef";
        // The second client must present the first one's nonce (no new check-in is started).
        Mock::given(method("GET"))
            .and(path("/theos"))
            .and(header("X-DeepAI-Code-Check-In", nonce))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "enabled": true, "lane_version": 4, "balance": "1", "granted": "2",
                "check_in": {"nonce": "fedcba9876543210fedcba9876543210", "seconds": 600}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/theos"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "enabled": true, "lane_version": 4, "balance": "1", "granted": "2",
                "check_in": {"nonce": nonce, "seconds": 600}
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("dir");
        let file = dir.path().join("deepai-checkin.json");
        let first = TheosClient::new(&server.uri(), "1");
        first.set_state_file(file.clone());
        assert!(!first.has_valid_check_in("k"));
        first.read("k", true).await.expect("first run");
        assert!(file.exists(), "the check-in is kept");
        assert!(!std::fs::read_to_string(&file).expect("text").contains("\"k\""), "no key in the file");
        // A new process: it knows the check-in and continues it.
        let second = TheosClient::new(&server.uri(), "1");
        second.set_state_file(file.clone());
        assert!(second.has_valid_check_in("k"));
        second.read("k", true).await.expect("second run carries the nonce");
        // An expired check-in is not carried on.
        std::fs::write(
            &file,
            serde_json::json!({crate::display_id("k"): {"nonce": nonce, "seconds": 600, "receivedAt": 1}}).to_string(),
        )
        .expect("write");
        let third = TheosClient::new(&server.uri(), "1");
        third.set_state_file(file);
        assert!(!third.has_valid_check_in("k"));
    }

    #[test]
    fn amounts_must_have_the_documented_shape() {
        let balance = parse_balance(&serde_json::json!({"enabled": true, "balance": "x", "granted": 200}));
        assert_eq!(balance.balance, None);
        assert_eq!(balance.granted.as_deref(), Some("200"));
    }
}
