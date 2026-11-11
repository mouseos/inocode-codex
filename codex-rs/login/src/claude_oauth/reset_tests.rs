#![allow(clippy::expect_used)]

use super::ConsumeFailure;
use super::consts;
use super::consume;
use super::pending_operation;
use crate::test_support::transport_default_auth_route_config;
use pretty_assertions::assert_eq;
use serde_json::json;
use serial_test::serial;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_json;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

const ORG: &str = "11111111-2222-4333-8444-555555555555";
const NOW: i64 = 1_794_000_000_000;
const OP: &str = "0b9c1f0e-1111-4222-8333-444455556666";
const OP_2: &str = "0b9c1f0e-9999-4222-8333-444455556666";

struct Endpoints;

impl Endpoints {
    fn point_at(server: &MockServer) -> Self {
        // SAFETY: these tests are `#[serial]`, so nothing else reads the environment meanwhile.
        unsafe {
            std::env::set_var(consts::PROFILE_URL_ENV, format!("{}/profile", server.uri()));
            std::env::set_var(consts::USAGE_URL_ENV, format!("{}/usage", server.uri()));
            std::env::set_var(consts::API_ORIGIN_ENV, server.uri());
        }
        Self
    }
}

impl Drop for Endpoints {
    fn drop(&mut self) {
        // SAFETY: see `point_at`.
        unsafe {
            std::env::remove_var(consts::PROFILE_URL_ENV);
            std::env::remove_var(consts::USAGE_URL_ENV);
            std::env::remove_var(consts::API_ORIGIN_ENV);
        }
    }
}

async fn profile(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/profile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "organization": {"uuid": ORG.to_uppercase()}
        })))
        .mount(server)
        .await;
}

fn usage_body(at_limit: bool, requires_limit: bool, paused: bool) -> serde_json::Value {
    json!({"cedar_ember": {
        "eligible": true,
        "at_limit": at_limit,
        "next_grant_id": "grant_1",
        "grants": [{
            "id": "grant_1",
            "label": "Bonus",
            "resets_total": 1,
            "resets_left": 1,
            "usable_now": true,
            "use_requires_limit": requires_limit,
            "paused": paused
        }]
    }})
}

