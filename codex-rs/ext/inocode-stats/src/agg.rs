use crate::record::LimitSample;
use crate::record::RequestRecord;
use chrono::Datelike;
use chrono::TimeZone;
use chrono::Timelike;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::HashMap;

/// Which records a view covers.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub since_ms: u64,
    pub model: Option<String>,
    pub provider: Option<String>,
}

fn keep(r: &RequestRecord, q: &Query) -> bool {
    r.ts_ms >= q.since_ms && q.provider.as_deref().is_none_or(|p| r.provider == p)
}

/// Quota percent per 1,000,000 tokens of each kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Weights {
    pub uncached_input: f64,
    pub cached_input: f64,
    pub output: f64,
}

// ---------------------------------------------------------------------------------------------
// Quota steps
// ---------------------------------------------------------------------------------------------

/// A rise of the used percent of one window, with the records that happened since the previous
/// reading of the same window of the same account. Readings are coarse (whole percents), so
/// consumption is only ever attributed over such steps.
struct Step {
    idxs: Vec<usize>,
    delta: f64,
    /// Tokens of this account's own requests in the step.
    tokens: u64,
}

/// Settings of the fit. Usage readings belong to the whole account, so a step also holds what
/// other devices used (it can only add to a step, never take away). The values are settings
/// with no evidence behind the defaults; only the direction of the effect is known.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FitOptions {
    /// Samples needed before a fit is trusted.
    pub min_samples: usize,
    /// Fraction (0..=1) of the steps, the ones with the lowest quota per token, that the fit
    /// uses. 1.0 uses all of them.
    pub lower_quantile: f64,
    /// A step in which this account sent fewer tokens than this, yet usage rose, is taken as
    /// used elsewhere and is not attributed to any request.
    pub elsewhere_min_tokens: u64,
}

impl Default for FitOptions {
    fn default() -> Self {
        Self {
            min_samples: 8,
            lower_quantile: 0.5,
            elsewhere_min_tokens: 1000,
        }
    }
}

fn is_elsewhere(step: &Step, opts: &FitOptions) -> bool {
    step.tokens < opts.elsewhere_min_tokens
}

/// Quota used in steps that the account's own tokens cannot explain.
fn elsewhere_total(steps: &[Step], opts: &FitOptions) -> f64 {
    steps
        .iter()
        .filter(|s| is_elsewhere(s, opts))
        .map(|s| s.delta)
        .sum()
}

#[derive(Clone, Copy)]
enum Window {
    FiveHour,
    Weekly,
}

fn sample_of(r: &RequestRecord, w: Window) -> Option<LimitSample> {
    match w {
        Window::FiveHour => r.five_hour,
        Window::Weekly => r.weekly,
    }
}

fn same_reset(a: Option<i64>, b: Option<i64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() <= 120,
        (None, None) => true,
        _ => false,
    }
}

/// `records` must be sorted by time.
fn steps(records: &[RequestRecord], w: Window) -> Vec<Step> {
    let mut series: BTreeMap<(&str, &str, &str), Vec<usize>> = BTreeMap::new();
    for (i, r) in records.iter().enumerate() {
        series
            .entry((
                r.provider.as_str(),
                r.account.as_deref().unwrap_or(""),
                r.limit_id.as_deref().unwrap_or(""),
            ))
            .or_default()
            .push(i);
    }
    let mut out = Vec::new();
    for idxs in series.into_values() {
        let mut base: Option<LimitSample> = None;
        let mut acc: Vec<usize> = Vec::new();
        for i in idxs {
            let Some(s) = sample_of(&records[i], w) else {
                if base.is_some() {
                    acc.push(i);
                }
                continue;
            };
            let Some(b) = base else {
                base = Some(s);
                acc.clear();
                continue;
            };
            if !same_reset(b.resets_at, s.resets_at) || s.used_percent < b.used_percent - 1e-9 {
                base = Some(s);
                acc.clear();
                continue;
            }
            acc.push(i);
            if s.used_percent > b.used_percent + 1e-9 {
                let idxs = std::mem::take(&mut acc);
                let tokens = idxs.iter().map(|&k| records[k].total_tokens()).sum();
                out.push(Step {
                    idxs,
                    delta: s.used_percent - b.used_percent,
                    tokens,
                });
                base = Some(s);
            }
        }
    }
    out
}

