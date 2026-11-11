//! `inocode/models/*`: benchmarks and prices for the models in the picker.
//! Contract: ref/docs/16-account-pool-contract.md (chapter 10).

use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeModelRef {
    /// InoCode's provider id (`openai`, `claude`, ...).
    pub provider: String,
    /// The model id as the provider names it (`claude-sonnet-5-5`).
    pub model_id: String,
    /// `chatgpt`, `claudeOauth` or `apiKey`: how the model is paid for. Only a display hint.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub auth_kind: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeModelsEnrichParams {
    pub models: Vec<InocodeModelRef>,
    /// Only these uses (`coding`, `reasoning`, `math`, `agentic`, `long_context`, `general`,
    /// `speed`, or a use the user added). Omitted: all.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub uses: Option<Vec<String>>,
    /// Read the public sources again even if the cache is fresh (still held back for ten minutes
    /// after the last attempt).
    #[serde(default)]
    #[ts(optional = nullable)]
    pub refresh: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeBenchmarkMatch {
    /// `llmbenchmark`.
    pub source: String,
    /// The model's id in that source (`anthropic/claude-sonnet-5.5`).
    pub source_id: String,
    /// `exact` or `normalized_id`.
    pub matched_by: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeModelScore {
    /// The use this metric answers. A metric listed under two uses appears once per use.
    #[serde(rename = "use")]
    #[ts(rename = "use")]
    pub use_name: String,
    pub metric: String,
    /// The benchmark the value comes from (`livebench`, `epoch`, ...).
    pub board: String,
    pub value: f64,
    /// `ratio`, `points`, `elo`, `count`, `usd`, ...
    pub unit: String,
    /// The measurement condition; for speed, the serving provider.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub setting: Option<String>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub observed_at: Option<String>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub source_url: Option<String>,
    /// How many models have a value for this metric.
    pub n_models_in_metric: u32,
    /// 1 = best among those models. Only values of one metric are comparable.
    pub rank: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum InocodePriceKind {
    /// A per-token list price.
    PerTokenList,
    /// Not known: the amounts are absent (never 0).
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodePriceTier {
    #[ts(type = "number")]
    pub above_prompt_tokens: i64,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub input: Option<f64>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub output: Option<f64>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub cache_write: Option<f64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodePriceSource {
    pub name: String,
    pub url: String,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub path: Option<String>,
}

/// A list price from a public source. It is a reference, not what the user is charged.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeReferencePrice {
    pub kind: InocodePriceKind,
    /// `usd_per_1m_tokens`.
    pub unit: String,
    /// Always true: this is a reference price.
    pub reference: bool,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub input: Option<f64>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub output: Option<f64>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub cache_write: Option<f64>,
    /// Prices that apply above a prompt size.
    pub tiers: Vec<InocodePriceTier>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub source: Option<InocodePriceSource>,
    /// Unix seconds.
    #[serde(default)]
    #[ts(type = "number | null", optional = nullable)]
    pub fetched_at: Option<i64>,
    /// The model's release date in the source.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub source_data_date: Option<String>,
    /// For `unknown`: `not_fetched`, `not_listed` or `zero_cost_not_free` (a cost of 0 is not
    /// free: subscription providers list 0).
    #[serde(default)]
    #[ts(optional = nullable)]
    pub reason: Option<String>,
}

/// A price estimated from the user's own usage. Never mixed with the reference price. For a
/// subscription it is an upper bound (other devices' usage is included). Reserved: the usage
/// statistics fill it; until then it is absent.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeMeasuredPrice {
    #[serde(default)]
    #[ts(optional = nullable)]
    pub input: Option<f64>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub output: Option<f64>,
    pub upper_bound: bool,
    pub basis: String,
    #[serde(default)]
    #[ts(type = "number | null", optional = nullable)]
    pub observed_at: Option<i64>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub n_samples: Option<u32>,
}

/// A price the user set by hand. A fetch never overwrites it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodePriceOverride {
    #[serde(default)]
    #[ts(optional = nullable)]
    pub input: Option<f64>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub output: Option<f64>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub cache_write: Option<f64>,
    #[ts(type = "number")]
    pub set_at: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeModelPrice {
    pub reference: InocodeReferencePrice,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub measured: Option<InocodeMeasuredPrice>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub user_override: Option<InocodePriceOverride>,
    /// `plan_included` (a subscription: the reference is what the API would charge), `api`, or
    /// `unknown`.
    pub billing: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeModelEnrichment {
    pub provider: String,
    pub model_id: String,
    /// How the model was found in the benchmark source; absent when it was not found (then
    /// `scores` is empty: unmeasured, not zero).
    #[serde(default)]
    #[ts(optional = nullable)]
    pub benchmark_match: Option<InocodeBenchmarkMatch>,
    pub scores: Vec<InocodeModelScore>,
    pub price: InocodeModelPrice,
}

/// A source behind the answer, with what its license asks to be shown.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeModelSource {
    pub name: String,
    pub url: String,
    pub license: String,
    /// Show this text where the data is shown (CC BY 4.0 requires it).
    #[serde(default)]
    #[ts(optional = nullable)]
    pub attribution: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeModelsEnrichResponse {
    pub models: Vec<InocodeModelEnrichment>,
    pub sources: Vec<InocodeModelSource>,
    /// The flag `model_enrichment.fetch` (default on). Off: only the cache is used.
    pub fetch_enabled: bool,
    /// Unix seconds of the oldest data behind the answer; absent when nothing was read.
    #[serde(default)]
    #[ts(type = "number | null", optional = nullable)]
    pub fetched_at: Option<i64>,
    /// Some data is older than its cache window (a read failed, or fetching is off).
    pub stale: bool,
    /// Which metrics answer which use (editable in `model-use-map.json`).
    pub use_map: HashMap<String, Vec<String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeModelsPriceOverrideParams {
    pub provider: String,
    pub model_id: String,
    /// Absent or null clears the user's price. Amounts are USD per 1M tokens.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub price: Option<InocodePriceOverrideValue>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodePriceOverrideValue {
    #[serde(default)]
    #[ts(optional = nullable)]
    pub input: Option<f64>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub output: Option<f64>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub cache_write: Option<f64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeModelsPriceOverrideResponse {}
