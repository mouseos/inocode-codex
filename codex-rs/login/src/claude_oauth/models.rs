//! The models a Claude credential can use, read from `GET /v1/models`.
//!
//! The list is read, never written: nothing here sends a prompt. An OAuth sign-in authenticates
//! with a bearer token plus the OAuth beta header (as the usage read does); an API key goes in
//! `x-api-key`. The error text never carries the token or the response body.

use super::consts;
use crate::AuthRouteConfig;
use crate::auth::default_client::create_raw_auth_client;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

const MODELS_TIMEOUT: Duration = Duration::from_secs(10);
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Pages followed at most (a page holds up to 1000 models).
const MAX_PAGES: usize = 5;

/// One entry of the models list, with what the endpoint says about it (absent stays absent).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteModel {
    pub id: String,
    pub display_name: Option<String>,
    /// RFC 3339, as the endpoint gives it.
    pub created_at: Option<String>,
    pub max_input_tokens: Option<i64>,
    pub max_tokens: Option<i64>,
    /// `capabilities.image_input.supported`.
    pub image_input: Option<bool>,
    /// `capabilities.thinking.supported`.
    pub thinking: Option<bool>,
}

/// Reads every model the credential can use. `base_url` is the provider's base URL (with or
/// without a trailing `/v1`).
pub async fn fetch_models(
    route: &AuthRouteConfig,
    base_url: &str,
    token: &str,
    oauth: bool,
) -> Result<Vec<RemoteModel>, String> {
    let base = base_url.trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    let mut models: Vec<RemoteModel> = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut url = format!("{base}/v1/models?limit=1000");
        if let Some(after) = &after {
            url.push_str("&after_id=");
            url.push_str(after);
        }
        let body = get(route, &url, token, oauth).await?;
        let (page, next) = parse_page(&body);
        models.extend(page);
        match next {
            Some(next) => after = Some(next),
            None => return Ok(models),
        }
    }
    Ok(models)
}

async fn get(
    route: &AuthRouteConfig,
    url: &str,
    token: &str,
    oauth: bool,
) -> Result<Value, String> {
    let client = create_raw_auth_client(url, route)
        .map_err(|_| "the models request could not be prepared".to_string())?;
    let mut request = client
        .get(url)
        .header("Accept", "application/json")
        .header("anthropic-version", ANTHROPIC_VERSION);
    request = if oauth {
        request
            .header("Authorization", format!("Bearer {token}"))
            .header("anthropic-beta", consts::OAUTH_BETA)
            .header("User-Agent", consts::ACCOUNT_USER_AGENT)
    } else {
        request.header("x-api-key", token)
    };
    let response = tokio::time::timeout(MODELS_TIMEOUT, request.send())
        .await
        .map_err(|_| "the models request timed out".to_string())?
        .map_err(|_| "the models request failed".to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("the models request was refused ({})", status.as_u16()));
    }
    let text = response
        .text()
        .await
        .map_err(|_| "the models answer could not be read".to_string())?;
    serde_json::from_str(&text).map_err(|_| "the models answer was not JSON".to_string())
}

/// The models of one page, and the cursor of the next page when there is one.
fn parse_page(body: &Value) -> (Vec<RemoteModel>, Option<String>) {
    let models = body
        .get("data")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_model).collect())
        .unwrap_or_default();
    let more = body.get("has_more").and_then(Value::as_bool) == Some(true);
    let next = more
        .then(|| body.get("last_id").and_then(Value::as_str))
        .flatten()
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_graphic()))
        .map(str::to_string);
    (models, next)
}

/// Parses a models answer (one page).
pub fn parse_models(body: &Value) -> Vec<RemoteModel> {
    parse_page(body).0
}

fn parse_model(item: &Value) -> Option<RemoteModel> {
    let id = item.get("id")?.as_str()?.trim();
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_graphic()) {
        return None;
    }
    let text = |key: &str| {
        item.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let count = |key: &str| {
        item.get(key)
            .and_then(Value::as_i64)
            .filter(|count| *count > 0)
    };
    let supported = |capability: &str| {
        item.get("capabilities")
            .and_then(|caps| caps.get(capability))
            .and_then(|cap| cap.get("supported"))
            .and_then(Value::as_bool)
    };
    Some(RemoteModel {
        id: id.to_string(),
        display_name: text("display_name"),
        created_at: text("created_at"),
        max_input_tokens: count("max_input_tokens"),
        max_tokens: count("max_tokens"),
        image_input: supported("image_input"),
        thinking: supported("thinking"),
    })
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod tests;
