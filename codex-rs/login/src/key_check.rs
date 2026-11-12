//! Checking an API key when it is registered: one light authenticated read (`GET /models`).
//!
//! A key the provider refuses (HTTP 401 or 403) is rejected with the reason. A check that could
//! not be made (no connection, a timeout, another status) is not a rejection: the key is stored
//! as unverified. The key itself never appears in a result or an error text.

use crate::AuthRouteConfig;
use crate::auth::default_client::create_raw_auth_client;
use serde_json::Value;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

/// A model the key can use, as the provider listed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckedModel {
    pub id: String,
    pub display_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyCheck {
    /// The provider accepted the key. `models` is what it listed (possibly empty).
    Valid { models: Vec<CheckedModel> },
    /// The provider refused the key (HTTP 401 or 403).
    Rejected { status: u16 },
    /// The check could not be made, or the answer says nothing about the key.
    Unverified {
        reason: String,
        status: Option<u16>,
    },
}

/// OpenAI: `GET {base_url}/models` with the key as a bearer. `base_url` ends in `/v1`.
pub async fn check_openai(route: &AuthRouteConfig, base_url: &str, key: &str) -> KeyCheck {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let client = match create_raw_auth_client(&url, route) {
        Ok(client) => client,
        Err(_) => return unverified("the check could not be prepared", None),
    };
    let request = client
        .get(&url)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {key}"));
    match send(request).await {
        Ok(body) => KeyCheck::Valid {
            models: models_from(&body),
        },
        Err(failure) => failure,
    }
}

/// Claude: `GET {base_url}/v1/models` with the key as `x-api-key`.
pub async fn check_claude(route: &AuthRouteConfig, base_url: &str, key: &str) -> KeyCheck {
    match crate::claude_oauth::fetch_models(route, base_url, key, /*oauth*/ false).await {
        Ok(models) => KeyCheck::Valid {
            models: models
                .into_iter()
                .map(|model| CheckedModel {
                    id: model.id,
                    display_name: model.display_name,
                })
                .collect(),
        },
        Err(message) => match refused_status(&message) {
            Some(status @ (401 | 403)) => KeyCheck::Rejected { status },
            status => unverified(&message, status),
        },
    }
}

/// The status in "the models request was refused (401)".
fn refused_status(message: &str) -> Option<u16> {
    let inner = message.strip_suffix(')')?.rsplit('(').next()?;
    inner.parse().ok()
}

fn unverified(reason: &str, status: Option<u16>) -> KeyCheck {
    KeyCheck::Unverified {
        reason: reason.to_string(),
        status,
    }
}

async fn send(request: codex_http_client::RequestBuilder) -> Result<Value, KeyCheck> {
    let response = tokio::time::timeout(TIMEOUT, request.send())
        .await
        .map_err(|_| unverified("the check timed out", None))?
        .map_err(|_| unverified("the provider could not be reached", None))?;
    let status = response.status().as_u16();
    if matches!(status, 401 | 403) {
        return Err(KeyCheck::Rejected { status });
    }
    if !(200..300).contains(&status) {
        return Err(unverified("the provider answered with an error", Some(status)));
    }
    let text = response
        .text()
        .await
        .map_err(|_| unverified("the answer could not be read", Some(status)))?;
    // A 2xx answer is acceptance even when the list cannot be read.
    Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
}

fn models_from(body: &Value) -> Vec<CheckedModel> {
    body.get("data")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some(CheckedModel {
                        id: item.get("id")?.as_str()?.to_string(),
                        display_name: item
                            .get("display_name")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::transport_default_auth_route_config;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::header;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    #[tokio::test]
    async fn openai_accepts_lists_and_rejects() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer good"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"data": [{"id": "gpt-a"}, {"id": "gpt-b"}]}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer forbidden"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer sick"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let route = transport_default_auth_route_config();
        let base = format!("{}/v1", server.uri());
        let ids = |check: KeyCheck| match check {
            KeyCheck::Valid { models } => models.into_iter().map(|m| m.id).collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        };
        assert_eq!(ids(check_openai(&route, &base, "good").await), ["gpt-a", "gpt-b"]);
        assert_eq!(
            check_openai(&route, &base, "forbidden").await,
            KeyCheck::Rejected { status: 403 }
        );
        assert_eq!(
            check_openai(&route, &base, "bad").await,
            KeyCheck::Rejected { status: 401 }
        );
        assert!(matches!(
            check_openai(&route, &base, "sick").await,
            KeyCheck::Unverified { status: Some(503), .. }
        ));
    }

    #[tokio::test]
    async fn a_provider_that_cannot_be_reached_leaves_the_key_unverified() {
        let route = transport_default_auth_route_config();
        let check = check_openai(&route, "http://127.0.0.1:1/v1", "k").await;
        assert!(matches!(check, KeyCheck::Unverified { status: None, .. }), "{check:?}");
    }

    #[tokio::test]
    async fn claude_reads_the_refusal_status_from_the_models_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("x-api-key", "good"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "claude-x", "display_name": "Claude X"}], "has_more": false
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let route = transport_default_auth_route_config();
        assert_eq!(
            check_claude(&route, &server.uri(), "good").await,
            KeyCheck::Valid {
                models: vec![CheckedModel {
                    id: "claude-x".to_string(),
                    display_name: Some("Claude X".to_string())
                }]
            }
        );
        assert_eq!(
            check_claude(&route, &server.uri(), "bad").await,
            KeyCheck::Rejected { status: 401 }
        );
        assert_eq!(refused_status("the models request was refused (429)"), Some(429));
        assert_eq!(refused_status("the models request failed"), None);
    }
}
