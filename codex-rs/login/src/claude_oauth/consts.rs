use base64::Engine;
use base64::engine::general_purpose::STANDARD;

pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";
pub const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
/// Quota and reset-grant read (`cedar_ember` is the grant block's name upstream).
pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const USAGE_QUERY: &str = "cedar_ember=1&skip_spend=1";
/// The Claude Code client identity the account endpoints expect: some response blocks (the reset
/// grants) are only filled in for this form of the header.
pub const ACCOUNT_USER_AGENT: &str = "claude-cli/2.1.280 (external, cli)";
pub const SCOPES: &str = "org:create_api_key user:profile user:inference";
pub const CALLBACK_PORT: u16 = 54545;
pub const CALLBACK_PATH: &str = "/callback";
/// The provider shows `code#state` for the user to paste (no local listener needed).
pub const CODE_REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
pub const OAUTH_BETA: &str = "oauth-2025-04-20";
/// Refresh this long before the token really expires.
pub const EXPIRY_MARGIN_MS: i64 = 5 * 60 * 1000;

/// Environment override for the OAuth client id (experiments).
pub const CLIENT_ID_ENV: &str = "INOCODE_CLAUDE_OAUTH_CLIENT_ID";
const CLIENT_ID_B64: &str = "OWQxYzI1MGEtZTYxYi00NGQ5LTg4ZWQtNTk0NGQxOTYyZjVl";

pub fn client_id() -> String {
    if let Ok(value) = std::env::var(CLIENT_ID_ENV)
        && !value.trim().is_empty()
    {
        return value.trim().to_string();
    }
    STANDARD
        .decode(CLIENT_ID_B64)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default()
}

/// Endpoint overrides, so tests (and local experiments) can point at a mock server.
pub const TOKEN_URL_ENV: &str = "INOCODE_CLAUDE_OAUTH_TOKEN_URL";
pub const PROFILE_URL_ENV: &str = "INOCODE_CLAUDE_OAUTH_PROFILE_URL";
pub const USAGE_URL_ENV: &str = "INOCODE_CLAUDE_OAUTH_USAGE_URL";

fn overridden(env: &str, default: &str) -> String {
    match std::env::var(env) {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => default.to_string(),
    }
}

pub fn token_url() -> String {
    overridden(TOKEN_URL_ENV, TOKEN_URL)
}

pub fn profile_url() -> String {
    overridden(PROFILE_URL_ENV, PROFILE_URL)
}

pub fn usage_url() -> String {
    overridden(USAGE_URL_ENV, USAGE_URL)
}

pub fn callback_redirect_uri() -> String {
    format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}")
}