async fn usage(server: &MockServer, body: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

fn claim_path() -> String {
    format!("/api/organizations/{ORG}/reset_rate_limits")
}

fn journal(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("reset-journal.json")
}

#[tokio::test]
#[serial]
async fn a_spend_posts_the_claim_and_a_repeat_of_the_same_operation_is_a_replay() {
    let server = MockServer::start().await;
    profile(&server).await;
    usage(&server, usage_body(true, true, false)).await;
    Mock::given(method("POST"))
        .and(path(claim_path()))
        .and(header("authorization", "Bearer access-1"))
        .and(header("anthropic-beta", consts::OAUTH_BETA))
        .and(body_json(json!({
            "program": "cedar_ember", "grant_id": "grant_1", "request_id": OP
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "reset", "resets_left": 0, "cleared": ["five_hour", "seven_day", "other"]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let _endpoints = Endpoints::point_at(&server);
    let dir = tempfile::tempdir().expect("tempdir");
    let route = transport_default_auth_route_config();

    let done = consume(&route, "access-1", &journal(&dir), "acct", None, OP, NOW)
        .await
        .expect("settled");
    assert_eq!(done.code, "reset");
    assert!(!done.replayed);
    assert_eq!(done.grant_id, "grant_1");
    assert_eq!(done.resets_left, Some(0));
    assert_eq!(done.cleared, vec!["five_hour", "seven_day"]);

    // The journal answers a repeat without a second claim (the mock expects exactly one).
    let again = consume(&route, "access-1", &journal(&dir), "acct", None, OP, NOW + 1_000)
        .await
        .expect("replayed");
    assert!(again.replayed);
    assert_eq!(again.code, "reset");
    assert_eq!(pending_operation(&journal(&dir), "acct", NOW + 2_000), None);
}

#[tokio::test]
#[serial]
async fn a_grant_that_cannot_be_spent_is_refused_before_any_claim() {
    let server = MockServer::start().await;
    profile(&server).await;
    usage(&server, usage_body(false, true, false)).await; // needs the limit; not at it
    Mock::given(method("POST"))
        .and(path(claim_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": "reset"})))
        .expect(0)
        .mount(&server)
        .await;
    let _endpoints = Endpoints::point_at(&server);
    let dir = tempfile::tempdir().expect("tempdir");
    let error = consume(
        &transport_default_auth_route_config(),
        "access-1",
        &journal(&dir),
        "acct",
        Some("grant_1"),
        OP,
        NOW,
    )
    .await
    .expect_err("blocked");
    assert_eq!(error, ConsumeFailure::NotUsable("not_limited"));
}

#[tokio::test]
#[serial]
async fn a_claim_without_an_answer_stays_open_and_only_the_same_operation_may_retry_it() {
    let server = MockServer::start().await;
    profile(&server).await;
    usage(&server, usage_body(true, true, false)).await;
    Mock::given(method("POST"))
        .and(path(claim_path()))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(claim_path()))
        .and(body_json(json!({
            "program": "cedar_ember", "grant_id": "grant_1", "request_id": OP
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "reset", "resets_left": 0, "cleared": ["five_hour"]
        })))
        .with_priority(2)
        .expect(1)
        .mount(&server)
        .await;
    let _endpoints = Endpoints::point_at(&server);
    let dir = tempfile::tempdir().expect("tempdir");
    let route = transport_default_auth_route_config();
    let journal = journal(&dir);

    let first = consume(&route, "access-1", &journal, "acct", None, OP, NOW)
        .await
        .expect_err("no answer");
    assert_eq!(
        first,
        ConsumeFailure::UnknownOutcome {
            operation_id: OP.to_string()
        }
    );
    assert_eq!(
        pending_operation(&journal, "acct", NOW + 1_000).map(|p| p.operation_id),
        Some(OP.to_string())
    );

    // Another confirmation for the same grant must not send a second claim.
    let other = consume(&route, "access-1", &journal, "acct", None, OP_2, NOW + 2_000)
        .await
        .expect_err("unresolved");
    assert_eq!(
        other,
        ConsumeFailure::UnresolvedPrior {
            operation_id: OP.to_string()
        }
    );

    // Ten minutes on, the open operation is expired instead of being resent.
    let expired = consume(&route, "access-1", &journal, "acct", None, OP, NOW + 700_000)
        .await
        .expect_err("expired");
    assert_eq!(expired, ConsumeFailure::Expired);

    // Retrying the same operation resends the same request id and settles.
    let retried = consume(&route, "access-1", &journal, "acct", None, OP, NOW + 3_000)
        .await
        .expect("settled on retry");
    assert_eq!(retried.code, "reset");
    assert_eq!(pending_operation(&journal, "acct", NOW + 4_000), None);
}

#[tokio::test]
#[serial]
async fn a_rate_limited_claim_is_settled_as_nothing_spent_and_a_refused_token_is_an_auth_failure() {
    let server = MockServer::start().await;
    profile(&server).await;
    usage(&server, usage_body(true, true, false)).await;
    Mock::given(method("POST"))
        .and(path(claim_path()))
        .respond_with(ResponseTemplate::new(429).set_body_string("access-1 secret-detail"))
        .mount(&server)
        .await;
    let _endpoints = Endpoints::point_at(&server);
    let dir = tempfile::tempdir().expect("tempdir");
    let route = transport_default_auth_route_config();
    let done = consume(&route, "access-1", &journal(&dir), "acct", None, OP, NOW)
        .await
        .expect("settled");
    assert_eq!(done.code, "rate_limited");

    let refused = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/profile"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&refused)
        .await;
    let _again = Endpoints::point_at(&refused);
    let error = consume(&route, "access-1", &journal(&dir), "acct", None, OP_2, NOW)
        .await
        .expect_err("refused");
    assert_eq!(error, ConsumeFailure::AuthFailed);
}

#[tokio::test]
async fn malformed_ids_are_rejected_before_anything_is_sent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let route = transport_default_auth_route_config();
    let bad_operation = consume(&route, "t", &journal(&dir), "acct", None, "has space", NOW)
        .await
        .expect_err("bad operation");
    assert_eq!(bad_operation, ConsumeFailure::InvalidInput);
    let bad_grant = consume(&route, "t", &journal(&dir), "acct", Some("BAD GRANT"), OP, NOW)
        .await
        .expect_err("bad grant");
    assert_eq!(bad_grant, ConsumeFailure::InvalidInput);
}
