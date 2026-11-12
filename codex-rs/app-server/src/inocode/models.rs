//! `inocode/models/*`: benchmarks and prices for the models in the picker.
//!
//! The reading, matching and caching are in `codex-inocode-model-catalog`; this file only maps
//! requests and answers. Fetching public sources is a flag (`model_enrichment.fetch`, default on).

use super::InocodeResult;
use super::unknown_method;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeBenchmarkMatch;
use codex_app_server_protocol::InocodeMeasuredPrice;
use codex_app_server_protocol::InocodeModelEnrichment;
use codex_app_server_protocol::InocodeModelPrice;
use codex_app_server_protocol::InocodeModelScore;
use codex_app_server_protocol::InocodeModelSource;
use codex_app_server_protocol::InocodeModelTheos;
use codex_app_server_protocol::InocodeModelsEnrichParams;
use codex_app_server_protocol::InocodeModelsEnrichResponse;
use codex_app_server_protocol::InocodeModelsPriceOverrideParams;
use codex_app_server_protocol::InocodeModelsPriceOverrideResponse;
use codex_app_server_protocol::InocodePriceKind;
use codex_app_server_protocol::InocodePriceOverride;
use codex_app_server_protocol::InocodePriceSource;
use codex_app_server_protocol::InocodePriceTier;
use codex_app_server_protocol::InocodeReferencePrice;
use codex_inocode_model_catalog as catalog;
use codex_inocode_stats::StatsStore;
use std::path::Path;

/// More models than the picker can hold; keeps one request bounded.
const MAX_MODELS: usize = 500;

pub(crate) async fn handle(request: ClientRequest) -> InocodeResult {
    match request {
        ClientRequest::InocodeModelsEnrich { params, .. } => {
            let home = codex_inocode_flags::inocode_home();
            let fetch_enabled = codex_inocode_flags::InocodeFlags::load_from(&home)
                .model_enrichment_fetch;
            Ok(Some(
                enrich(&home, fetch_enabled, catalog::Endpoints::default(), params)
                    .await?
                    .into(),
            ))
        }
        ClientRequest::InocodeModelsPriceOverride { params, .. } => {
            let home = codex_inocode_flags::inocode_home();
            price_override(&home, params)?;
            Ok(Some(InocodeModelsPriceOverrideResponse {}.into()))
        }
        other => Err(unknown_method(other.method_name())),
    }
}

async fn enrich(
    home: &Path,
    fetch_enabled: bool,
    endpoints: catalog::Endpoints,
    params: InocodeModelsEnrichParams,
) -> Result<InocodeModelsEnrichResponse, codex_app_server_protocol::JSONRPCErrorError> {
    if params.models.len() > MAX_MODELS {
        return Err(invalid_params(format!(
            "at most {MAX_MODELS} models per request"
        )));
    }
    let models: Vec<catalog::ModelRef> = params
        .models
        .into_iter()
        .map(|model| catalog::ModelRef {
            provider: model.provider,
            model_id: model.model_id,
            auth_kind: model.auth_kind,
        })
        .collect();
    let options = catalog::Options {
        fetch_enabled,
        refresh: params.refresh.unwrap_or(false),
        uses: params.uses,
    };
    // DeepAI models are free or cost Theos; the live catalog says which (read once if the
    // process has not read it yet).
    let wants_deepai = models
        .iter()
        .any(|model| model.provider == codex_inocode_deepai::PROVIDER_ID);
    if wants_deepai && codex_inocode_deepai::catalog::current().is_none() {
        match codex_inocode_deepai::catalog::fetch(&codex_inocode_deepai::base_url()).await {
            Ok(read) => codex_inocode_deepai::catalog::remember(&read),
            Err(error) => tracing::debug!("deepai model list read failed: {error}"),
        }
    }
    let mut result = catalog::Catalog::with_endpoints(home, endpoints)
        .enrich(&models, &options)
        .await;
    attach_measured(&mut result.models, &StatsStore::at(home));
    // The main agent reads a short line per model when it chooses a model for delegated work.
    for model in &result.models {
        codex_inocode_advisor::set_model_hint(&model.model_id, hint_line(model));
    }
    Ok(InocodeModelsEnrichResponse {
        models: result.models.into_iter().map(to_model).map(with_theos).collect(),
        sources: result
            .sources
            .into_iter()
            .map(|source| InocodeModelSource {
                name: source.name,
                url: source.url,
                license: source.license,
                attribution: source.attribution,
            })
            .collect(),
        fetch_enabled,
        fetched_at: result.fetched_at,
        stale: result.stale,
        use_map: result.use_map.into_iter().collect(),
    })
}

