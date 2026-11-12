//! DeepAI Code as a model provider (`deepai`): the free and Theos-metered models served at
//! `https://api.deepai.org/deepai_code`.
//!
//! Spec: the Notion page "DeepAI Code" (static analysis of DeepAI Code 0.3.20 plus anonymous
//! measurements). What lives here: the model catalog (`GET /models`, no sign-in), the headers
//! every request carries, the Theos balance and check-in, the loopback sign-in, the reading of
//! the error codes, and the wait for a busy session. Credentials are stored by the account pool
//! (group `deepai`); this crate never writes an API key anywhere and never logs one.

pub mod busy;
pub mod catalog;
pub mod errors;
pub mod headers;
pub mod machine;
pub mod signin;
pub mod theos;

/// The origin of every DeepAI endpoint. Fixed: the account endpoints sit beside the model base.
pub const ORIGIN: &str = "https://api.deepai.org";
/// The base of the model, catalog, balance and history endpoints.
pub const BASE_URL: &str = "https://api.deepai.org/deepai_code";
/// The provider id and the account-pool group.
pub const PROVIDER_ID: &str = "deepai";
/// The lane version this client declares (`X-DeepAI-Code-Theos`).
pub const LANE_VERSION: &str = "4";

/// A loopback origin (`INOCODE_DEEPAI_TEST_ORIGIN`) that replaces the real one, for tests against
/// a mock server. Anything that is not `http://127.0.0.1`, `http://localhost` or `http://[::1]`
/// is ignored, so the key can never be sent to another host this way.
fn test_origin() -> Option<String> {
    let value = std::env::var("INOCODE_DEEPAI_TEST_ORIGIN").ok()?;
    let value = value.trim().trim_end_matches('/').to_string();
    let rest = value.strip_prefix("http://")?;
    let host = rest.split([':', '/']).next().unwrap_or_default();
    let host = if rest.starts_with('[') { rest.split(']').next().map(|h| format!("{h}]")).unwrap_or_default() } else { host.to_string() };
    matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]").then_some(value)
}

/// The origin in use: the real one, or the loopback test origin.
pub fn origin() -> String {
    test_origin().unwrap_or_else(|| ORIGIN.to_string())
}

/// The base of the model and account endpoints in use.
pub fn base_url() -> String {
    match test_origin() {
        Some(origin) => format!("{origin}/deepai_code"),
        None => BASE_URL.to_string(),
    }
}

/// Whether `base_url` points at DeepAI's own host (or the loopback test origin).
pub fn is_deepai_url(base_url: &str) -> bool {
    base_url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .is_some_and(|host| host.eq_ignore_ascii_case("api.deepai.org"))
        || test_origin().is_some_and(|origin| base_url.starts_with(&origin))
}

/// What identifies an account on screen: the first characters of `sha256(api_key)`. The key
/// itself is never shown.
pub fn display_id(api_key: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(api_key.as_bytes());
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        // A redirect is never followed: the key must not travel to another place.
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|error| format!("could not build the HTTP client: {error}"))
}
