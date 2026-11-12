//! The sign-in: a loopback redirect that hands over a short-lived code, which is exchanged for a
//! long-lived API key (RFC 8252 in shape; not OAuth 2.0 — there is no client id and no PKCE).
//!
//! 1. A random `state` (32 bytes, 64 hex characters).
//! 2. A listener on 127.0.0.1, any free port, waiting at `/auth-callback`.
//! 3. The browser opens `{BASE}/desktop_auth/?state=..&port=..`.
//! 4. The callback must be `GET /auth-callback`, carry the same `state` (compared in constant
//!    time) and a `code` of at most 4096 characters.
//! 5. `POST {BASE}/desktop_auth/exchange/ {"code"}` answers `{api_key, username, is_pro}`.
//!
//! The state, the code and the key never reach a log or an error text. What step 5 does on the
//! server side after a login in the browser is not confirmed (the spec lists it as open).

use crate::errors::DeepAiError;
use serde_json::Value;
use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

const CALLBACK_PATH: &str = "/auth-callback";
const MAX_CODE_LEN: usize = 4096;

/// A sign-in that has begun: the URL to open and the listener that waits for the redirect.
pub struct Pending {
    pub url: String,
    state: String,
    listener: CallbackListener,
}

/// The result of the exchange. `api_key` is a secret: it goes into the account pool and nowhere
/// else.
pub struct Exchanged {
    pub api_key: String,
    pub username: String,
    pub is_pro: bool,
}

impl std::fmt::Debug for Exchanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Exchanged")
            .field("api_key", &"<hidden>")
            .field("username", &self.username)
            .field("is_pro", &self.is_pro)
            .finish()
    }
}

pub fn new_state() -> String {
    let bytes: [u8; 32] = rand::random();
    crate::hex(&bytes)
}

/// Comparison whose time does not depend on where the strings first differ.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut difference = a.len() ^ b.len();
    for index in 0..a.len().max(b.len()) {
        difference |= usize::from(a.get(index).copied().unwrap_or(0) ^ b.get(index).copied().unwrap_or(0));
    }
    difference == 0
}

pub fn sign_in_url(base_url: &str, state: &str, port: u16) -> String {
    format!(
        "{}/desktop_auth/?state={state}&port={port}",
        base_url.trim_end_matches('/')
    )
}

/// Binds the listener and builds the URL. Dropping the result closes the port.
pub async fn begin(base_url: &str) -> io::Result<Pending> {
    let listener = CallbackListener::bind().await?;
    let port = listener
        .port()
        .ok_or_else(|| io::Error::other("the sign-in listener has no port"))?;
    let state = new_state();
    Ok(Pending {
        url: sign_in_url(base_url, &state, port),
        state,
        listener,
    })
}

impl Pending {
    /// Waits for the redirect and returns the code. `cancel` ends the wait early.
    pub async fn wait_for_code(self, timeout: Duration, cancel: Arc<AtomicBool>) -> io::Result<String> {
        self.listener.wait(self.state, timeout, cancel).await
    }
}

struct CallbackListener {
    server: tiny_http::Server,
}

impl CallbackListener {
    async fn bind() -> io::Result<Self> {
        tokio::task::spawn_blocking(|| {
            tiny_http::Server::http(("127.0.0.1", 0))
                .map(|server| Self { server })
                .map_err(|error| io::Error::other(format!("cannot listen for the sign-in: {error}")))
        })
        .await
        .map_err(io::Error::other)?
    }

    fn port(&self) -> Option<u16> {
        self.server.server_addr().to_ip().map(|addr| addr.port())
    }

    async fn wait(self, state: String, timeout: Duration, cancel: Arc<AtomicBool>) -> io::Result<String> {
        tokio::task::spawn_blocking(move || {
            let server = self.server;
            let deadline = Instant::now() + timeout;
            loop {
                if cancel.load(Ordering::SeqCst) {
                    return Err(io::Error::other("the sign-in was cancelled"));
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::other("the sign-in timed out"));
                }
                let Some(request) = server
                    .recv_timeout(Duration::from_millis(250))
                    .map_err(io::Error::other)?
                else {
                    continue;
                };
                let method_is_get = *request.method() == tiny_http::Method::Get;
                let url = url::Url::parse(&format!("http://127.0.0.1{}", request.url())).ok();
                let is_callback = method_is_get
                    && url.as_ref().is_some_and(|u| u.path() == CALLBACK_PATH);
                let pair = |name: &str| {
                    url.as_ref().and_then(|u| {
                        u.query_pairs()
                            .find(|(key, _)| key == name)
                            .map(|(_, value)| value.into_owned())
                    })
                };
                let page = |status: u16, body: &str| {
                    let response = tiny_http::Response::from_string(body.to_string())
                        .with_status_code(status)
                        .with_header(
                            tiny_http::Header::from_bytes(
                                &b"Content-Type"[..],
                                &b"text/html; charset=utf-8"[..],
                            )
                            .expect("a constant header parses"),
                        );
                    let _ = request.respond(response);
                };
                if !is_callback {
                    page(400, "Not a sign-in callback.");
                    continue;
                }
                let state_ok = pair("state").is_some_and(|got| constant_time_eq(&got, &state));
                let code = pair("code").filter(|code| !code.is_empty() && code.len() <= MAX_CODE_LEN);
                match (state_ok, code) {
                    (true, Some(code)) => {
                        page(200, "Signed in to InoCode. You can close this tab.");
                        return Ok(code);
                    }
                    _ => page(400, "The sign-in answer was not valid. Start again."),
                }
            }
        })
        .await
        .map_err(io::Error::other)?
    }
}