/// Two benchmark readings (best-ranked uses first) and the list price, as one short line:
/// `Benchmarks: coding 71 (rank 3 of 40), speed 120 tok/s (rank 5 of 38). List price $1.25 in /
/// $10 out per 1M tokens.` `None` when the model has neither.
fn hint_line(model: &catalog::ModelEnrichment) -> Option<String> {
    let mut seen: Vec<&str> = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    for score in &model.scores {
        if parts.len() == 2 {
            break;
        }
        if seen.contains(&score.use_name.as_str()) {
            continue;
        }
        seen.push(&score.use_name);
        let unit = if score.unit.is_empty() { String::new() } else { format!(" {}", score.unit) };
        parts.push(format!(
            "{} {}{unit} (rank {} of {})",
            score.use_name,
            format!("{:.4}", score.value).trim_end_matches('0').trim_end_matches('.'),
            score.rank,
            score.n_models_in_metric
        ));
    }
    let reference = &model.price.reference;
    let price = match (reference.input, reference.output) {
        (Some(input), Some(output)) => Some(format!(
            "List price ${input} in / ${output} out per 1M tokens."
        )),
        _ => None,
    };
    match (parts.is_empty(), price) {
        (true, None) => None,
        (true, Some(price)) => Some(price),
        (false, None) => Some(format!("Benchmarks: {}.", parts.join(", "))),
        (false, Some(price)) => Some(format!("Benchmarks: {}. {price}", parts.join(", "))),
    }
}

/// The Theos data of a DeepAI model, from the catalog the process last read. A model the catalog
/// does not list gets none.
fn with_theos(mut model: InocodeModelEnrichment) -> InocodeModelEnrichment {
    if model.provider != codex_inocode_deepai::PROVIDER_ID {
        return model;
    }
    let found = codex_inocode_deepai::catalog::current()
        .and_then(|catalog| catalog.find(&model.model_id).cloned());
    if let Some(found) = found {
        model.price.billing = if found.is_free() {
            "free".to_string()
        } else if found.is_metered() {
            "theos".to_string()
        } else {
            model.price.billing
        };
        model.theos = Some(InocodeModelTheos {
            is_free: found.is_free(),
            theos_per_request: found.theos_per_request.clone(),
            premier: found.premier == Some(true),
        });
        // A model paid for in Theos has no per-token price: the list price of the upstream model
        // and a cost worked out from it would be numbers that mean nothing here. Benchmarks stay.
        if matches!(model.price.billing.as_str(), "free" | "theos") {
            model.price.reference = InocodeReferencePrice {
                kind: InocodePriceKind::Unknown,
                unit: "usd_per_1m_tokens".to_string(),
                reference: true,
                input: None,
                output: None,
                cache_read: None,
                cache_write: None,
                tiers: Vec::new(),
                source: None,
                fetched_at: None,
                source_data_date: None,
                reason: Some("billed_in_theos".to_string()),
            };
            model.price.measured = None;
        }
    }
    model
}

/// How far back the recorded requests count toward a measured price.
const MEASURED_WINDOW_DAYS: u64 = 30;

/// Prices the recorded live requests of each model at its reference price.
fn attach_measured(models: &mut [catalog::ModelEnrichment], store: &StatsStore) {
    let since_ms = codex_inocode_stats::now_ms().saturating_sub(MEASURED_WINDOW_DAYS * 86_400_000);
    let records = store.read_since(since_ms);
    for model in models {
        let usage: Vec<catalog::RequestUsage> = records
            .iter()
            .filter(|r| r.source == "live" && r.provider == model.provider && r.model == model.model_id)
            .map(|r| catalog::RequestUsage {
                ts_ms: r.ts_ms,
                uncached_input: r.uncached_input,
                cached_input: r.cached_input,
                output: r.output,
            })
            .collect();
        model.price.measured = catalog::measure(&model.price.reference, &usage, &model.price.billing);
    }
}