/// Per-record share of the consumption: each step is divided over its records by token count.
/// Steps used elsewhere are not attributed to any request.
fn attribute(records: &[RequestRecord], steps: &[Step], opts: &FitOptions) -> Vec<f64> {
    let mut share = vec![0.0; records.len()];
    for step in steps.iter().filter(|s| !is_elsewhere(s, opts)) {
        let total: f64 = step
            .idxs
            .iter()
            .map(|&i| records[i].total_tokens() as f64 + 1.0)
            .sum();
        for &i in &step.idxs {
            share[i] += step.delta * (records[i].total_tokens() as f64 + 1.0) / total;
        }
    }
    share
}

// ---------------------------------------------------------------------------------------------
// Fit
// ---------------------------------------------------------------------------------------------

type Sample = ([f64; 3], f64);

fn solve(a: &mut Vec<Vec<f64>>, b: &mut Vec<f64>) -> Option<Vec<f64>> {
    let n = b.len();
    for col in 0..n {
        let pivot = (col..n).max_by(|&x, &y| a[x][col].abs().total_cmp(&a[y][col].abs()))?;
        if a[pivot][col].abs() < 1e-18 {
            return None;
        }
        a.swap(col, pivot);
        b.swap(col, pivot);
        for row in col + 1..n {
            let f = a[row][col] / a[col][col];
            for k in col..n {
                a[row][k] -= f * a[col][k];
            }
            b[row] -= f * b[col];
        }
    }
    let mut x = vec![0.0; n];
    for row in (0..n).rev() {
        let s: f64 = (row + 1..n).map(|k| a[row][k] * x[k]).sum();
        x[row] = (b[row] - s) / a[row][row];
    }
    Some(x)
}

/// Non-negative least squares (no intercept) of `y ≈ w · x` over at most three variables.
fn fit(samples: &[Sample]) -> [f64; 3] {
    let mut active = [true; 3];
    for k in 0..3 {
        if samples.iter().all(|(x, _)| x[k] == 0.0) {
            active[k] = false;
        }
    }
    loop {
        let ids: Vec<usize> = (0..3).filter(|&k| active[k]).collect();
        if ids.is_empty() {
            return [0.0; 3];
        }
        let mut a = vec![vec![0.0; ids.len()]; ids.len()];
        let mut b = vec![0.0; ids.len()];
        for (x, y) in samples {
            for (r, &i) in ids.iter().enumerate() {
                b[r] += x[i] * y;
                for (c, &j) in ids.iter().enumerate() {
                    a[r][c] += x[i] * x[j];
                }
            }
        }
        for (r, row) in a.iter_mut().enumerate() {
            row[r] += 1e-12;
        }
        let Some(w) = solve(&mut a, &mut b) else {
            return [0.0; 3];
        };
        if let Some((worst, _)) = w
            .iter()
            .enumerate()
            .filter(|(_, v)| **v < 0.0)
            .min_by(|a, b| a.1.total_cmp(b.1))
        {
            active[ids[worst]] = false;
            continue;
        }
        let mut out = [0.0; 3];
        for (r, &i) in ids.iter().enumerate() {
            out[i] = w[r];
        }
        return out;
    }
}

/// Fit and blended rate for one model and window from the steps made only of its requests.
struct WindowFit {
    samples: usize,
    weights: Option<Weights>,
    per_million: Option<f64>,
}