/// Exchanges the code for the key: `POST {base_url}/desktop_auth/exchange/`. Only a JSON content
/// type goes with it (no lane headers, no key).
pub async fn exchange(base_url: &str, code: &str) -> Result<Exchanged, DeepAiError> {
    let client = crate::http_client().map_err(|e| DeepAiError::unreachable(&e))?;
    let response = client
        .post(format!("{}/desktop_auth/exchange/", base_url.trim_end_matches('/')))
        .header("Content-Type", "application/json")
        .body(serde_json::json!({ "code": code }).to_string())
        .send()
        .await
        .map_err(|error| DeepAiError::unreachable(&error.without_url().to_string()))?;
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(exchange_error(status, &body));
    }
    parse_exchange(&body)
}

/// Exchange failures: 400 and 401 mean the link expired or was used; the flat body is kept.
fn exchange_error(status: u16, body: &str) -> DeepAiError {
    let mut error = DeepAiError::from_response(status, body);
    if matches!(status, 400 | 401) && error.code.is_none() {
        error.code = Some("exchange_rejected".to_string());
    }
    error
}

pub(crate) fn parse_exchange(body: &str) -> Result<Exchanged, DeepAiError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| DeepAiError::unreachable("the sign-in answer was not JSON"))?;
    let api_key = value
        .get("api_key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty() && !key.contains(['\n', '\r']))
        .ok_or_else(|| DeepAiError::unreachable("the sign-in answer had no usable API key"))?
        .to_string();
    Ok(Exchanged {
        api_key,
        username: value
            .get("username")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        is_pro: value.get("is_pro").and_then(Value::as_bool) == Some(true),
    })
}

/// What the user is told when the exchange was refused with 400 or 401.
pub fn exchange_failure_text(error: &DeepAiError) -> String {
    if matches!(error.status, 400 | 401) {
        format!(
            "サインインのリンクが期限切れか、すでに使われています。最初からやり直してください。 (HTTP {})",
            error.status
        )
    } else if error.status == 429 {
        "サインインの試行が多すぎます。しばらく待ってからやり直してください。 (HTTP 429)".to_string()
    } else {
        error.user_message()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::body_json;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    #[test]
    fn the_state_is_32_random_bytes_in_hex() {
        let state = new_state();
        assert_eq!(state.len(), 64);
        assert_ne!(state, new_state());
        assert!(constant_time_eq(&state, &state.clone()));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("", "a"));
    }

    #[test]
    fn the_sign_in_url_names_state_and_port() {
        assert_eq!(
            sign_in_url("https://api.deepai.org/deepai_code/", "ab", 5000),
            "https://api.deepai.org/deepai_code/desktop_auth/?state=ab&port=5000"
        );
    }

    async fn get(port: u16, target: String) -> u16 {
        tokio::task::spawn_blocking(move || {
            use std::io::Read;
            use std::io::Write;
            let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
            write!(stream, "GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .expect("write");
            let mut text = String::new();
            let _ = stream.read_to_string(&mut text);
            text.split_whitespace()
                .nth(1)
                .and_then(|status| status.parse().ok())
                .unwrap_or(0)
        })
        .await
        .expect("join")
    }

    #[tokio::test]
    async fn only_the_right_state_with_a_code_ends_the_wait() {
        let pending = begin("https://example.test/deepai_code").await.expect("begin");
        let port = pending.listener.port().expect("port");
        let state = pending.state.clone();
        let waiting = tokio::spawn(pending.wait_for_code(
            Duration::from_secs(20),
            Arc::new(AtomicBool::new(false)),
        ));
        assert_eq!(get(port, "/favicon.ico".to_string()).await, 400);
        assert_eq!(get(port, "/auth-callback?code=c&state=wrong".to_string()).await, 400);
        assert_eq!(get(port, format!("/auth-callback?state={state}")).await, 400);
        let long = "a".repeat(4097);
        assert_eq!(get(port, format!("/auth-callback?code={long}&state={state}")).await, 400);
        assert!(!waiting.is_finished(), "a bad callback must not end the wait");
        assert_eq!(get(port, format!("/auth-callback?code=good&state={state}")).await, 200);
        assert_eq!(waiting.await.expect("join").expect("code"), "good");
    }

    #[tokio::test]
    async fn cancel_ends_the_wait() {
        let pending = begin("https://example.test/deepai_code").await.expect("begin");
        let cancel = Arc::new(AtomicBool::new(true));
        assert!(pending.wait_for_code(Duration::from_secs(5), cancel).await.is_err());
    }

    #[tokio::test]
    async fn the_exchange_returns_the_key_and_the_documented_failures_are_read() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/desktop_auth/exchange/"))
            .and(body_json(serde_json::json!({"code": "good"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "api_key": "secret-key", "username": "yuma", "is_pro": true
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/desktop_auth/exchange/"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"error": "invalid or expired code"})),
            )
            .mount(&server)
            .await;
        let got = exchange(&server.uri(), "good").await.expect("exchanged");
        assert_eq!(got.api_key, "secret-key");
        assert_eq!(got.username, "yuma");
        assert!(got.is_pro);
        assert!(!format!("{got:?}").contains("secret-key"));
        let bad = exchange(&server.uri(), "bad").await.expect_err("refused");
        assert_eq!(bad.status, 400);
        let text = exchange_failure_text(&bad);
        assert!(text.contains("期限切れ") && text.contains("HTTP 400"), "{text}");
    }

    #[test]
    fn a_key_with_a_newline_or_no_key_is_not_accepted() {
        assert!(parse_exchange(r#"{"api_key":"a\nb"}"#).is_err());
        assert!(parse_exchange(r#"{"username":"x"}"#).is_err());
        let ok = parse_exchange(r#"{"api_key":"k"}"#).expect("ok");
        assert_eq!(ok.username, "");
        assert!(!ok.is_pro);
    }
}
