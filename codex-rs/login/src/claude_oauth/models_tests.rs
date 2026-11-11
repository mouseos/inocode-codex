#![allow(clippy::expect_used)]

use super::RemoteModel;
use super::fetch_models;
use super::parse_models;
use crate::test_support::transport_default_auth_route_config;
use pretty_assertions::assert_eq;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::header_exists;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::matchers::query_param;

fn page_one() -> serde_json::Value {
    json!({
        "data": [
            {
                "id": "claude-haiku-5-5",
                "display_name": "Claude Haiku 5.5",
                "created_at": "2026-10-08T00:00:00Z",
                "type": "model",
                "max_input_tokens": 1_000_000,
                "max_tokens": 128_000,
                "capabilities": {"image_input": {"supported": true}, "thinking": {"supported": true}}
            },
            {"id": "bad id with spaces"},
            {"type": "model"}
        ],
        "has_more": true,
        "last_id": "claude-haiku-5-5"
    })
}

#[test]
fn a_page_keeps_what_the_endpoint_says_and_drops_entries_without_a_usable_id() {
    let models = parse_models(&page_one());
    assert_eq!(
        models,
        vec![RemoteModel {
            id: "claude-haiku-5-5".to_string(),
            display_name: Some("Claude Haiku 5.5".to_string()),
            created_at: Some("2026-10-08T00:00:00Z".to_string()),
            max_input_tokens: Some(1_000_000),
            max_tokens: Some(128_000),
            image_input: Some(true),
            thinking: Some(true),
        }]
    );
    let sparse = parse_models(&json!({"data": [{"id": "claude-x"}]}));
    assert_eq!(sparse.len(), 1);
    assert_eq!(sparse[0].display_name, None);
    assert_eq!(sparse[0].max_input_tokens, None);
    assert_eq!(sparse[0].image_input, None);
}

#[tokio::test]
async fn an_oauth_read_follows_the_pages_and_sends_the_bearer_and_beta_headers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(query_param("after_id", "claude-haiku-5-5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "claude-opus-5-5", "display_name": "Claude Opus 5.5"}],
            "has_more": false
        })))
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer access-1"))
        .and(header("anthropic-beta", "oauth-2025-04-20"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page_one()))
        .with_priority(5)
        .expect(1)
        .mount(&server)
        .await;
    // A trailing `/v1` on the base URL is tolerated.
    let models = fetch_models(
        &transport_default_auth_route_config(),
        &format!("{}/v1/", server.uri()),
        "access-1",
        true,
    )
    .await
    .expect("models");
    let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, vec!["claude-haiku-5-5", "claude-opus-5-5"]);
}

#[tokio::test]
async fn an_api_key_read_uses_x_api_key_and_a_refusal_does_not_leak_the_key() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", "sk-ant-FAKE"))
        .and(header_exists("anthropic-version"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "claude-a"}]})),
        )
        .mount(&server)
        .await;
    let route = transport_default_auth_route_config();
    let models = fetch_models(&route, &server.uri(), "sk-ant-FAKE", false)
        .await
        .expect("models");
    assert_eq!(models.len(), 1);

    let refusing = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401).set_body_string("sk-ant-FAKE secret-detail"))
        .mount(&refusing)
        .await;
    let error = fetch_models(&route, &refusing.uri(), "sk-ant-FAKE", false)
        .await
        .expect_err("refused");
    assert!(error.contains("401"));
    assert!(!error.contains("sk-ant"));
    assert!(!error.contains("secret-detail"));
}