fn window_fit(records: &[RequestRecord], steps: &[Step], model: &str, opts: &FitOptions) -> WindowFit {
    let min = opts.min_samples;
    let mut samples: Vec<Sample> = Vec::new();
    for s in steps {
        if is_elsewhere(s, opts) || !s.idxs.iter().all(|&i| records[i].model == model) {
            continue;
        }
        let mut x = [0.0; 3];
        for &i in &s.idxs {
            x[0] += records[i].uncached_input as f64 / 1e6;
            x[1] += records[i].cached_input as f64 / 1e6;
            x[2] += records[i].output as f64 / 1e6;
        }
        samples.push((x, s.delta));
    }
    let n = samples.len();
    if n < min {
        return WindowFit {
            samples: n,
            weights: None,
            per_million: None,
        };
    }
    // Other devices only add to a step: the steps with the least quota per token are the least
    // contaminated. Keep that lower fraction (at least three steps).
    let rate = |x: &[f64; 3], d: f64| {
        let tokens = x[0] + x[1] + x[2];
        if tokens > 0.0 { d / tokens } else { f64::INFINITY }
    };
    samples.sort_by(|a, b| rate(&a.0, a.1).total_cmp(&rate(&b.0, b.1)));
    let keep_n = ((opts.lower_quantile.clamp(0.0, 1.0) * n as f64).ceil() as usize).clamp(n.min(3), n);
    samples.truncate(keep_n);
    let sum_delta: f64 = samples.iter().map(|(_, d)| d).sum();
    let sum_tokens: f64 = samples.iter().map(|(x, _)| x[0] + x[1] + x[2]).sum();
    let w = fit(&samples);
    let usable = w.iter().any(|v| *v > 0.0);
    WindowFit {
        samples: n,
        weights: usable.then_some(Weights {
            uncached_input: w[0],
            cached_input: w[1],
            output: w[2],
        }),
        per_million: (sum_tokens > 0.0).then(|| sum_delta / sum_tokens),
    }
}

// ---------------------------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------------------------

/// One row of `inocode/stats/summary`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelStats {
    pub model: String,
    pub provider: String,
    pub requests: u64,
    pub uncached_input: u64,
    pub cached_input: u64,
    pub output: u64,
    pub reasoning: u64,
    /// Quota consumed in the 5-hour window (percent points).
    pub five_hour_percent: f64,
    /// Quota consumed in the weekly window (percent points).
    pub weekly_percent: f64,
    pub five_hour_weights: Option<Weights>,
    pub weekly_weights: Option<Weights>,
    /// Blended percent per 1,000,000 tokens.
    pub five_hour_per_million: Option<f64>,
    pub weekly_per_million: Option<f64>,
    pub five_hour_samples: u64,
    pub weekly_samples: u64,
    /// Too few samples for a per-million figure in both windows.
    pub insufficient: bool,
    pub avg_tok_per_s: Option<f64>,
    pub speed_samples: u64,
}

/// Result of [`summarize`]. The quota figures come from readings of the whole account, so they
/// include what other devices used: an upper bound for this machine.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub models: Vec<ModelStats>,
    /// Quota that rose while this machine sent (almost) nothing: used elsewhere.
    pub used_elsewhere_five_hour: f64,
    pub used_elsewhere_weekly: f64,
}

/// Output tokens divided by generation seconds, summed over the measured records.
fn avg_speed<'a>(it: impl Iterator<Item = &'a RequestRecord>) -> (Option<f64>, u64) {
    let (mut out, mut secs, mut n) = (0.0, 0.0, 0u64);
    for r in it {
        if let (Some(ms), Some(_)) = (r.gen_ms, r.tok_per_s) {
            out += r.output as f64;
            secs += ms as f64 / 1000.0;
            n += 1;
        }
    }
    ((secs > 0.0).then(|| out / secs), n)
}

pub fn summarize(all: &[RequestRecord], q: &Query, opts: &FitOptions) -> Summary {
    let records: Vec<RequestRecord> = all.iter().filter(|r| keep(r, q)).cloned().collect();
    let five_steps = steps(&records, Window::FiveHour);
    let week_steps = steps(&records, Window::Weekly);
    let five_share = attribute(&records, &five_steps, opts);
    let week_share = attribute(&records, &week_steps, opts);

    let mut by_model: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, r) in records.iter().enumerate() {
        if q.model.as_deref().is_none_or(|m| r.model == m) {
            by_model.entry(&r.model).or_default().push(i);
        }
    }
    let mut out = Vec::new();
    for (model, idxs) in by_model {
        let rows = || idxs.iter().map(|&i| &records[i]);
        let five = window_fit(&records, &five_steps, model, opts);
        let week = window_fit(&records, &week_steps, model, opts);
        let (avg, speed_samples) = avg_speed(rows());
        let mut providers: HashMap<&str, usize> = HashMap::new();
        for r in rows() {
            *providers.entry(&r.provider).or_default() += 1;
        }
        let provider = providers
            .into_iter()
            .max_by_key(|(_, n)| *n)
            .map(|(p, _)| p.to_string())
            .unwrap_or_default();
        out.push(ModelStats {
            model: model.to_string(),
            provider,
            requests: idxs.len() as u64,
            uncached_input: rows().map(|r| r.uncached_input).sum(),
            cached_input: rows().map(|r| r.cached_input).sum(),
            output: rows().map(|r| r.output).sum(),
            reasoning: rows().map(|r| r.reasoning).sum(),
            five_hour_percent: idxs.iter().map(|&i| five_share[i]).sum(),
            weekly_percent: idxs.iter().map(|&i| week_share[i]).sum(),
            insufficient: five.per_million.is_none() && week.per_million.is_none(),
            five_hour_weights: five.weights,
            weekly_weights: week.weights,
            five_hour_per_million: five.per_million,
            weekly_per_million: week.per_million,
            five_hour_samples: five.samples as u64,
            weekly_samples: week.samples as u64,
            avg_tok_per_s: avg,
            speed_samples,
        });
    }
    Summary {
        models: out,
        used_elsewhere_five_hour: elsewhere_total(&five_steps, opts),
        used_elsewhere_weekly: elsewhere_total(&week_steps, opts),
    }
}

