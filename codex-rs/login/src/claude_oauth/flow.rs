#![allow(clippy::expect_used)]

//! Authorization-code + PKCE flow against Anthropic (ported from OpenCodex `oauth/anthropic.ts`).
//!
//! Two redirects exist: a local listener on `localhost:54545` and the provider-hosted
//! "code display" page (`code#state` is shown for the user to paste, which also works where no
//! browser can reach this process). The same `redirect_uri` must be used for the authorize
//! request and the token exchange.

use super::consts;
use super::store::ClaudeCredentials;
use crate::AuthRouteConfig;
use crate::auth::default_client::create_raw_auth_client;
use crate::pkce::generate_pkce;
use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);
const PROFILE_TIMEOUT: Duration = Duration::from_secs(10);
const REFRESH_PROFILE_TIMEOUT: Duration = Duration::from_secs(2);

/// A sign-in that has been started. The verifier never leaves this struct.
#[derive(Clone)]
pub struct Pending {
    pub url: String,
    pub state: String,
    pub redirect_uri: String,
    verifier: String,
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending")
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

pub fn begin(code_mode: bool) -> Pending {
    let pkce = generate_pkce();
    let mut raw = [0u8; 32];
    rand::rng().fill_bytes(&mut raw);
    let state = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    let redirect_uri = if code_mode {
        consts::CODE_REDIRECT_URI.to_string()
    } else {
        consts::callback_redirect_uri()
    };
    let mut url = url::Url::parse(consts::AUTHORIZE_URL).expect("a constant URL parses");
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", &consts::client_id())
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", consts::SCOPES)
        .append_pair("code_challenge", &pkce.code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state);
    Pending {
        url: url.to_string(),
        state,
        redirect_uri,
        verifier: pkce.code_verifier,
    }
}

/// Accepts what a user can paste: `code#state`, the final redirect URL, or the bare code.
pub fn split_code_input(input: &str) -> (String, Option<String>) {
    let input = input.trim();
    if let Ok(url) = url::Url::parse(input) {
        let pair = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
        };
        if let Some(code) = pair("code") {
            return (code, pair("state"));
        }
    }
    match input.split_once('#') {
        Some((code, state)) if !state.is_empty() => (code.to_string(), Some(state.to_string())),
        Some((code, _)) => (code.to_string(), None),
        None => (input.to_string(), None),
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<f64>,
    #[serde(default)]
    account: Option<Account>,
}

#[derive(Deserialize)]
struct Account {
    #[serde(default)]
    uuid: Option<String>,
}

fn credentials_from(response: TokenResponse, refresh_fallback: Option<&str>, now_ms: i64) -> io::Result<ClaudeCredentials> {
    let refresh = response
        .refresh_token
        .filter(|t| !t.is_empty())
        .or_else(|| refresh_fallback.map(str::to_string))
        .ok_or_else(|| io::Error::other("the token response carried no refresh token"))?;
    // A missing, non-finite or negative lifetime would stamp a bogus expiry.
    let seconds = response
        .expires_in
        .filter(|s| s.is_finite() && *s >= 0.0)
        .unwrap_or(3600.0);
    let expires_at_ms = now_ms
        .saturating_add((seconds * 1000.0) as i64)
        .saturating_sub(consts::EXPIRY_MARGIN_MS);
    Ok(ClaudeCredentials {
        access: response.access_token,
        refresh,
        expires_at_ms,
        account_uuid: response
            .account
            .and_then(|a| a.uuid)
            .filter(|u| !u.is_empty() && u.len() <= 128 && u.trim() == u),
        plan: None,
    })
}

async fn post_token(route: &AuthRouteConfig, body: Value) -> io::Result<TokenResponse> {
    let url = consts::token_url();
    let client = create_raw_auth_client(&url, route).map_err(io::Error::other)?;
    let request = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .body(body.to_string())
        .send();
    let response = tokio::time::timeout(TOKEN_TIMEOUT, request)
        .await
        .map_err(|_| io::Error::other("the Anthropic token request timed out"))?
        .map_err(io::Error::other)?;
    let status = response.status();
    let text = response.text().await.map_err(io::Error::other)?;
    if !status.is_success() {
        // Only the OAuth error code, never the body: it may echo parts of the request.
        let code = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string));
        return Err(io::Error::other(match code {
            Some(code) => format!("Anthropic OAuth HTTP {status}: {code}"),
            None => format!("Anthropic OAuth HTTP {status}"),
        }));
    }
    serde_json::from_str(&text).map_err(|_| io::Error::other("the token response was not valid JSON"))
}

