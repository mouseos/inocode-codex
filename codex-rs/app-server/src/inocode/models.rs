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
    let result = catalog::Catalog::with_endpoints(home, endpoints)
        .enrich(&models, &options)
        .await;
    Ok(InocodeModelsEnrichResponse {
        models: result.models.into_iter().map(to_model).collect(),
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
}