fn local(ts_ms: u64) -> Option<chrono::DateTime<chrono::Local>> {
    chrono::Local
        .timestamp_millis_opt(i64::try_from(ts_ms).ok()?)
        .single()
}

/// Average speed of one local hour of the day (0–23) or weekday (0 = Monday).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HourlyRow {
    pub bucket: u32,
    pub requests: u64,
    pub speed_samples: u64,
    pub avg_tok_per_s: Option<f64>,
}

/// Speed rows of one model (and provider). Speeds of different models are never mixed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HourlySeries {
    pub model: String,
    pub provider: String,
    /// 24 rows, hour 0-23 in local time.
    pub hours: Vec<HourlyRow>,
    /// 7 rows, Monday first.
    pub weekdays: Vec<HourlyRow>,
}

fn speed_rows(groups: &[Vec<&RequestRecord>]) -> Vec<HourlyRow> {
    groups
        .iter()
        .enumerate()
        .map(|(bucket, g)| {
            let (avg, n) = avg_speed(g.iter().copied());
            HourlyRow {
                bucket: bucket as u32,
                requests: g.len() as u64,
                speed_samples: n,
                avg_tok_per_s: avg,
            }
        })
        .collect()
}

/// One series per (model, provider), sorted by model.
pub fn hourly(all: &[RequestRecord], q: &Query) -> Vec<HourlySeries> {
    let mut by_model: BTreeMap<(&str, &str), Vec<&RequestRecord>> = BTreeMap::new();
    for r in all
        .iter()
        .filter(|r| keep(r, q) && q.model.as_deref().is_none_or(|m| r.model == m))
    {
        by_model.entry((&r.model, &r.provider)).or_default().push(r);
    }
    by_model
        .into_iter()
        .map(|((model, provider), records)| {
            let mut hours: Vec<Vec<&RequestRecord>> = vec![Vec::new(); 24];
            let mut days: Vec<Vec<&RequestRecord>> = vec![Vec::new(); 7];
            for r in records {
                if let Some(t) = local(r.ts_ms) {
                    hours[t.hour() as usize].push(r);
                    days[t.weekday().num_days_from_monday() as usize].push(r);
                }
            }
            HourlySeries {
                model: model.to_string(),
                provider: provider.to_string(),
                hours: speed_rows(&hours),
                weekdays: speed_rows(&days),
            }
        })
        .collect()
}

/// One model's part of a day.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyModel {
    pub model: String,
    pub provider: String,
    pub requests: u64,
    pub tokens: u64,
    pub five_hour_percent: f64,
    pub weekly_percent: f64,
    pub avg_tok_per_s: Option<f64>,
}

/// A day: totals of requests, tokens and quota, and the per-model parts (with their own speeds;
/// no mixed speed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyRow {
    /// Local date, `YYYY-MM-DD`.
    pub date: String,
    pub requests: u64,
    pub tokens: u64,
    pub five_hour_percent: f64,
    pub weekly_percent: f64,
    pub models: Vec<DailyModel>,
}

