//! Per-request usage statistics: tokens, generation speed and quota (rate-limit) consumption.
//!
//! Every model request appends one record to `<INOCODE_HOME>/stats/requests.jsonl`. Nothing is
//! ever deleted; the "last N days" views are produced by filtering at query time. Cost and speed
//! per model are never hard-coded: they are fitted from the records (see [`summarize`]).

mod agg;
mod record;
mod seed;

pub use agg::DailyRow;
pub use agg::EstimateResult;
pub use agg::HourlyRow;
pub use agg::DailyModel;
pub use agg::HourlySeries;
pub use agg::ModelStats;
pub use agg::Query;
pub use agg::Weights;
pub use agg::daily;
pub use agg::estimate;
pub use agg::hourly;
pub use agg::summarize;
pub use record::LimitSample;
pub use record::RequestRecord;
pub use record::StatsStore;
pub use record::classify_window;
pub use record::now_ms;
pub use seed::SeedProgress;
pub use seed::SeedReport;
pub use seed::default_sessions_dir;
pub use seed::seed_from_codex;
pub use seed::seed_with_progress;

/// Samples needed before a per-model fit is trusted (`stats.min_samples`).
pub fn min_samples() -> usize {
    usize::try_from(codex_inocode_flags::int_param("stats.min_samples")).unwrap_or(usize::MAX)
}

/// Appends one record to the default store; errors are logged, never propagated.
pub fn record_global(record: &RequestRecord) {
    if let Err(err) = StatsStore::from_env().append(record) {
        eprintln!("inocode-stats: failed to record request: {err}");
    }
}