fn to_model(model: catalog::ModelEnrichment) -> InocodeModelEnrichment {
    let reference = model.price.reference;
    InocodeModelEnrichment {
        provider: model.provider,
        model_id: model.model_id,
        benchmark_match: model.benchmark_match.map(|found| InocodeBenchmarkMatch {
            source: found.source,
            source_id: found.source_id,
            matched_by: found.matched_by,
        }),
        scores: model
            .scores
            .into_iter()
            .map(|score| InocodeModelScore {
                use_name: score.use_name,
                metric: score.metric,
                board: score.board,
                value: score.value,
                unit: score.unit,
                setting: score.setting,
                observed_at: score.observed_at,
                source_url: score.source_url,
                n_models_in_metric: score.n_models_in_metric,
                rank: score.rank,
            })
            .collect(),
        price: InocodeModelPrice {
            reference: InocodeReferencePrice {
                kind: match reference.kind {
                    catalog::PriceKind::PerTokenList => InocodePriceKind::PerTokenList,
                    catalog::PriceKind::Unknown => InocodePriceKind::Unknown,
                },
                unit: "usd_per_1m_tokens".to_string(),
                reference: true,
                input: reference.input,
                output: reference.output,
                cache_read: reference.cache_read,
                cache_write: reference.cache_write,
                tiers: reference
                    .tiers
                    .into_iter()
                    .map(|tier| InocodePriceTier {
                        above_prompt_tokens: tier.above_prompt_tokens,
                        input: tier.input,
                        output: tier.output,
                        cache_read: tier.cache_read,
                        cache_write: tier.cache_write,
                    })
                    .collect(),
                source: reference.source.map(|source| InocodePriceSource {
                    name: source.name,
                    url: source.url,
                    path: source.path,
                }),
                fetched_at: reference.fetched_at,
                source_data_date: reference.source_data_date,
                reason: reference.reason,
            },
            measured: model.price.measured.map(|measured| InocodeMeasuredPrice {
                input: measured.input,
                output: measured.output,
                upper_bound: measured.upper_bound,
                basis: measured.basis,
                observed_at: measured.observed_at,
                n_samples: measured.n_samples,
                total_cost_usd: measured.total_cost_usd,
                uncached_input_tokens: measured.uncached_input_tokens,
                cached_input_tokens: measured.cached_input_tokens,
                output_tokens: measured.output_tokens,
            }),
            user_override: model.price.user_override.map(|user| InocodePriceOverride {
                input: user.input,
                output: user.output,
                cache_read: user.cache_read,
                cache_write: user.cache_write,
                set_at: user.set_at,
            }),
            billing: model.price.billing,
        },
        theos: None,
    }
}