pub fn daily(all: &[RequestRecord], q: &Query, opts: &FitOptions) -> Vec<DailyRow> {
    let records: Vec<RequestRecord> = all.iter().filter(|r| keep(r, q)).cloned().collect();
    let five_share = attribute(&records, &steps(&records, Window::FiveHour), opts);
    let week_share = attribute(&records, &steps(&records, Window::Weekly), opts);
    let mut by_day: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, r) in records.iter().enumerate() {
        if q.model.as_deref().is_none_or(|m| r.model == m)
            && let Some(t) = local(r.ts_ms)
        {
            by_day.entry(t.format("%Y-%m-%d").to_string()).or_default().push(i);
        }
    }
    by_day
        .into_iter()
        .map(|(date, idxs)| {
            let mut by_model: BTreeMap<(&str, &str), Vec<usize>> = BTreeMap::new();
            for &i in &idxs {
                by_model
                    .entry((&records[i].model, &records[i].provider))
                    .or_default()
                    .push(i);
            }
            let models: Vec<DailyModel> = by_model
                .into_iter()
                .map(|((model, provider), ids)| DailyModel {
                    model: model.to_string(),
                    provider: provider.to_string(),
                    requests: ids.len() as u64,
                    tokens: ids.iter().map(|&i| records[i].total_tokens()).sum(),
                    five_hour_percent: ids.iter().map(|&i| five_share[i]).sum(),
                    weekly_percent: ids.iter().map(|&i| week_share[i]).sum(),
                    avg_tok_per_s: avg_speed(ids.iter().map(|&i| &records[i])).0,
                })
                .collect();
            DailyRow {
                date,
                requests: idxs.len() as u64,
                tokens: idxs.iter().map(|&i| records[i].total_tokens()).sum(),
                five_hour_percent: models.iter().map(|m| m.five_hour_percent).sum(),
                weekly_percent: models.iter().map(|m| m.weekly_percent).sum(),
                models,
            }
        })
        .collect()
}

/// Prediction for a planned request, from the fitted rates of the recent records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EstimateResult {
    pub five_hour_percent: Option<f64>,
    pub weekly_percent: Option<f64>,
    pub seconds: Option<f64>,
    /// No fitted rate for this model yet.
    pub insufficient: bool,
    /// The rates include what other devices used on the same account: an upper estimate.
    pub upper_bound: bool,
}

