//! Benchmark scores from LLMBenchmark.ai (`GET /v1/scores?metric=`): one small read per metric.

use crate::Endpoints;
use crate::Match;
use crate::ModelRef;
use crate::cache::Fetcher;
use crate::cache::get_json;
use crate::normalize::normalize_model;
use crate::normalize::source_provider;
use crate::normalize::split_provider;
use crate::usemap::valid_metric;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// Scores change slowly; read each metric at most twice a day.
const SCORES_TTL_SECS: i64 = 12 * 3600;
const BOARD: &str = "llmbenchmark";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct RawScore {
    pub model_id: String,
    pub source: String,
    pub value: f64,
    pub unit: String,
    pub observed_at: Option<String>,
    pub source_url: Option<String>,
    pub setting: Option<String>,
}

/// One score of a model, as the source gave it, plus its place among the models of the metric.
#[derive(Clone, Debug, PartialEq)]
pub struct Score {
    /// The use this metric answers (`coding`, ...), from the editable table.
    pub use_name: String,
    pub metric: String,
    /// The benchmark the value comes from (`livebench`, `epoch`, ...).
    pub board: String,
    pub value: f64,
    pub unit: String,
    /// The measurement condition (`xHigh`, `OpenHands`; for speed, the serving provider).
    pub setting: Option<String>,
    pub observed_at: Option<String>,
    pub source_url: Option<String>,
    /// How many models have a value for this metric.
    pub n_models_in_metric: u32,
    /// 1 = best. A model with several settings has one rank, from its best value.
    pub rank: u32,
}

pub(crate) struct Board {
    pub metrics: BTreeMap<String, Vec<RawScore>>,
    pub any_data: bool,
    pub fetched_at: Option<i64>,
    pub stale: bool,
}

fn lower_is_better(metric: &str) -> bool {
    metric.contains("time_to_first_token") || metric.contains("latency")
}

fn parse_scores(body: &serde_json::Value) -> Vec<RawScore> {
    body.as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let text = |key: &str| {
                        row.get(key)
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    };
                    Some(RawScore {
                        model_id: text("modelId")?,
                        source: text("source").unwrap_or_default(),
                        value: row.get("value")?.as_f64().filter(|v| v.is_finite())?,
                        unit: text("unit").unwrap_or_default(),
                        observed_at: text("observedAt"),
                        source_url: text("sourceUrl"),
                        setting: text("setting"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) async fn load_board(
    fetcher: &Fetcher,
    endpoints: &Endpoints,
    wanted: &BTreeMap<String, Vec<String>>,
) -> Board {
    let names: BTreeSet<&String> = wanted
        .values()
        .flatten()
        .filter(|m| valid_metric(m))
        .collect();
    let mut board = Board {
        metrics: BTreeMap::new(),
        any_data: false,
        fetched_at: None,
        stale: false,
    };
    for metric in names {
        let url = format!(
            "{}/v1/scores?metric={metric}",
            endpoints.llmbenchmark_base.trim_end_matches('/')
        );
        let loaded = fetcher
            .load(
                &format!("llmbenchmark-scores-{metric}"),
                SCORES_TTL_SECS,
                |client| async move { Ok(parse_scores(&get_json(&client, &url).await?)) },
            )
            .await;
        if let Some(rows) = loaded.data {
            board.any_data = true;
            board.stale |= loaded.stale;
            board.fetched_at = match (board.fetched_at, loaded.fetched_at) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            board.metrics.insert(metric.clone(), rows);
        }
    }
    board
}

fn matches_model(row: &RawScore, model: &ModelRef) -> Option<&'static str> {
    let provider = source_provider(&model.provider);
    let (row_provider, row_model) = split_provider(&row.model_id);
    if row_provider.map(str::to_ascii_lowercase).as_deref() != Some(provider.as_str()) {
        return None;
    }
    if row_model == model.model_id {
        Some("exact")
    } else if normalize_model(row_model) == normalize_model(&model.model_id) {
        Some("normalized_id")
    } else {
        None
    }
}

fn better(metric: &str, a: f64, b: f64) -> bool {
    if lower_is_better(metric) { a < b } else { a > b }
}

pub(crate) fn scores_for(
    board: &Board,
    wanted: &BTreeMap<String, Vec<String>>,
    model: &ModelRef,
) -> (Option<Match>, Vec<Score>) {
    let mut found: Option<Match> = None;
    let mut out = Vec::new();
    for (use_name, metrics) in wanted {
        for metric in metrics {
            let Some(rows) = board.metrics.get(metric) else {
                continue;
            };
            let mine: Vec<(&RawScore, &'static str)> = rows
                .iter()
                .filter_map(|row| matches_model(row, model).map(|how| (row, how)))
                .collect();
            if mine.is_empty() {
                continue;
            }
            // Each model's best value, to place this model among the others.
            let mut best: BTreeMap<&str, f64> = BTreeMap::new();
            for row in rows {
                let slot = best.entry(row.model_id.as_str()).or_insert(row.value);
                if better(metric, row.value, *slot) {
                    *slot = row.value;
                }
            }
            let mut my_best = mine[0].0.value;
            for (row, _) in &mine {
                if better(metric, row.value, my_best) {
                    my_best = row.value;
                }
            }
            let ahead = best.values().filter(|v| better(metric, **v, my_best)).count();
            for (row, how) in mine {
                found.get_or_insert_with(|| Match {
                    source: BOARD.to_string(),
                    source_id: row.model_id.clone(),
                    matched_by: how.to_string(),
                });
                out.push(Score {
                    use_name: use_name.clone(),
                    metric: metric.clone(),
                    board: row.source.clone(),
                    value: row.value,
                    unit: row.unit.clone(),
                    setting: row.setting.clone(),
                    observed_at: row.observed_at.clone(),
                    source_url: row.source_url.clone(),
                    n_models_in_metric: u32::try_from(best.len()).unwrap_or(u32::MAX),
                    rank: u32::try_from(ahead + 1).unwrap_or(u32::MAX),
                });
            }
        }
    }
    (found, out)
}
