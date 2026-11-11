#![allow(clippy::expect_used)]

use super::consts;
use super::fetch_usage;
use super::parse_usage;
use crate::account_pool::ResetStatus;
use crate::test_support::transport_default_auth_route_config;
use pretty_assertions::assert_eq;
use serde_json::json;
use serial_test::serial;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::matchers::query_param;

const NOW: i64 = 1_794_000_000;
const TEN_UTC: i64 = 1_794_391_200;

fn full_body() -> serde_json::Value {
    json!({
        "five_hour": {"utilization": 42.5, "resets_at": "2026-11-11T10:00:00.000000+00:00"},
        "seven_day": {"utilization": 7, "resets_at": "2026-11-15T00:00:00+00:00"},
        "seven_day_opus": {"utilization": 99, "resets_at": "2026-11-15T00:00:00+00:00"},
        "cedar_ember": {
            "eligible": true,
            "at_limit": false,
            "cooldown_until": null,
            "grants": [{
                "id": "grant_1",
                "label": "  Bonus\nreset  ",
                "resets_total": 2,
                "resets_left": 1,
                "starts_at": "2026-11-01T00:00:00Z",
                "ends_at": "2026-12-01T00:00:00Z",
                "clears": ["five_hour", "seven_day", "something_new"],
                "paused": false,
                "usable_now": true,
                "use_requires_limit": false
            }]
        }
    })
}

#[test]
fn the_legacy_windows_are_read_as_percent_with_their_reset_times() {
    let usage = parse_usage(&full_body(), NOW);
    let five = usage.five_hour.expect("five-hour window");
    assert_eq!(five.used_percent, 42.5);
    assert_eq!(five.window_minutes, Some(300));
    assert_eq!(five.resets_at, Some(TEN_UTC));
    let week = usage.weekly.expect("weekly window");
    assert_eq!(week.used_percent, 7.0);
    assert_eq!(week.window_minutes, Some(10_080));
    assert_eq!(week.resets_at, Some(1_794_700_800));
    assert_eq!(usage.updated_at, Some(NOW));
}

#[test]
fn the_reset_credit_comes_from_the_grant_block() {
    let credit = parse_usage(&full_body(), NOW)
        .reset_credit
        .expect("reset credit");
    assert!(credit.eligible);
    assert!(!credit.at_limit);
    assert_eq!(credit.resets_left, 1);
    assert_eq!(credit.resets_total, 2);
    assert!(credit.usable_now);
    assert_eq!(credit.ends_at, Some(1_796_083_200));
    assert_eq!(credit.cooldown_until, None);
    assert_eq!(credit.grants.len(), 1);
    assert_eq!(credit.grants[0].label, "Bonus reset");
    // Windows the grant cannot clear (or that this client does not know) are dropped.
    assert_eq!(credit.grants[0].clears, vec!["five_hour", "seven_day"]);
}

#[test]
fn a_grant_that_needs_the_limit_is_not_usable_until_the_account_is_at_it() {
    let mut body = full_body();
    body["cedar_ember"]["grants"][0]["use_requires_limit"] = json!(true);
    assert!(!parse_usage(&body, NOW).reset_credit.expect("credit").usable_now);
    body["cedar_ember"]["at_limit"] = json!(true);
    assert!(parse_usage(&body, NOW).reset_credit.expect("credit").usable_now);
}

#[test]
fn a_malformed_grant_block_reads_as_unknown_not_as_zero_and_keeps_the_windows() {
    let mut body = full_body();
    body["cedar_ember"]["grants"][0]["resets_left"] = json!(5); // more than the total
    let usage = parse_usage(&body, NOW);
    assert_eq!(usage.reset_credit, None);
    assert_eq!(usage.reset_status, Some(ResetStatus::error("block_unreadable")));
    assert!(usage.five_hour.is_some());
    assert!(usage.weekly.is_some());
}

#[test]
fn the_reset_status_says_why_there_is_no_credit() {
    let ok = parse_usage(&full_body(), NOW);
    assert_eq!(ok.reset_status, Some(ResetStatus::ok()));

    let mut empty = full_body();
    empty["cedar_ember"]["grants"] = json!([]);
    let zero = parse_usage(&empty, NOW);
    assert_eq!(zero.reset_status, Some(ResetStatus::ok()), "zero resets is still a good read");
    assert_eq!(zero.reset_credit.expect("credit").resets_left, 0);

    let mut missing = full_body();
    missing.as_object_mut().expect("object").remove("cedar_ember");
    assert_eq!(
        parse_usage(&missing, NOW).reset_status,
        Some(ResetStatus::unavailable("block_missing"))
    );

    let mut null = full_body();
    null["cedar_ember"] = json!(null);
    assert_eq!(
        parse_usage(&null, NOW).reset_status,
        Some(ResetStatus::unavailable("block_null"))
    );
}