pub fn estimate(
    all: &[RequestRecord],
    q: &Query,
    model: &str,
    uncached_input: u64,
    cached_input: u64,
    output: u64,
    opts: &FitOptions,
) -> EstimateResult {
    let q = Query {
        model: Some(model.to_string()),
        ..q.clone()
    };
    let row = summarize(all, &q, opts).models.into_iter().next();
    let cost = |w: Option<Weights>| {
        w.map(|w| {
            (uncached_input as f64 * w.uncached_input
                + cached_input as f64 * w.cached_input
                + output as f64 * w.output)
                / 1e6
        })
    };
    let five = row.as_ref().and_then(|r| cost(r.five_hour_weights));
    let week = row.as_ref().and_then(|r| cost(r.weekly_weights));
    let seconds = row
        .as_ref()
        .and_then(|r| r.avg_tok_per_s)
        .filter(|v| *v > 0.0)
        .map(|v| output as f64 / v);
    EstimateResult {
        insufficient: five.is_none() && week.is_none(),
        five_hour_percent: five,
        weekly_percent: week,
        seconds,
        upper_bound: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(min_samples: usize) -> FitOptions {
        FitOptions {
            min_samples,
            ..FitOptions::default()
        }
    }

    fn rec(
        ts_ms: u64,
        model: &str,
        uncached: u64,
        cached: u64,
        out: u64,
        five: f64,
        week: f64,
    ) -> RequestRecord {
        RequestRecord {
            ts_ms,
            source: "live".into(),
            thread_id: "t".into(),
            model: model.into(),
            provider: "openai".into(),
            account: Some("a".into()),
            limit_id: None,
            uncached_input: uncached,
            cached_input: cached,
            output: out,
            reasoning: 0,
            gen_ms: Some(10_000),
            tok_per_s: RequestRecord::speed(out, Some(10_000)),
            five_hour: Some(LimitSample {
                used_percent: five,
                resets_at: Some(1_000_000),
            }),
            weekly: Some(LimitSample {
                used_percent: week,
                resets_at: Some(9_000_000),
            }),
        }
    }

    /// Cost: 2 %/M uncached, 0.2 %/M cached, 10 %/M output. Readings every request, so steps
    /// are single records.
    fn history() -> Vec<RequestRecord> {
        let mut out = Vec::new();
        let mut five = 0.0;
        for i in 0..20u64 {
            let (u, c, o) = (100_000 + i * 10_000, 400_000 + (i % 3) * 100_000, 5_000 + (i % 4) * 3_000);
            five += (u as f64 * 2.0 + c as f64 * 0.2 + o as f64 * 10.0) / 1e6;
            out.push(rec(1_000 + i, "m", u, c, o, five, five / 4.0));
        }
        out
    }

    #[test]
    fn fit_recovers_weights() {
        let rows = summarize(&history(), &Query::default(), &opts(5)).models;
        let m = &rows[0];
        let w = m.five_hour_weights.expect("weights");
        assert!((w.uncached_input - 2.0).abs() < 0.05, "{w:?}");
        assert!((w.cached_input - 0.2).abs() < 0.05, "{w:?}");
        assert!((w.output - 10.0).abs() < 0.2, "{w:?}");
        assert!(!m.insufficient);
        assert_eq!(m.requests, 20);
        // 10 000 ms for output tokens: average is output / 10 s.
        assert!(m.avg_tok_per_s.is_some());
    }

    #[test]
    fn too_few_samples_is_insufficient() {
        let rows = summarize(&history()[..3], &Query::default(), &opts(8)).models;
        assert!(rows[0].insufficient);
        assert!(rows[0].five_hour_weights.is_none());
        let e = estimate(&history()[..3], &Query::default(), "m", 1, 1, 1, &opts(8));
        assert!(e.insufficient && e.five_hour_percent.is_none());
    }

    #[test]
    fn estimate_uses_the_fitted_weights() {
        let e = estimate(&history(), &Query::default(), "m", 1_000_000, 0, 100_000, &opts(5));
        let five = e.five_hour_percent.expect("five");
        assert!((five - 3.0).abs() < 0.2, "{five}");
        assert!(e.seconds.expect("seconds") > 0.0);
        assert!(!e.insufficient);
    }

    #[test]
    fn mixed_models_do_not_pollute_the_fit_but_share_the_consumption() {
        let mut rows = history();
        // An interleaved other-model request makes every step mixed for neither model's fit...
        for (i, r) in rows.iter_mut().enumerate() {
            if i % 2 == 1 {
                r.model = "other".into();
            }
        }
        // ...but a step is pure when only one model's records fall in it; here every second
        // record's step also holds the previous record of the other model.
        let out = summarize(&rows, &Query::default(), &opts(1)).models;
        let total: f64 = out.iter().map(|m| m.five_hour_percent).sum();
        let last = rows.last().unwrap().five_hour.unwrap().used_percent;
        let first = rows[0].five_hour.unwrap().used_percent;
        assert!((total - (last - first)).abs() < 1e-6, "{total}");
    }

    #[test]
    fn claude_and_openai_quota_readings_are_never_mixed() {
        // Alternating requests of two providers with unrelated percentages. Compared with each
        // other they would produce big jumps and resets; per family each series climbs by 1.
        let mut rows = Vec::new();
        for i in 0..10u64 {
            let mut a = rec(100 + i * 2, "gpt", 1000, 0, 100, 50.0 + i as f64, 1.0);
            a.limit_id = Some("codex".into());
            let mut c = rec(101 + i * 2, "claude-x", 1000, 0, 100, 10.0 + i as f64, 1.0);
            c.provider = "anthropic".into();
            c.limit_id = Some("claude".into());
            rows.push(a);
            rows.push(c);
        }
        let out = summarize(&rows, &Query::default(), &opts(1)).models;
        let used = |m: &str| out.iter().find(|r| r.model == m).unwrap().five_hour_percent;
        assert!((used("gpt") - 9.0).abs() < 1e-9, "{}", used("gpt"));
        assert!((used("claude-x") - 9.0).abs() < 1e-9, "{}", used("claude-x"));
        // Same provider id but another family: still separate.
        let mut same_provider = rows.clone();
        for r in &mut same_provider {
            r.provider = "openai".into();
        }
        let out = summarize(&same_provider, &Query::default(), &opts(1)).models;
        assert!((out.iter().map(|m| m.five_hour_percent).sum::<f64>() - 18.0).abs() < 1e-9);
    }

    #[test]
    fn a_window_reset_starts_a_new_baseline() {
        let mut a = rec(1, "m", 1000, 0, 100, 10.0, 1.0);
        let mut b = rec(2, "m", 1000, 0, 100, 12.0, 1.1);
        let mut c = rec(3, "m", 1000, 0, 100, 1.0, 1.2); // dropped: window reset
        c.five_hour = Some(LimitSample {
            used_percent: 1.0,
            resets_at: Some(2_000_000),
        });
        a.account = None;
        b.account = None;
        c.account = None;
        let out = summarize(&[a, b, c], &Query::default(), &opts(1)).models;
        assert!((out[0].five_hour_percent - 2.0).abs() < 1e-9);
    }

    #[test]
    fn hourly_and_daily_group_by_local_time() {
        let mut rows = history();
        rows[0].model = "slow".into();
        let h = hourly(&rows, &Query::default());
        assert_eq!(h.len(), 2, "one series per model");
        assert_eq!(h[0].hours.len(), 24);
        assert_eq!(h[0].weekdays.len(), 7);
        assert_eq!(h.iter().map(|s| s.hours.iter().map(|r| r.requests).sum::<u64>()).sum::<u64>(), 20);
        let d = daily(&rows, &Query::default(), &opts(1));
        assert!(!d.is_empty());
        assert_eq!(d.iter().map(|r| r.requests).sum::<u64>(), 20);
        assert_eq!(d.iter().map(|r| r.models.len()).max(), Some(2));
    }

    #[test]
    fn a_step_with_almost_no_own_tokens_is_used_elsewhere() {
        // Percent rises 10 -> 15 while this machine sent 50 tokens: another device used it.
        let a = rec(1, "m", 100_000, 0, 5_000, 10.0, 1.0);
        let mut b = rec(2, "m", 40, 0, 10, 15.0, 1.0);
        b.five_hour = Some(LimitSample {
            used_percent: 15.0,
            resets_at: Some(1_000_000),
        });
        let c = rec(3, "m", 100_000, 0, 5_000, 16.0, 1.0);
        let summary = summarize(&[a, b, c], &Query::default(), &opts(1));
        assert!((summary.used_elsewhere_five_hour - 5.0).abs() < 1e-9);
        // The last step (15 -> 16) is the machine's own; the elsewhere step is not attributed.
        assert!((summary.models[0].five_hour_percent - 1.0).abs() < 1e-9);
        // Turning the rule off (threshold 0) attributes everything to the machine.
        let all = summarize(
            &[rec(1, "m", 100_000, 0, 5_000, 10.0, 1.0), {
                let mut b = rec(2, "m", 40, 0, 10, 15.0, 1.0);
                b.five_hour = Some(LimitSample { used_percent: 15.0, resets_at: Some(1_000_000) });
                b
            }],
            &Query::default(),
            &FitOptions { min_samples: 1, elsewhere_min_tokens: 0, ..FitOptions::default() },
        );
        assert!((all.models[0].five_hour_percent - 5.0).abs() < 1e-9);
        assert_eq!(all.used_elsewhere_five_hour, 0.0);
    }

    #[test]
    fn the_lower_quantile_ignores_steps_inflated_by_other_devices() {
        // Truth: 2 %/M uncached input. Every other step also carries 3 extra percent from elsewhere.
        let mut rows = Vec::new();
        let mut five = 0.0;
        for i in 0..24u64 {
            let u = 1_000_000 + (i % 5) * 100_000;
            five += u as f64 * 2.0 / 1e6 + if i % 2 == 0 { 3.0 } else { 0.0 };
            rows.push(rec(1_000 + i, "m", u, 0, 0, five, five / 4.0));
        }
        let weight = |q: f64| {
            let s = summarize(
                &rows,
                &Query::default(),
                &FitOptions { min_samples: 5, lower_quantile: q, elsewhere_min_tokens: 0 },
            );
            s.models[0].five_hour_weights.expect("weights").uncached_input
        };
        let all_steps = weight(1.0);
        let lower = weight(0.5);
        assert!(all_steps > 2.5, "{all_steps}");
        assert!((lower - 2.0).abs() < (all_steps - 2.0).abs(), "{lower} vs {all_steps}");
        assert!((lower - 2.0).abs() < 0.1, "{lower}");
    }

    #[test]
    fn estimates_say_they_are_an_upper_bound() {
        let e = estimate(&history(), &Query::default(), "m", 1_000_000, 0, 0, &opts(5));
        assert!(e.upper_bound);
    }
}
