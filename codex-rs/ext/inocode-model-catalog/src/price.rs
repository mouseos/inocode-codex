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
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub upper_bound: bool,
    pub basis: String,
    pub observed_at: Option<i64>,
    pub n_samples: Option<u32>,
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
