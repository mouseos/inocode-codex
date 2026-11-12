//! Benchmarks and prices for the models the user can pick, from public APIs.
//!
//! What it answers: "what is this model good at" (scores by use: coding, reasoning, math,
//! agentic, long context, speed, general) and "what does it cost" (a reference list price).
//! Design and the measured behaviour of each source: ref/docs/37-model-benchmark-and-price-sources.md.
//!
//! Rules that hold everywhere in this crate:
//! - A model is matched by `(provider, model id)` after a small normalisation (case, and `.` vs
//!   `-` between digits). Suffixes such as `-pro`, `-batch` or `-preview` are never equated, and
//!   nothing is matched by similarity. A model that is not found has no scores and an `unknown`
//!   price: never zero, never an estimate.
//! - Scores are kept as the source gave them (metric, board, value, unit, setting, date, URL).
//!   No average across metrics is made; the use table (editable) only says which metrics answer
//!   which use.
//! - The reference price (a list price from a public source) and a measured price (from the
//!   user's own usage) are separate fields and never mixed. `userOverride` wins for display and
//!   is never overwritten by a fetch.
//! - Every network read is cached on disk and not repeated within a window; a fetch can be
//!   switched off, and then only the cache is used.

mod cache;
mod normalize;
mod overrides;
mod price;
mod scores;
mod usemap;

pub use overrides::PriceOverride;
pub use overrides::read_overrides;
pub use overrides::write_override;
pub use price::MeasuredPrice;
pub use price::RequestUsage;
pub use price::measure;
pub use price::Price;
pub use price::PriceKind;
pub use price::PriceSource;
pub use price::PriceTier;
pub use price::ReferencePrice;
pub use scores::Score;
pub use usemap::load_use_map;

use cache::Fetcher;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

/// Where each public source is read from. Tests point these at a mock server.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub llmbenchmark_base: String,
    pub modelsdev_url: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            llmbenchmark_base: "https://api.llmbenchmark.ai".to_string(),
            modelsdev_url: "https://models.dev/api.json".to_string(),
        }
    }
}

/// A model to look up. `provider` is InoCode's provider id (`openai`, `claude`, ...).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRef {
    pub provider: String,
    pub model_id: String,
    /// `chatgpt`, `claudeOauth` or `apiKey`: how the model is paid for. Only a hint for display.
    pub auth_kind: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// False: use only what is cached; make no request.
    pub fetch_enabled: bool,
    /// Read again even if the cache is fresh (still not more than once per ten minutes).
    pub refresh: bool,
    /// Only these uses (`coding`, ...). `None`: every use in the table.
    pub uses: Option<Vec<String>>,
}

/// How a model was matched in a source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub source: String,
    pub source_id: String,
    pub matched_by: String,
}

#[derive(Clone, Debug)]
pub struct ModelEnrichment {
    pub provider: String,
    pub model_id: String,
    /// How the model was matched in the benchmark source; `None` when it was not found there.
    pub benchmark_match: Option<Match>,
    pub scores: Vec<Score>,
    pub price: Price,
}

/// A source used for the answer, with what its license asks to be shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceInfo {
    pub name: String,
    pub url: String,
    pub license: String,
    /// Text to show when the license requires attribution.
    pub attribution: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Enrichment {
    pub models: Vec<ModelEnrichment>,
    pub sources: Vec<SourceInfo>,
    /// Unix seconds of the oldest data behind the answer; `None` when nothing was read.
    pub fetched_at: Option<i64>,
    /// Some data is older than its cache window (a fetch failed, or fetching is off).
    pub stale: bool,
    pub use_map: BTreeMap<String, Vec<String>>,
}

pub struct Catalog {
    home: PathBuf,
    endpoints: Endpoints,
}

impl Catalog {
    pub fn new(home: &Path) -> Self {
        Self::with_endpoints(home, Endpoints::default())
    }

    pub fn with_endpoints(home: &Path, endpoints: Endpoints) -> Self {
        Self {
            home: home.to_path_buf(),
            endpoints,
        }
    }

    pub async fn enrich(&self, models: &[ModelRef], options: &Options) -> Enrichment {
        let use_map = usemap::load_use_map(&self.home);
        let wanted: BTreeMap<String, Vec<String>> = use_map
            .iter()
            .filter(|(use_name, _)| {
                options
                    .uses
                    .as_ref()
                    .is_none_or(|uses| uses.iter().any(|u| u == *use_name))
            })
            .map(|(use_name, metrics)| (use_name.clone(), metrics.clone()))
            .collect();

        let fetcher = Fetcher::new(&self.home, options);
        let board = scores::load_board(&fetcher, &self.endpoints, &wanted).await;
        let costs = price::load_costs(&fetcher, &self.endpoints).await;
        let overrides = overrides::read_overrides(&self.home);

        let mut sources = Vec::new();
        if board.any_data {
            sources.push(SourceInfo {
                name: "LLMBenchmark.ai".to_string(),
                url: "https://llmbenchmark.ai".to_string(),
                license: "CC BY 4.0".to_string(),
                attribution: Some("LLMBenchmark.ai (https://llmbenchmark.ai)".to_string()),
            });
        }
        if costs.data.is_some() {
            sources.push(SourceInfo {
                name: "models.dev".to_string(),
                url: "https://models.dev".to_string(),
                license: "MIT".to_string(),
                attribution: None,
            });
        }

        let out = models
            .iter()
            .map(|model| {
                let (benchmark_match, scores) = scores::scores_for(&board, &wanted, model);
                let reference = price::reference_for(costs.data.as_ref(), costs.fetched_at, model);
                let user_override = overrides
                    .get(&overrides::key(&model.provider, &model.model_id))
                    .cloned();
                ModelEnrichment {
                    provider: model.provider.clone(),
                    model_id: model.model_id.clone(),
                    benchmark_match,
                    scores,
                    price: Price {
                        reference,
                        measured: None,
                        user_override,
                        billing: price::billing_of(model.auth_kind.as_deref()),
                    },
                }
            })
            .collect();

        let times = [board.fetched_at, costs.fetched_at];
        Enrichment {
            models: out,
            sources,
            fetched_at: times.iter().flatten().min().copied(),
            stale: board.stale || costs.stale,
            use_map,
        }
    }
}

#[cfg(test)]
mod tests;
