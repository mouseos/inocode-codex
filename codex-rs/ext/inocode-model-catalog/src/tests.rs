#![allow(clippy::expect_used)]

use super::Catalog;
use super::Endpoints;
use super::ModelRef;
use super::Options;
use super::PriceKind;
use super::PriceOverride;
use super::load_use_map;
use super::normalize::normalize_model;
use super::write_override;
use pretty_assertions::assert_eq;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::matchers::query_param;

fn model(provider: &str, id: &str, auth: Option<&str>) -> ModelRef {
    ModelRef {
        provider: provider.to_string(),
        model_id: id.to_string(),
        auth_kind: auth.map(str::to_string),
    }
}

fn coding_rows() -> serde_json::Value {
    let row = |id: &str, value: f64| {
        json!({
            "modelId": id, "source": "livebench", "metric": "livebench_coding", "value": value,
            "unit": "ratio", "observedAt": "2026-11-11", "sourceUrl": "https://livebench.ai/x.csv"
        })
    };
    json!([
        row("openai/gpt-6-astra", 0.875),
        row("anthropic/claude-sonnet-5.5", 0.772),
        row("anthropic/claude-opus-5.5", 0.871),
        row("openai/gpt-6-astra-pro", 0.9),
        {"modelId": "bad", "value": "x"}
    ])
}

fn prices() -> serde_json::Value {
    json!({
        "openai": {"id": "openai", "models": {
            "gpt-6-astra": {"release_date": "2026-10-01", "cost": {
                "input": 10, "output": 50, "cache_read": 1, "cache_write": 12.5,
                "tiers": [{"input": 20, "output": 75, "cache_read": 2, "cache_write": 25,
                           "tier": {"type": "context", "size": 272000}}]}},
            "gpt-5.4-mini": {"cost": {"input": 0.75, "output": 4.5}}
        }},
        "anthropic": {"id": "anthropic", "models": {
            "claude-sonnet-5-5": {"release_date": "2026-09-28",
                                  "cost": {"input": 2, "output": 10, "cache_read": 0.1}}
        }},
        "alibaba-token-plan": {"id": "alibaba-token-plan", "models": {
            "qwen3.8-max": {"cost": {"input": 0, "output": 0}}
        }},
        "alibaba": {"id": "alibaba", "models": {"qwen3.8-max": {"name": "no cost key"}}}
    })
}

async fn server(expected_reads: u64) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/scores"))
        .and(query_param("metric", "livebench_coding"))
        .respond_with(ResponseTemplate::new(200).set_body_json(coding_rows()))
        .expect(expected_reads)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(prices()))
        .expect(expected_reads)
        .mount(&server)
        .await;
    server
}

fn endpoints(server: &MockServer) -> Endpoints {
    Endpoints {
        llmbenchmark_base: server.uri(),
        modelsdev_url: format!("{}/api.json", server.uri()),
    }
}

fn coding_only(fetch: bool, refresh: bool) -> Options {
    Options {
        fetch_enabled: fetch,
        refresh,
        uses: Some(vec!["coding".to_string()]),
    }
}

#[test]
fn ids_are_normalised_without_equating_variants() {
    assert_eq!(normalize_model("gpt-5.4-mini"), normalize_model("GPT-5-4-mini"));
    assert_eq!(normalize_model("claude-sonnet-5.5"), "claude-sonnet-5-5");
    assert_ne!(normalize_model("gpt-6-astra"), normalize_model("gpt-6-astra-pro"));
    assert_ne!(normalize_model("claude-x-batch"), normalize_model("claude-x"));
    assert_eq!(normalize_model("a.b"), "a.b", "only a dot between digits changes");
}

#[tokio::test]
async fn a_model_gets_its_scores_with_a_rank_and_a_provider_exact_price() {
    let server = server(1).await;
    let home = tempfile::tempdir().expect("tempdir");
    let catalog = Catalog::with_endpoints(home.path(), endpoints(&server));
    let out = catalog
        .enrich(
            &[
                model("openai", "gpt-6-astra", Some("chatgpt")),
                model("claude", "claude-sonnet-5-5", Some("claudeOauth")),
                model("openai", "gpt-9-unknown", None),
            ],
            &coding_only(true, false),
        )
        .await;

    let astra = &out.models[0];
    assert_eq!(astra.scores.len(), 1);
    let score = &astra.scores[0];
    assert_eq!(
        (score.use_name.as_str(), score.metric.as_str(), score.board.as_str()),
        ("coding", "livebench_coding", "livebench")
    );
    assert_eq!((score.value, score.unit.as_str()), (0.875, "ratio"));
    // gpt-6-astra-pro (0.9) is a different model and is ahead of it; the bad row is dropped.
    assert_eq!((score.rank, score.n_models_in_metric), (2, 4));
    assert_eq!(astra.benchmark_match.as_ref().expect("match").matched_by, "exact");
    assert_eq!(astra.price.reference.kind, PriceKind::PerTokenList);
    assert_eq!(astra.price.reference.input, Some(10.0));
    assert_eq!(astra.price.reference.tiers.len(), 1);
    assert_eq!(astra.price.reference.tiers[0].above_prompt_tokens, 272_000);
    assert_eq!(astra.price.billing, "plan_included");
    assert!(astra.price.measured.is_none(), "a measured price is never made here");

    // `claude` is `anthropic` in the sources, and `5.5` / `5-5` are the same version.
    let sonnet = &out.models[1];
    assert_eq!(sonnet.scores[0].rank, 4);
    assert_eq!(sonnet.benchmark_match.as_ref().expect("match").matched_by, "normalized_id");
    assert_eq!(sonnet.benchmark_match.as_ref().expect("match").source_id, "anthropic/claude-sonnet-5.5");
    assert_eq!(sonnet.price.reference.cache_read, Some(0.1));

    // Not found: no scores, an unknown price (not zero).
    let unknown = &out.models[2];
    assert!(unknown.scores.is_empty() && unknown.benchmark_match.is_none());
    assert_eq!(unknown.price.reference.kind, PriceKind::Unknown);
    assert_eq!(unknown.price.reference.input, None);
    assert_eq!(unknown.price.reference.reason.as_deref(), Some("not_listed"));

    assert!(!out.stale);
    let names: Vec<&str> = out.sources.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["LLMBenchmark.ai", "models.dev"]);
    assert!(out.sources[0].attribution.is_some(), "CC BY asks for attribution");
}

#[tokio::test]
async fn a_cost_of_zero_is_unknown_and_another_providers_price_is_not_borrowed() {
    let server = server(1).await;
    let home = tempfile::tempdir().expect("tempdir");
    let catalog = Catalog::with_endpoints(home.path(), endpoints(&server));
    let out = catalog
        .enrich(
            &[
                model("alibaba-token-plan", "qwen3.8-max", None),
                model("alibaba", "qwen3.8-max", None),
            ],
            &coding_only(true, false),
        )
        .await;
    assert_eq!(out.models[0].price.reference.reason.as_deref(), Some("zero_cost_not_free"));
    assert_eq!(out.models[0].price.reference.input, None);
    // `alibaba` lists the model without a cost: the token-plan row's 0 is not used for it.
    assert_eq!(out.models[1].price.reference.reason.as_deref(), Some("not_listed"));
}

#[tokio::test]
async fn reads_are_cached_not_repeated_and_can_be_switched_off() {
    // Each source is read once (the mock expects exactly one read of each).
    let server = server(1).await;
    let home = tempfile::tempdir().expect("tempdir");
    let catalog = Catalog::with_endpoints(home.path(), endpoints(&server));
    let models = [model("openai", "gpt-6-astra", None)];
    let first = catalog.enrich(&models, &coding_only(true, false)).await;
    let again = catalog.enrich(&models, &coding_only(true, false)).await;
    // Even an explicit refresh is held back for ten minutes after the last attempt.
    let forced = catalog.enrich(&models, &coding_only(true, true)).await;
    // With fetching off, only the cache is used.
    let off = catalog.enrich(&models, &coding_only(false, false)).await;
    for out in [&again, &forced, &off] {
        assert_eq!(out.models[0].scores.len(), 1);
        assert_eq!(out.models[0].price.reference.input, Some(10.0));
    }
    assert_eq!(first.fetched_at, off.fetched_at);

    // A fresh home with fetching off makes no request and has nothing.
    let empty_home = tempfile::tempdir().expect("tempdir");
    let quiet = MockServer::start().await;
    let quiet_catalog = Catalog::with_endpoints(empty_home.path(), endpoints(&quiet));
    let nothing = quiet_catalog.enrich(&models, &coding_only(false, false)).await;
    assert!(nothing.models[0].scores.is_empty());
    assert_eq!(nothing.models[0].price.reference.reason.as_deref(), Some("not_fetched"));
    assert!(nothing.sources.is_empty());
    assert_eq!(nothing.fetched_at, None);
    assert!(quiet.received_requests().await.expect("requests").is_empty());
}