fn price_override(
    home: &Path,
    params: InocodeModelsPriceOverrideParams,
) -> Result<(), codex_app_server_protocol::JSONRPCErrorError> {
    if params.provider.trim().is_empty() || params.model_id.trim().is_empty() {
        return Err(invalid_params("provider and modelId must not be empty"));
    }
    let value = match params.price {
        None => None,
        Some(price) => {
            let amounts = [price.input, price.output, price.cache_read, price.cache_write];
            if amounts.iter().flatten().any(|v| !v.is_finite() || *v < 0.0) {
                return Err(invalid_params("amounts must be zero or more"));
            }
            Some(catalog::PriceOverride {
                input: price.input,
                output: price.output,
                cache_read: price.cache_read,
                cache_write: price.cache_write,
                set_at: chrono::Utc::now().timestamp(),
            })
        }
    };
    catalog::write_override(home, &params.provider, &params.model_id, value)
        .map_err(|error| internal_error(format!("failed to save the price: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hint_line_is_short_and_has_scores_and_the_list_price() {
        let model = catalog::ModelEnrichment {
            provider: "openai".into(),
            model_id: "gpt-x".into(),
            benchmark_match: None,
            scores: vec![
                catalog::Score {
                    use_name: "coding".into(),
                    metric: "m".into(),
                    board: "b".into(),
                    value: 71.5,
                    unit: String::new(),
                    setting: None,
                    observed_at: None,
                    source_url: None,
                    n_models_in_metric: 40,
                    rank: 3,
                },
                catalog::Score {
                    use_name: "coding".into(),
                    metric: "m2".into(),
                    board: "b".into(),
                    value: 60.0,
                    unit: String::new(),
                    setting: None,
                    observed_at: None,
                    source_url: None,
                    n_models_in_metric: 40,
                    rank: 9,
                },
            ],
            price: catalog::Price {
                reference: catalog::ReferencePrice {
                    kind: catalog::PriceKind::PerTokenList,
                    input: Some(1.25),
                    output: Some(10.0),
                    cache_read: None,
                    cache_write: None,
                    tiers: Vec::new(),
                    source: None,
                    fetched_at: None,
                    source_data_date: None,
                    reason: None,
                },
                measured: None,
                user_override: None,
                billing: "api".into(),
            },
        };
        assert_eq!(
            hint_line(&model).as_deref(),
            Some("Benchmarks: coding 71.5 (rank 3 of 40). List price $1.25 in / $10 out per 1M tokens.")
        );
        let mut bare = model.clone();
        bare.scores.clear();
        bare.price.reference.input = None;
        assert_eq!(hint_line(&bare), None);
    }
    use codex_app_server_protocol::InocodeModelRef;
    use pretty_assertions::assert_eq;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    use wiremock::matchers::query_param;

    #[tokio::test]
    async fn the_answer_carries_scores_price_sources_and_the_flag() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/scores"))
            .and(query_param("metric", "livebench_coding"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"modelId": "openai/gpt-6-astra", "source": "livebench", "value": 0.875,
                 "unit": "ratio", "observedAt": "2026-11-11"}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "openai": {"models": {"gpt-6-astra": {"cost": {"input": 10, "output": 50}}}}
            })))
            .mount(&server)
            .await;
        let home = tempfile::tempdir().expect("tempdir");
        let endpoints = catalog::Endpoints {
            llmbenchmark_base: server.uri(),
            modelsdev_url: format!("{}/api.json", server.uri()),
        };
        let answer = enrich(
            home.path(),
            true,
            endpoints,
            InocodeModelsEnrichParams {
                models: vec![InocodeModelRef {
                    provider: "openai".to_string(),
                    model_id: "gpt-6-astra".to_string(),
                    auth_kind: Some("chatgpt".to_string()),
                }],
                uses: Some(vec!["coding".to_string()]),
                refresh: None,
            },
        )
        .await
        .expect("answer");
        assert!(answer.fetch_enabled);
        let model = &answer.models[0];
        assert_eq!(model.scores[0].use_name, "coding");
        assert_eq!(model.price.reference.input, Some(10.0));
        assert!(model.price.reference.reference);
        assert_eq!(model.price.billing, "plan_included");
        assert!(answer.sources.iter().any(|s| s.attribution.is_some()));
    }

    #[test]
    fn a_price_set_by_hand_is_checked_and_can_be_cleared() {
        let home = tempfile::tempdir().expect("tempdir");
        let set = |price| InocodeModelsPriceOverrideParams {
            provider: "openai".to_string(),
            model_id: "gpt-6-astra".to_string(),
            price,
        };
        let bad = codex_app_server_protocol::InocodePriceOverrideValue {
            input: Some(-1.0),
            output: None,
            cache_read: None,
            cache_write: None,
        };
        assert!(price_override(home.path(), set(Some(bad))).is_err());
        let good = codex_app_server_protocol::InocodePriceOverrideValue {
            input: Some(7.0),
            output: Some(33.0),
            cache_read: None,
            cache_write: None,
        };
        price_override(home.path(), set(Some(good))).expect("set");
        assert_eq!(catalog::read_overrides(home.path()).len(), 1);
        price_override(home.path(), set(None)).expect("clear");
        assert_eq!(catalog::read_overrides(home.path()).len(), 0);
    }

    #[test]
    fn a_theos_model_has_no_list_price_or_measured_cost_but_keeps_its_benchmarks() {
        let catalog = codex_inocode_deepai::catalog::parse(
            r#"{"models":[
              {"id":"free-one","name":"F","input":["text"],"contextWindow":10,"maxTokens":5,"theosPerRequest":"0","premier":false},
              {"id":"paid-one","name":"P","input":["text"],"contextWindow":10,"maxTokens":5,"theosPerRequest":"10","premier":true}
            ]}"#,
        )
        .expect("catalog");
        codex_inocode_deepai::catalog::remember(&catalog);
        let priced = |provider: &str, id: &str| InocodeModelEnrichment {
            provider: provider.to_string(),
            model_id: id.to_string(),
            benchmark_match: None,
            scores: vec![InocodeModelScore {
                use_name: "coding".to_string(),
                metric: "m".to_string(),
                board: "b".to_string(),
                value: 1.0,
                unit: "points".to_string(),
                setting: None,
                observed_at: None,
                source_url: None,
                n_models_in_metric: 2,
                rank: 1,
            }],
            price: InocodeModelPrice {
                reference: InocodeReferencePrice {
                    kind: InocodePriceKind::PerTokenList,
                    unit: "usd_per_1m_tokens".to_string(),
                    reference: true,
                    input: Some(1.0),
                    output: Some(2.0),
                    cache_read: None,
                    cache_write: None,
                    tiers: Vec::new(),
                    source: None,
                    fetched_at: None,
                    source_data_date: None,
                    reason: None,
                },
                measured: Some(InocodeMeasuredPrice {
                    input: Some(1.0),
                    output: Some(2.0),
                    upper_bound: false,
                    basis: "x".to_string(),
                    observed_at: None,
                    n_samples: Some(1),
                    total_cost_usd: Some(0.5),
                    uncached_input_tokens: None,
                    cached_input_tokens: None,
                    output_tokens: None,
                }),
                user_override: None,
                billing: "unknown".to_string(),
            },
            theos: None,
        };
        for id in ["free-one", "paid-one"] {
            let out = with_theos(priced("deepai", id));
            assert!(out.theos.is_some());
            assert_eq!(out.price.reference.kind, InocodePriceKind::Unknown);
            assert_eq!(out.price.reference.input, None);
            assert_eq!(out.price.reference.reason.as_deref(), Some("billed_in_theos"));
            assert!(out.price.measured.is_none());
            assert_eq!(out.scores.len(), 1, "benchmarks stay");
        }
        // Another provider's model keeps its price and measured cost.
        let other = with_theos(priced("openai", "free-one"));
        assert_eq!(other.price.reference.kind, InocodePriceKind::PerTokenList);
        assert!(other.price.measured.is_some());
    }
}