#[test]
fn limits_rows_supersede_the_legacy_fields_and_an_idle_session_is_absent() {
    let body = json!({
        "five_hour": null,
        "seven_day": {"utilization": 1},
        "limits": [
            {"kind": "session", "percent": 80, "is_active": false, "resets_at": "2026-11-11T10:00:00Z"},
            {"kind": "weekly_all", "percent": "12.5", "resets_at": 1_794_700_800}
        ]
    });
    let usage = parse_usage(&body, NOW);
    assert_eq!(usage.five_hour, None, "a null five_hour needs an active session row");
    let week = usage.weekly.expect("weekly from the limits row");
    assert_eq!(week.used_percent, 12.5);
    assert_eq!(week.resets_at, Some(1_794_700_800));

    let active = json!({
        "five_hour": null,
        "limits": [{"kind": "session", "percent": 250, "is_active": true}]
    });
    let five = parse_usage(&active, NOW).five_hour.expect("active session");
    assert_eq!(five.used_percent, 100.0, "percentages are clamped to 0-100");
    assert_eq!(five.resets_at, None);
}

#[test]
fn a_body_without_windows_has_no_windows() {
    let usage = parse_usage(&json!({}), NOW);
    assert_eq!(usage.five_hour, None);
    assert_eq!(usage.weekly, None);
    assert_eq!(usage.reset_credit, None);
    assert_eq!(usage.updated_at, Some(NOW));
}

struct UsageEndpoint;

impl UsageEndpoint {
    fn point_at(server: &MockServer) -> Self {
        // SAFETY: these tests are `#[serial]`, so nothing else reads the environment meanwhile.
        unsafe {
            std::env::set_var(consts::USAGE_URL_ENV, format!("{}/usage", server.uri()));
        }
        Self
    }
}

impl Drop for UsageEndpoint {
    fn drop(&mut self) {
        // SAFETY: see `point_at`.
        unsafe {
            std::env::remove_var(consts::USAGE_URL_ENV);
        }
    }
}

#[tokio::test]
#[serial]
async fn fetching_sends_the_bearer_the_beta_header_and_the_grant_query() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/usage"))
        .and(query_param("cedar_ember", "1"))
        .and(query_param("skip_spend", "1"))
        .and(header("authorization", "Bearer access-1"))
        .and(header("anthropic-beta", consts::OAUTH_BETA))
        .and(header("user-agent", consts::ACCOUNT_USER_AGENT))
        .respond_with(ResponseTemplate::new(200).set_body_json(full_body()))
        .expect(1)
        .mount(&server)
        .await;
    let _endpoint = UsageEndpoint::point_at(&server);
    let usage = fetch_usage(&transport_default_auth_route_config(), "access-1", NOW)
        .await
        .expect("usage");
    assert_eq!(usage.five_hour.expect("five-hour").used_percent, 42.5);
    assert_eq!(usage.reset_credit.expect("credit").resets_left, 1);
}

#[tokio::test]
#[serial]
async fn a_server_that_balks_at_the_grant_query_is_asked_again_without_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/usage"))
        .and(query_param("cedar_ember", "1"))
        .respond_with(ResponseTemplate::new(400))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "five_hour": {"utilization": 3, "resets_at": "2026-11-11T10:00:00Z"}
        })))
        .with_priority(5)
        .mount(&server)
        .await;
    let _endpoint = UsageEndpoint::point_at(&server);
    let usage = fetch_usage(&transport_default_auth_route_config(), "access-1", NOW)
        .await
        .expect("usage");
    assert_eq!(usage.five_hour.expect("five-hour").used_percent, 3.0);
    assert_eq!(usage.reset_credit, None);
    assert_eq!(usage.reset_status, Some(ResetStatus::unavailable("grant_query_refused")));
}

#[tokio::test]
#[serial]
async fn a_refusal_is_an_error_that_does_not_leak_the_token_or_the_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/usage"))
        .respond_with(ResponseTemplate::new(401).set_body_string("secret-detail access-1"))
        .mount(&server)
        .await;
    let _endpoint = UsageEndpoint::point_at(&server);
    let error = fetch_usage(&transport_default_auth_route_config(), "access-1", NOW)
        .await
        .expect_err("refused");
    assert!(!error.contains("access-1") && !error.contains("secret-detail"), "{error}");
}