/// What the profile endpoint says about the account. Every field is best effort.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profile {
    pub plan: Option<String>,
    pub email: Option<String>,
    pub name: Option<String>,
}

/// The first non-empty string under any of `keys` in `object`.
fn first_string(object: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn parse_profile(data: &Value) -> Profile {
    let account = data.get("account").unwrap_or(&Value::Null);
    Profile {
        plan: data.get("organization").and_then(|org| {
            ["rate_limit_tier", "organization_type"]
                .iter()
                .find_map(|key| org.get(*key).and_then(Value::as_str))
                .map(|plan| plan.to_ascii_lowercase())
        }),
        email: first_string(account, &["email", "email_address"]).or_else(|| first_string(data, &["email"])),
        name: first_string(account, &["display_name", "full_name", "name"]).or_else(|| first_string(data, &["name"])),
    }
}

/// The signed-in account's plan, e-mail address and display name.
pub async fn fetch_profile(route: &AuthRouteConfig, access: &str) -> Profile {
    fetch_profile_within(route, access, PROFILE_TIMEOUT).await
}

async fn fetch_profile_within(route: &AuthRouteConfig, access: &str, timeout: Duration) -> Profile {
    fetch_profile_inner(route, access, timeout).await.unwrap_or_default()
}

/// Best effort: the account's plan, from the profile endpoint.
async fn fetch_plan(route: &AuthRouteConfig, access: &str, timeout: Duration) -> Option<String> {
    fetch_profile_within(route, access, timeout).await.plan
}

async fn fetch_profile_inner(route: &AuthRouteConfig, access: &str, timeout: Duration) -> Option<Profile> {
    let url = consts::profile_url();
    let client = create_raw_auth_client(&url, route).ok()?;
    let request = client
        .get(&url)
        .header("Authorization", format!("Bearer {access}"))
        .header("Accept", "application/json")
        .header("anthropic-beta", consts::OAUTH_BETA)
        .send();
    let response = tokio::time::timeout(timeout, request).await.ok()?.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let data: Value = serde_json::from_str(&response.text().await.ok()?).ok()?;
    Some(parse_profile(&data))
}

/// Exchanges what the user pasted (or the listener received) for credentials.
pub async fn exchange(
    pending: &Pending,
    input: &str,
    route: &AuthRouteConfig,
    now_ms: i64,
) -> io::Result<ClaudeCredentials> {
    let (code, state) = split_code_input(input);
    if code.is_empty() {
        return Err(io::Error::other("no authorization code was given"));
    }
    if let Some(state) = state.as_deref()
        && state != pending.state
    {
        return Err(io::Error::other("the sign-in state does not match; start again"));
    }
    let response = post_token(
        route,
        json!({
            "grant_type": "authorization_code",
            "client_id": consts::client_id(),
            "code": code,
            "state": state.unwrap_or_else(|| pending.state.clone()),
            "redirect_uri": pending.redirect_uri,
            "code_verifier": pending.verifier,
        }),
    )
    .await?;
    let mut credentials = credentials_from(response, None, now_ms)?;
    credentials.plan = fetch_plan(route, &credentials.access, PROFILE_TIMEOUT).await;
    Ok(credentials)
}

pub async fn refresh_credentials(
    credentials: &ClaudeCredentials,
    route: &AuthRouteConfig,
    now_ms: i64,
) -> io::Result<ClaudeCredentials> {
    let response = post_token(
        route,
        json!({
            "grant_type": "refresh_token",
            "client_id": consts::client_id(),
            "refresh_token": credentials.refresh,
        }),
    )
    .await?;
    let mut refreshed = credentials_from(response, Some(&credentials.refresh), now_ms)?;
    if refreshed.account_uuid.is_none() {
        refreshed.account_uuid.clone_from(&credentials.account_uuid);
    }
    refreshed.plan = fetch_plan(route, &refreshed.access, REFRESH_PROFILE_TIMEOUT)
        .await
        .or_else(|| credentials.plan.clone());
    Ok(refreshed)
}

/// The local listener a browser sign-in returns to (`http://localhost:54545/callback`).
///
/// Binding is separate from waiting so a caller learns about a busy port *before* it hands the
/// user an authorization URL. Dropping the listener (completion, failure, cancel, timeout) closes
/// the port.
pub struct CallbackListener {
    server: tiny_http::Server,
}

impl CallbackListener {
    pub async fn bind() -> io::Result<Self> {
        Self::bind_on(consts::CALLBACK_PORT).await
    }

    /// Binds `port` (0 = any free port). A port that a just-cancelled sign-in is still releasing
    /// is retried for a moment; a port held by another process fails with `AddrInUse`.
    pub async fn bind_on(port: u16) -> io::Result<Self> {
        tokio::task::spawn_blocking(move || {
            let deadline = Instant::now() + Duration::from_millis(1500);
            loop {
                match tiny_http::Server::http(("127.0.0.1", port)) {
                    Ok(server) => return Ok(Self { server }),
                    Err(err) => {
                        let in_use = err
                            .downcast_ref::<io::Error>()
                            .is_some_and(|e| e.kind() == io::ErrorKind::AddrInUse);
                        if in_use && Instant::now() < deadline {
                            std::thread::sleep(Duration::from_millis(100));
                            continue;
                        }
                        let kind = if in_use { io::ErrorKind::AddrInUse } else { io::ErrorKind::Other };
                        return Err(io::Error::new(
                            kind,
                            format!("cannot listen on the callback port {port}: {err}"),
                        ));
                    }
                }
            }
        })
        .await
        .map_err(io::Error::other)?
    }

    pub fn port(&self) -> Option<u16> {
        self.server.server_addr().to_ip().map(|addr| addr.port())
    }

    /// Waits for the redirect and returns the authorization code. `cancel` ends the wait early.
    ///
    /// Requests that are not the callback, or that carry neither `code` nor `state` (a favicon, a
    /// probe), get a 404 and are ignored; a callback with the wrong `state` gets a 400 and the
    /// wait goes on.
    pub async fn wait(
        self,
        state: String,
        timeout: Duration,
        cancel: Arc<AtomicBool>,
    ) -> io::Result<String> {
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
                let url = url::Url::parse(&format!("http://localhost{}", request.url())).ok();
                let is_callback = url.as_ref().is_some_and(|u| u.path() == consts::CALLBACK_PATH);
                let pair = |name: &str| {
                    url.as_ref().and_then(|u| {
                        u.query_pairs()
                            .find(|(k, _)| k == name)
                            .map(|(_, v)| v.into_owned())
                    })
                };
                let page = |status: u16, body: &str| {
                    let response = tiny_http::Response::from_string(body.to_string())
                        .with_status_code(status)
                        .with_header(
                            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                                .expect("a constant header parses"),
                        );
                    let _ = request.respond(response);
                };
                if !is_callback {
                    page(404, "Not found");
                    continue;
                }
                if let Some(error) = pair("error") {
                    page(400, "Sign-in failed. You can close this tab.");
                    return Err(io::Error::other(format!("the sign-in was refused: {error}")));
                }
                match (pair("code"), pair("state")) {
                    (Some(code), Some(got)) if got == state => {
                        page(200, "Signed in. You can close this tab and return to InoCode.");
                        return Ok(code);
                    }
                    (None, None) => page(404, "Not found"),
                    _ => page(400, "The sign-in state did not match. Start again."),
                }
            }
        })
        .await
        .map_err(io::Error::other)?
    }
}

/// Binds the callback port and waits for the redirect (for callers that do both at once).
pub async fn wait_for_callback(
    state: String,
    timeout: Duration,
    cancel: Arc<AtomicBool>,
) -> io::Result<String> {
    CallbackListener::bind().await?.wait(state, timeout, cancel).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn the_authorize_url_carries_pkce_state_scope_and_the_chosen_redirect() {
        let pending = begin(false);
        let url = url::Url::parse(&pending.url).expect("url");
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.host_str(), Some("claude.ai"));
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["redirect_uri"], "http://localhost:54545/callback");
        assert_eq!(query["state"], pending.state);
        assert!(query["scope"].contains("user:inference"));
        assert!(!query["client_id"].is_empty());
        assert!(!pending.url.contains(&pending.verifier));
        assert_eq!(begin(true).redirect_uri, consts::CODE_REDIRECT_URI);
        assert_ne!(begin(false).state, begin(false).state);
    }

    #[test]
    fn pasted_input_is_understood_in_all_three_shapes() {
        assert_eq!(split_code_input("abc#st"), ("abc".into(), Some("st".into())));
        assert_eq!(
            split_code_input("http://localhost:54545/callback?code=abc&state=st"),
            ("abc".into(), Some("st".into()))
        );
        assert_eq!(split_code_input("  abc  "), ("abc".into(), None));
        assert_eq!(split_code_input("abc#"), ("abc".into(), None));
    }

    #[test]
    fn token_responses_become_credentials_with_a_safe_expiry() {
        let response = |expires_in: Option<f64>, refresh: Option<&str>| TokenResponse {
            access_token: "a".into(),
            refresh_token: refresh.map(str::to_string),
            expires_in,
            account: Some(Account { uuid: Some("u-1".into()) }),
        };
        let now = 1_000_000;
        let ok = credentials_from(response(Some(3600.0), Some("r")), None, now).expect("ok");
        assert_eq!(ok.expires_at_ms, now + 3_600_000 - consts::EXPIRY_MARGIN_MS);
        assert_eq!(ok.account_uuid.as_deref(), Some("u-1"));
        // A bad lifetime falls back to an hour instead of an already-past or infinite expiry.
        let bad = credentials_from(response(Some(-5.0), Some("r")), None, now).expect("ok");
        assert_eq!(bad.expires_at_ms, now + 3_600_000 - consts::EXPIRY_MARGIN_MS);
        // A refresh response that omits the refresh token keeps the old one.
        let kept = credentials_from(response(Some(60.0), None), Some("old"), now).expect("ok");
        assert_eq!(kept.refresh, "old");
        assert!(credentials_from(response(Some(60.0), None), None, now).is_err());
    }

    #[test]
    fn the_profile_yields_plan_email_and_name_when_present() {
        let profile = parse_profile(&json!({
            "account": {"uuid": "u", "email": " me@example.test ", "display_name": "Me"},
            "organization": {"rate_limit_tier": "Default_Claude_Max_5x"}
        }));
        assert_eq!(profile.plan.as_deref(), Some("default_claude_max_5x"));
        assert_eq!(profile.email.as_deref(), Some("me@example.test"));
        assert_eq!(profile.name.as_deref(), Some("Me"));
        assert_eq!(parse_profile(&json!({})), Profile::default());
    }

    #[tokio::test]
    async fn a_cancelled_sign_in_releases_the_port_and_a_new_one_can_take_it() {
        let first = CallbackListener::bind_on(0).await.expect("first bind");
        let port = first.port().expect("a port");
        // While the first one holds the port, nobody else gets it.
        assert_eq!(
            CallbackListener::bind_on(port).await.err().map(|e| e.kind()),
            Some(io::ErrorKind::AddrInUse)
        );
        // Cancelling ends the wait, and dropping the listener frees the port.
        let cancel = Arc::new(AtomicBool::new(true));
        let waited = first.wait("s1".to_string(), Duration::from_secs(5), cancel).await;
        assert!(waited.is_err());
        let second = CallbackListener::bind_on(port).await.expect("the port is free again");
        assert_eq!(second.port(), Some(port));
    }

    #[tokio::test]
    async fn probes_are_ignored_a_wrong_state_does_not_end_the_wait_and_the_right_one_wins() {
        let listener = CallbackListener::bind_on(0).await.expect("bind");
        let port = listener.port().expect("a port");
        let cancel = Arc::new(AtomicBool::new(false));
        let waiting = tokio::spawn(listener.wait("right".to_string(), Duration::from_secs(20), cancel));
        let get = |path: String| async move {
            tokio::task::spawn_blocking(move || {
                use std::io::Read;
                use std::io::Write;
                let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
                write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").expect("write");
                let mut text = String::new();
                stream.read_to_string(&mut text).expect("read");
                text.lines().next().unwrap_or_default().to_string()
            })
            .await
            .expect("join")
        };
        assert!(get("/favicon.ico".to_string()).await.contains("404"));
        assert!(get("/callback".to_string()).await.contains("404"));
        assert!(get("/callback?code=x&state=other".to_string()).await.contains("400"));
        assert!(get("/callback?code=CODE9&state=right".to_string()).await.contains("200"));
        assert_eq!(waiting.await.expect("join").expect("code"), "CODE9");
    }
}