#[tokio::test]
async fn a_price_the_user_set_is_kept_and_never_touched_by_a_fetch() {
    let server = server(1).await;
    let home = tempfile::tempdir().expect("tempdir");
    write_override(
        home.path(),
        "OpenAI",
        "gpt-6-astra",
        Some(PriceOverride {
            input: Some(7.0),
            output: Some(33.0),
            cache_read: None,
            cache_write: None,
            set_at: 1_794_000_000,
        }),
    )
    .expect("write");
    let catalog = Catalog::with_endpoints(home.path(), endpoints(&server));
    let out = catalog
        .enrich(&[model("openai", "gpt-6-astra", None)], &coding_only(true, false))
        .await;
    let price = &out.models[0].price;
    let user = price.user_override.as_ref().expect("override");
    assert_eq!((user.input, user.output), (Some(7.0), Some(33.0)));
    // The reference price is still the source's, not the user's.
    assert_eq!(price.reference.input, Some(10.0));

    write_override(home.path(), "openai", "gpt-6-astra", None).expect("clear");
    let cleared = catalog
        .enrich(&[model("openai", "gpt-6-astra", None)], &coding_only(false, false))
        .await;
    assert!(cleared.models[0].price.user_override.is_none());
}

#[test]
fn the_use_table_can_be_edited_and_bad_names_are_dropped() {
    let home = tempfile::tempdir().expect("tempdir");
    let default = load_use_map(home.path());
    assert!(default["coding"].contains(&"livebench_coding".to_string()));
    assert!(default["long_context"].is_empty());
    std::fs::write(
        home.path().join("model-use-map.json"),
        r#"{"coding": ["my_metric", "bad metric!", "../x"], "long_context": ["aa_long"], "Bad Use": ["x"]}"#,
    )
    .expect("write");
    let custom = load_use_map(home.path());
    assert_eq!(custom["coding"], vec!["my_metric".to_string()]);
    assert_eq!(custom["long_context"], vec!["aa_long".to_string()]);
    assert_eq!(custom["math"], default["math"], "other uses keep their defaults");
    assert!(!custom.contains_key("Bad Use"));
}

/// Reads the real public sources once (GET only, no credentials). Run by hand:
/// `cargo test -p codex-inocode-model-catalog -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "reads the real public sources"]
async fn real_sources_smoke() {
    let home = tempfile::tempdir().expect("tempdir");
    let catalog = Catalog::new(home.path());
    let out = catalog
        .enrich(
            &[
                model("openai", "gpt-6-astra", Some("chatgpt")),
                model("claude", "claude-sonnet-5-5", Some("claudeOauth")),
                model("claude", "claude-haiku-5-5", Some("claudeOauth")),
                model("openai", "gpt-5.4-mini", None),
            ],
            &coding_only(true, false),
        )
        .await;
    for entry in &out.models {
        eprintln!(
            "SMOKE {}/{} match={:?} scores={} price={:?} input={:?} output={:?} reason={:?}",
            entry.provider,
            entry.model_id,
            entry.benchmark_match.as_ref().map(|m| m.source_id.as_str()),
            entry.scores.len(),
            entry.price.reference.kind,
            entry.price.reference.input,
            entry.price.reference.output,
            entry.price.reference.reason,
        );
        for score in &entry.scores {
            eprintln!(
                "SMOKE   {} {} = {} {} rank {}/{}",
                score.use_name, score.metric, score.value, score.unit, score.rank, score.n_models_in_metric
            );
        }
    }
    eprintln!("SMOKE sources={:?} stale={}", out.sources, out.stale);
}
