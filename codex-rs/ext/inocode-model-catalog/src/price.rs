//! Prices: a reference list price from models.dev, kept apart from a measured price and from the
//! user's own override.
//!
//! models.dev lists a cost per provider and model (USD per 1M tokens). A model it does not list
//! for that provider is `unknown`. A cost of 0 / 0 is not free: subscription providers list 0
//! there, so it is `unknown` too (reason `zero_cost_not_free`). Another provider's price is never
//! borrowed for the same model name.

use crate::Endpoints;
use crate::ModelRef;
use crate::PriceOverride;
use crate::cache::Fetcher;
use crate::cache::get_json;
use crate::normalize::normalize_model;
use crate::normalize::source_provider;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;

const MODELSDEV_TTL_SECS: i64 = 24 * 3600;
const MODELSDEV_PAGE: &str = "https://models.dev/api.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PriceKind {
    /// A per-token list price.
    PerTokenList,
    /// Nothing is known: the amounts are `None`.
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PriceTier {
    /// The price applies to a prompt above this many tokens.
    pub above_prompt_tokens: i64,
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceSource {
    pub name: String,
    pub url: String,
    pub path: Option<String>,
}

/// A list price from a public source. Always `reference: true` on the wire.
#[derive(Clone, Debug, PartialEq)]
pub struct ReferencePrice {
    pub kind: PriceKind,
    /// USD per 1M tokens.
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    pub tiers: Vec<PriceTier>,
    pub source: Option<PriceSource>,
    pub fetched_at: Option<i64>,
    /// The model's release date in the source (not the date of the price).
    pub source_data_date: Option<String>,
    /// Why the price is unknown: `not_fetched`, `not_listed`, `zero_cost_not_free`.
    pub reason: Option<String>,
}

/// A price estimated from the user's own usage. This crate never makes one; the usage statistics
/// do, and a subscription's figure is an upper bound (other devices' usage is mixed in).
#[derive(Clone, Debug, PartialEq)]
pub struct MeasuredPrice {
    /// The effective input price, USD per 1M input tokens (cached and uncached together):
    /// the cost of the input actually sent divided by the input tokens.
    pub input: Option<f64>,
    /// The effective output price, USD per 1M output tokens.
    pub output: Option<f64>,
    pub upper_bound: bool,
    pub basis: String,
    /// Unix seconds of the newest request counted.
    pub observed_at: Option<i64>,
    pub n_samples: Option<u32>,
    /// What the counted requests would cost at the reference price, in USD.
    pub total_cost_usd: Option<f64>,
    pub uncached_input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// One request's tokens, as recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestUsage {
    pub ts_ms: u64,
    pub uncached_input: u64,
    pub cached_input: u64,
    /// Output tokens as recorded (reasoning tokens are taken to be inside this number).
    pub output: u64,
}

/// The cost of the recorded requests at the reference price, and the effective unit prices that
/// follow from it. `None` when the reference price has no amounts (nothing is made up) or no
/// request was recorded.
///
/// Per request: the tier whose `above_prompt_tokens` the prompt (uncached + cached input)
/// exceeds is used when there is one; cached input is priced at `cache_read` (at the input price
/// when the source lists none). This is a price from list prices and counted tokens, not what a
/// bill said; for a subscription (`plan_included`) it is an upper bound, because other devices'
/// usage is not seen.
pub fn measure(
    reference: &ReferencePrice,
    requests: &[RequestUsage],
    billing: &str,
) -> Option<MeasuredPrice> {
    if reference.kind != PriceKind::PerTokenList || requests.is_empty() {
        return None;
    }
    let (list_input, list_output) = (reference.input?, reference.output?);
    let mut cost = 0.0_f64;
    let (mut uncached, mut cached, mut output) = (0_u64, 0_u64, 0_u64);
    let (mut input_cost, mut output_cost) = (0.0_f64, 0.0_f64);
    for request in requests {
        let prompt = request.uncached_input.saturating_add(request.cached_input);
        let tier = reference
            .tiers
            .iter()
            .filter(|tier| i64::try_from(prompt).is_ok_and(|p| p > tier.above_prompt_tokens))
            .max_by_key(|tier| tier.above_prompt_tokens);
        let input_rate = tier.and_then(|t| t.input).unwrap_or(list_input);
        let output_rate = tier.and_then(|t| t.output).unwrap_or(list_output);
        let cache_rate = tier
            .and_then(|t| t.cache_read)
            .or(reference.cache_read)
            .unwrap_or(input_rate);
        let this_input =
            (request.uncached_input as f64 * input_rate + request.cached_input as f64 * cache_rate)
                / 1e6;
        let this_output = request.output as f64 * output_rate / 1e6;
        input_cost += this_input;
        output_cost += this_output;
        cost += this_input + this_output;
        uncached += request.uncached_input;
        cached += request.cached_input;
        output += request.output;
    }
    let input_tokens = uncached + cached;
    Some(MeasuredPrice {
        input: (input_tokens > 0).then(|| input_cost * 1e6 / input_tokens as f64),
        output: (output > 0).then(|| output_cost * 1e6 / output as f64),
        upper_bound: billing == "plan_included",
        basis: "recorded_tokens_x_reference_price".to_string(),
        observed_at: requests
            .iter()
            .map(|r| r.ts_ms)
            .max()
            .and_then(|ms| i64::try_from(ms / 1000).ok()),
        n_samples: u32::try_from(requests.len()).ok(),
        total_cost_usd: Some(cost),
        uncached_input_tokens: Some(uncached),
        cached_input_tokens: Some(cached),
        output_tokens: Some(output),
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct Price {
    pub reference: ReferencePrice,
    pub measured: Option<MeasuredPrice>,
    pub user_override: Option<PriceOverride>,
    /// `plan_included` (a subscription), `api` (pay per token), `unknown`: how the model is paid
    /// for, so the reference price can be read as "what the API would charge".
    pub billing: String,
}

pub(crate) fn billing_of(auth_kind: Option<&str>) -> String {
    match auth_kind {
        Some("chatgpt" | "claudeOauth") => "plan_included",
        Some("apiKey") => "api",
        _ => "unknown",
    }
    .to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CostRow {
    pub input: f64,
    pub output: f64,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    /// `(above_prompt_tokens, input, output, cache_read, cache_write)`.
    pub tiers: Vec<(i64, Option<f64>, Option<f64>, Option<f64>, Option<f64>)>,
    pub release_date: Option<String>,
}

pub(crate) type CostTable = BTreeMap<String, BTreeMap<String, CostRow>>;

pub(crate) struct Costs {
    pub data: Option<CostTable>,
    pub fetched_at: Option<i64>,
    pub stale: bool,
}

fn number(value: Option<&serde_json::Value>) -> Option<f64> {
    value
        .and_then(serde_json::Value::as_f64)
        .filter(|v| v.is_finite() && *v >= 0.0)
}

/// Keeps only what is needed: the cost of each model, per provider.
fn reduce(body: &serde_json::Value) -> CostTable {
    let mut table = CostTable::new();
    let Some(providers) = body.as_object() else {
        return table;
    };
    for (provider, entry) in providers {
        let Some(models) = entry.get("models").and_then(serde_json::Value::as_object) else {
            continue;
        };
        let mut rows = BTreeMap::new();
        for (key, model) in models {
            let Some(cost) = model.get("cost") else {
                continue;
            };
            let (Some(input), Some(output)) = (number(cost.get("input")), number(cost.get("output")))
            else {
                continue;
            };
            let tiers = cost
                .get("tiers")
                .and_then(serde_json::Value::as_array)
                .map(|tiers| {
                    tiers
                        .iter()
                        .filter_map(|tier| {
                            let info = tier.get("tier")?;
                            if info.get("type")?.as_str()? != "context" {
                                return None;
                            }
                            Some((
                                info.get("size")?.as_i64()?,
                                number(tier.get("input")),
                                number(tier.get("output")),
                                number(tier.get("cache_read")),
                                number(tier.get("cache_write")),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            rows.insert(
                key.clone(),
                CostRow {
                    input,
                    output,
                    cache_read: number(cost.get("cache_read")),
                    cache_write: number(cost.get("cache_write")),
                    tiers,
                    release_date: model
                        .get("release_date")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                },
            );
        }
        if !rows.is_empty() {
            table.insert(provider.clone(), rows);
        }
    }
    table
}

pub(crate) async fn load_costs(fetcher: &Fetcher, endpoints: &Endpoints) -> Costs {
    let url = endpoints.modelsdev_url.clone();
    let loaded = fetcher
        .load("modelsdev-costs", MODELSDEV_TTL_SECS, |client| async move {
            Ok(reduce(&get_json(&client, &url).await?))
        })
        .await;
    Costs {
        data: loaded.data,
        fetched_at: loaded.fetched_at,
        stale: loaded.stale,
    }
}

fn unknown(reason: &str, fetched_at: Option<i64>) -> ReferencePrice {
    ReferencePrice {
        kind: PriceKind::Unknown,
        input: None,
        output: None,
        cache_read: None,
        cache_write: None,
        tiers: Vec::new(),
        source: None,
        fetched_at,
        source_data_date: None,
        reason: Some(reason.to_string()),
    }
}

pub(crate) fn reference_for(
    table: Option<&CostTable>,
    fetched_at: Option<i64>,
    model: &ModelRef,
) -> ReferencePrice {
    let Some(table) = table else {
        return unknown("not_fetched", None);
    };
    let provider = source_provider(&model.provider);
    let Some(rows) = table.get(&provider) else {
        return unknown("not_listed", fetched_at);
    };
    let wanted = normalize_model(&model.model_id);
    let found = rows
        .get_key_value(&model.model_id)
        .or_else(|| rows.iter().find(|(key, _)| normalize_model(key) == wanted));
    let Some((key, row)) = found else {
        return unknown("not_listed", fetched_at);
    };
    if row.input == 0.0 && row.output == 0.0 {
        return unknown("zero_cost_not_free", fetched_at);
    }
    ReferencePrice {
        kind: PriceKind::PerTokenList,
        input: Some(row.input),
        output: Some(row.output),
        cache_read: row.cache_read,
        cache_write: row.cache_write,
        tiers: row
            .tiers
            .iter()
            .map(|(above, input, output, cache_read, cache_write)| PriceTier {
                above_prompt_tokens: *above,
                input: *input,
                output: *output,
                cache_read: *cache_read,
                cache_write: *cache_write,
            })
            .collect(),
        source: Some(PriceSource {
            name: "models.dev".to_string(),
            url: MODELSDEV_PAGE.to_string(),
            path: Some(format!("{provider}.models.{key}.cost")),
        }),
        fetched_at,
        source_data_date: row.release_date.clone(),
        reason: None,
    }
}

#[cfg(test)]
mod measure_tests {
    use super::*;

    fn reference(input: f64, output: f64, cache_read: Option<f64>) -> ReferencePrice {
        ReferencePrice {
            kind: PriceKind::PerTokenList,
            input: Some(input),
            output: Some(output),
            cache_read,
            cache_write: None,
            tiers: Vec::new(),
            source: None,
            fetched_at: None,
            source_data_date: None,
            reason: None,
        }
    }

    fn usage(uncached: u64, cached: u64, output: u64) -> RequestUsage {
        RequestUsage {
            ts_ms: 5_000,
            uncached_input: uncached,
            cached_input: cached,
            output,
        }
    }

    #[test]
    fn cost_is_tokens_times_the_listed_prices_and_the_effective_prices_follow() {
        // 1M uncached at $2, 1M cached at $0.5, 0.5M output at $8: 2 + 0.5 + 4 = 6.5.
        let measured = measure(
            &reference(2.0, 8.0, Some(0.5)),
            &[usage(1_000_000, 1_000_000, 500_000)],
            "api",
        )
        .expect("measured");
        assert!((measured.total_cost_usd.expect("cost") - 6.5).abs() < 1e-9);
        assert!((measured.input.expect("input") - 1.25).abs() < 1e-9);
        assert!((measured.output.expect("output") - 8.0).abs() < 1e-9);
        assert!(!measured.upper_bound);
        assert_eq!(measured.observed_at, Some(5));
        assert_eq!(measured.n_samples, Some(1));
        assert_eq!(measured.cached_input_tokens, Some(1_000_000));
    }

    #[test]
    fn a_subscription_figure_is_an_upper_bound_and_cache_defaults_to_the_input_price() {
        let measured =
            measure(&reference(2.0, 8.0, None), &[usage(0, 1_000_000, 0)], "plan_included")
                .expect("measured");
        assert!(measured.upper_bound);
        assert!((measured.total_cost_usd.expect("cost") - 2.0).abs() < 1e-9);
        assert_eq!(measured.output, None, "no output tokens, no output price");
    }

    #[test]
    fn a_tier_applies_when_the_prompt_is_above_its_threshold() {
        let mut price = reference(1.0, 4.0, None);
        price.tiers.push(PriceTier {
            above_prompt_tokens: 200_000,
            input: Some(2.0),
            output: Some(6.0),
            cache_read: None,
            cache_write: None,
        });
        let small = measure(&price, &[usage(100_000, 0, 1_000_000)], "api").expect("small");
        let large = measure(&price, &[usage(300_000, 0, 1_000_000)], "api").expect("large");
        assert!((small.output.expect("o") - 4.0).abs() < 1e-9);
        assert!((large.output.expect("o") - 6.0).abs() < 1e-9);
        assert!((large.input.expect("i") - 2.0).abs() < 1e-9);
    }

    #[test]
    fn nothing_is_made_up_without_a_price_or_without_requests() {
        let mut unknown = reference(1.0, 1.0, None);
        unknown.kind = PriceKind::Unknown;
        assert!(measure(&unknown, &[usage(1, 1, 1)], "api").is_none());
        assert!(measure(&reference(1.0, 1.0, None), &[], "api").is_none());
        let mut missing = reference(1.0, 1.0, None);
        missing.output = None;
        assert!(measure(&missing, &[usage(1, 1, 1)], "api").is_none());
    }
}
