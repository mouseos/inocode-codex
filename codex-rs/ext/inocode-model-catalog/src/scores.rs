//! Benchmark scores from LLMBenchmark.ai (`GET /v1/scores?metric=`): one small read per metric.

use crate::Endpoints;
use crate::Match;
use crate::ModelRef;
use crate::cache::Fetcher;
use crate::cache::get_json;
use crate::resolve::Resolved;
use crate::resolve::resolve;
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

/// Output speed from the benchmark source: the median over the serving providers it lists.
#[derive(Clone, Debug, PartialEq)]
pub struct BenchSpeed {
    pub tokens_per_second: f64,
    /// How many serving providers' values the median is of.
    pub n_settings: u32,
    pub metric: String,
    pub observed_at: Option<String>,
    pub source_url: Option<String>,
}

/// The metric the benchmark source's speed comes from.
pub(crate) const SPEED_METRIC: &str = "or_output_tokens_per_second";

pub(crate) fn speed_for(board: &Board, model: &ModelRef) -> Option<BenchSpeed> {
    let rows = board.metrics.get(SPEED_METRIC)?;
    let resolved = resolve(model, &|provider| board_ids(board, provider))?;
    let mut mine: Vec<&RawScore> = rows
        .iter()
        .filter(|row| matches_model(row, &resolved).is_some())
        .filter(|row| row.value.is_finite() && row.value > 0.0)
        .collect();
    if mine.is_empty() {
        return None;
    }
    mine.sort_by(|a, b| a.value.total_cmp(&b.value));
    let middle = mine.len() / 2;
    let value = if mine.len() % 2 == 1 {
        mine[middle].value
    } else {
        (mine[middle - 1].value + mine[middle].value) / 2.0
    };
    let newest = mine
        .iter()
        .filter_map(|row| row.observed_at.clone())
        .max();
    Some(BenchSpeed {
        tokens_per_second: value,
        n_settings: u32::try_from(mine.len()).unwrap_or(u32::MAX),
        metric: SPEED_METRIC.to_string(),
        observed_at: newest,
        source_url: mine[0].source_url.clone(),
    })
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

/// The row is the model the lookup resolved to (see `resolve`).
fn matches_model(row: &RawScore, resolved: &Resolved) -> Option<&'static str> {
    let (row_provider, row_model) = split_provider(&row.model_id);
    (row_provider.map(str::to_ascii_lowercase).as_deref() == Some(resolved.provider.as_str())
        && row_model == resolved.model)
        .then_some(resolved.how)
}

/// The model ids the board lists for one provider.
fn board_ids(board: &Board, provider: &str) -> Vec<String> {
    let mut ids: BTreeSet<String> = BTreeSet::new();
    for rows in board.metrics.values() {
        for row in rows {
            let (row_provider, row_model) = split_provider(&row.model_id);
            if row_provider.map(str::to_ascii_lowercase).as_deref() == Some(provider) {
                ids.insert(row_model.to_string());
            }
        }
    }
    ids.into_iter().collect()
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
    let Some(resolved) = resolve(model, &|provider| board_ids(board, provider)) else {
        return (found, out);
    };
    for (use_name, metrics) in wanted {
        for metric in metrics {
            let Some(rows) = board.metrics.get(metric) else {
                continue;
            };
            let mine: Vec<(&RawScore, &'static str)> = rows
                .iter()
                .filter_map(|row| matches_model(row, &resolved).map(|how| (row, how)))
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

#[cfg(test)]
mod speed_tests {
    use super::*;

    fn row(model_id: &str, value: f64, setting: &str) -> RawScore {
        RawScore {
            model_id: model_id.to_string(),
            source: "openrouter".to_string(),
            value,
            unit: "count".to_string(),
            observed_at: Some("2026-11-11".to_string()),
            source_url: Some("https://example.test/speed".to_string()),
            setting: Some(setting.to_string()),
        }
    }

    fn board(rows: Vec<RawScore>) -> Board {
        Board {
            metrics: BTreeMap::from([(SPEED_METRIC.to_string(), rows)]),
            any_data: true,
            fetched_at: None,
            stale: false,
        }
    }

    fn model(provider: &str, id: &str) -> ModelRef {
        ModelRef {
            provider: provider.to_string(),
            model_id: id.to_string(),
            auth_kind: None,
        }
    }

    #[test]
    fn speed_is_the_median_over_serving_providers_found_under_the_common_name() {
        let board = board(vec![
            row("deepseek/deepseek-v4.1-flash", 30.0, "a"),
            row("deepseek/deepseek-v4.1-flash", 90.0, "b"),
            row("deepseek/deepseek-v4.1-flash", 60.0, "c"),
            row("deepseek/other", 5.0, "a"),
        ]);
        let speed = speed_for(&board, &model("deepai", "deepseek-v4.1-flash")).expect("speed");
        assert_eq!(speed.tokens_per_second, 60.0);
        assert_eq!(speed.n_settings, 3);
        assert_eq!(speed.metric, SPEED_METRIC);
        assert_eq!(speed.observed_at.as_deref(), Some("2026-11-11"));
        // The same model under another provider's name gives the same figure.
        let via = speed_for(&board, &model("openscale", "deepseek/deepseek-v4.1-flash")).expect("via");
        assert_eq!(via, speed);
        // An even number of values: the mean of the two in the middle.
        let even = super::speed_for(
            &super::speed_tests::board(vec![
                row("openai/m", 10.0, "a"),
                row("openai/m", 30.0, "b"),
            ]),
            &model("openai", "m"),
        )
        .expect("even");
        assert_eq!(even.tokens_per_second, 20.0);
        // No row, no figure; a model that is not listed gets none.
        assert_eq!(speed_for(&board, &model("deepai", "glyph-cluster")), None);
    }
}
