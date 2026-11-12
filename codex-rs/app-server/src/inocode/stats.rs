//! `inocode/stats/*`: per-model quota cost and speed, fitted from the request records written by
//! `codex-inocode-stats`. Nothing here holds a number of its own; every value comes from the
//! records of the requested window.

use super::InocodeResult;
use super::unknown_method;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeStatsDailyModel;
use codex_app_server_protocol::InocodeStatsDailyParams;
use codex_app_server_protocol::InocodeStatsDailyResponse;
use codex_app_server_protocol::InocodeStatsDailyRow;
use codex_app_server_protocol::InocodeStatsEstimateParams;
use codex_app_server_protocol::InocodeStatsEstimateResponse;
use codex_app_server_protocol::InocodeStatsHourRow;
use codex_app_server_protocol::InocodeStatsHourlyParams;
use codex_app_server_protocol::InocodeStatsHourlyResponse;
use codex_app_server_protocol::InocodeStatsModel;
use codex_app_server_protocol::InocodeStatsQuota;
use codex_app_server_protocol::InocodeStatsQuotaPerMillion;
use codex_app_server_protocol::InocodeStatsSeedFromCodexHistoryResponse;
use codex_app_server_protocol::InocodeStatsSeedProgressNotification;
use codex_app_server_protocol::InocodeStatsSpeedSeries;
use codex_app_server_protocol::InocodeStatsSummaryParams;
use codex_app_server_protocol::InocodeStatsSummaryResponse;
use codex_app_server_protocol::InocodeStatsTokenTotals;
use codex_app_server_protocol::InocodeStatsTurn;
use codex_app_server_protocol::InocodeStatsTurnsParams;
use codex_app_server_protocol::InocodeStatsTurnsResponse;
use codex_app_server_protocol::InocodeStatsWeekdayRow;
use codex_inocode_stats::HourlyRow;
use codex_inocode_stats::ModelStats;
use codex_inocode_stats::Query;
use codex_inocode_stats::StatsStore;
use codex_inocode_stats::Weights;
use std::sync::OnceLock;

const DAY_MS: u64 = 86_400_000;
/// Window of the estimate when the caller gives none: recent values are the most trusted.
const DEFAULT_ESTIMATE_DAYS: i64 = 3;

fn num(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn since_of(days: i64) -> u64 {
    if days <= 0 {
        return 0;
    }
    codex_inocode_stats::now_ms().saturating_sub(u64::try_from(days).unwrap_or(0) * DAY_MS)
}

fn query(days: i64, model: &Option<String>, provider: &Option<String>) -> Query {
    Query {
        since_ms: since_of(days),
        model: model.clone().filter(|m| !m.is_empty()),
        provider: provider.clone().filter(|p| !p.is_empty()),
    }
}

/// One token kind of the fitted rates: primary (5 h) and secondary (weekly) windows.
fn per_kind(
    primary: Option<Weights>,
    secondary: Option<Weights>,
    pick: fn(&Weights) -> f64,
) -> InocodeStatsQuota {
    InocodeStatsQuota {
        primary_percent: primary.as_ref().map(pick),
        secondary_percent: secondary.as_ref().map(pick),
    }
}

fn model_row(m: ModelStats) -> InocodeStatsModel {
    let per_million = (!m.insufficient).then(|| InocodeStatsQuotaPerMillion {
        uncached_input: per_kind(m.five_hour_weights, m.weekly_weights, |w| w.uncached_input),
        cached_input: per_kind(m.five_hour_weights, m.weekly_weights, |w| w.cached_input),
        output: per_kind(m.five_hour_weights, m.weekly_weights, |w| w.output),
    });
    InocodeStatsModel {
        model: m.model,
        provider: m.provider,
        requests: num(m.requests),
        tokens: InocodeStatsTokenTotals {
            uncached_input: num(m.uncached_input),
            cached_input: num(m.cached_input),
            output: num(m.output),
            reasoning: num(m.reasoning),
        },
        quota_used: InocodeStatsQuota {
            primary_percent: Some(m.five_hour_percent),
            secondary_percent: Some(m.weekly_percent),
        },
        quota_per_million: per_million,
        avg_tokens_per_second: m.avg_tok_per_s,
        insufficient: m.insufficient,
    }
}

pub(crate) fn summary_in(
    store: &StatsStore,
    params: &InocodeStatsSummaryParams,
) -> InocodeStatsSummaryResponse {
    let q = query(params.days, &params.model, &params.provider);
    let records = store.read_since(q.since_ms);
    let summary = codex_inocode_stats::summarize(&records, &q, &codex_inocode_stats::fit_options());
    InocodeStatsSummaryResponse {
        days: params.days,
        models: summary.models.into_iter().map(model_row).collect(),
        quota_is_upper_bound: true,
        used_elsewhere: InocodeStatsQuota {
            primary_percent: Some(summary.used_elsewhere_five_hour),
            secondary_percent: Some(summary.used_elsewhere_weekly),
        },
    }
}

fn hour_rows(rows: Vec<HourlyRow>) -> Vec<InocodeStatsHourRow> {
    rows.into_iter()
        .map(|r| InocodeStatsHourRow {
            hour: i64::from(r.bucket),
            avg_tokens_per_second: r.avg_tok_per_s,
            samples: num(r.speed_samples),
        })
        .collect()
}

fn weekday_rows(rows: Vec<HourlyRow>) -> Vec<InocodeStatsWeekdayRow> {
    rows.into_iter()
        .map(|r| InocodeStatsWeekdayRow {
            weekday: i64::from(r.bucket),
            avg_tokens_per_second: r.avg_tok_per_s,
            samples: num(r.speed_samples),
        })
        .collect()
}

pub(crate) fn hourly_in(
    store: &StatsStore,
    params: &InocodeStatsHourlyParams,
) -> InocodeStatsHourlyResponse {
    let q = query(params.days, &params.model, &None);
    let series = codex_inocode_stats::hourly(&store.read_since(q.since_ms), &q)
        .into_iter()
        .map(|s| InocodeStatsSpeedSeries {
            model: s.model,
            provider: s.provider,
            hours: hour_rows(s.hours),
            weekdays: weekday_rows(s.weekdays),
        })
        .collect();
    InocodeStatsHourlyResponse { series }
}

pub(crate) fn daily_in(
    store: &StatsStore,
    params: &InocodeStatsDailyParams,
) -> InocodeStatsDailyResponse {
    let q = query(params.days, &None, &None);
    let days = codex_inocode_stats::daily(&store.read_since(q.since_ms), &q, &codex_inocode_stats::fit_options())
        .into_iter()
        .map(|r| InocodeStatsDailyRow {
            date: r.date,
            requests: num(r.requests),
            tokens: num(r.tokens),
            quota_primary_percent: r.five_hour_percent,
            quota_secondary_percent: r.weekly_percent,
            models: r
                .models
                .into_iter()
                .map(|m| InocodeStatsDailyModel {
                    model: m.model,
                    provider: m.provider,
                    requests: num(m.requests),
                    tokens: num(m.tokens),
                    avg_tokens_per_second: m.avg_tok_per_s,
                    quota_primary_percent: m.five_hour_percent,
                    quota_secondary_percent: m.weekly_percent,
                })
                .collect(),
        })
        .collect();
    InocodeStatsDailyResponse { days }
}

pub(crate) fn estimate_in(
    store: &StatsStore,
    params: &InocodeStatsEstimateParams,
) -> InocodeStatsEstimateResponse {
    let days = params.days.unwrap_or(DEFAULT_ESTIMATE_DAYS);
    let q = query(days, &None, &None);
    let nonneg = |v: i64| u64::try_from(v).unwrap_or(0);
    let e = codex_inocode_stats::estimate(
        &store.read_since(q.since_ms),
        &q,
        &params.model,
        nonneg(params.tokens.uncached_input),
        nonneg(params.tokens.cached_input),
        nonneg(params.tokens.output),
        &codex_inocode_stats::fit_options(),
    );
    let ok = !e.insufficient;
    InocodeStatsEstimateResponse {
        model: params.model.clone(),
        days,
        primary_percent: e.five_hour_percent.filter(|_| ok),
        secondary_percent: e.weekly_percent.filter(|_| ok),
        seconds: e.seconds.filter(|_| ok),
        insufficient: e.insufficient,
        upper_bound: true,
    }
}

fn progress_feed() -> &'static tokio::sync::broadcast::Sender<InocodeStatsSeedProgressNotification>
{
    static FEED: OnceLock<tokio::sync::broadcast::Sender<InocodeStatsSeedProgressNotification>> =
        OnceLock::new();
    FEED.get_or_init(|| tokio::sync::broadcast::channel(64).0)
}

/// Starts the import in the background and returns its job id. Progress goes to the feed behind
/// `inocode/stats/seedProgress`; the last message has `done: true`.
fn start_seed(
    store: StatsStore,
    dir: std::path::PathBuf,
) -> InocodeStatsSeedFromCodexHistoryResponse {
    let job_id = format!("seed-{}", codex_inocode_stats::now_ms());
    let id = job_id.clone();
    let send = move |files_done: u64, files_total: u64, added: u64, done: bool| {
        let _ = progress_feed().send(InocodeStatsSeedProgressNotification {
            job_id: id.clone(),
            files_done: num(files_done),
            files_total: num(files_total),
            samples_added: num(added),
            done,
        });
    };
    // Hundreds of megabytes are streamed: keep it off the async workers.
    tokio::task::spawn_blocking(move || {
        let mut last = (0, 0, 0);
        let result = codex_inocode_stats::seed_with_progress(&store, &dir, &mut |p| {
            last = (p.files_done, p.files_total, p.samples_added);
            send(p.files_done, p.files_total, p.samples_added, false);
        });
        if let Err(err) = result {
            tracing::warn!("Codex history import failed: {err}");
        }
        send(last.0, last.1, last.2, true);
    });
    InocodeStatsSeedFromCodexHistoryResponse { job_id }
}

/// Forwards import progress as `inocode/stats/seedProgress`. Call once at startup.
pub(crate) fn spawn_notifier(
    outgoing: std::sync::Arc<crate::outgoing_message::OutgoingMessageSender>,
) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let mut rx = progress_feed().subscribe();
    handle.spawn(async move {
        loop {
            match rx.recv().await {
                Ok(p) => {
                    outgoing
                        .send_server_notification(
                            codex_app_server_protocol::ServerNotification::InocodeStatsSeedProgress(
                                p,
                            ),
                        )
                        .await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// The rollout of a thread: `sessions/<yyyy>/<mm>/<dd>/rollout-<time>-<thread id>.jsonl`.
fn find_rollout(sessions: &std::path::Path, thread_id: &str) -> Option<std::path::PathBuf> {
    let suffix = format!("-{thread_id}.jsonl");
    let mut stack = vec![sessions.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(&suffix))
            {
                return Some(path);
            }
        }
    }
    None
}

/// Past turns of a thread with their speed: turn bounds from the rollout, speed from the
/// records inside each turn. A thread without a rollout has no turns.
pub(crate) fn turns_in(
    store: &StatsStore,
    sessions: &std::path::Path,
    params: &InocodeStatsTurnsParams,
) -> InocodeStatsTurnsResponse {
    let Some(text) = find_rollout(sessions, &params.thread_id)
        .and_then(|path| std::fs::read_to_string(path).ok())
    else {
        return InocodeStatsTurnsResponse { turns: Vec::new() };
    };
    let bounds = codex_inocode_stats::parse_turns(&text);
    let records: Vec<_> = store
        .read_since(0)
        .into_iter()
        .filter(|r| r.thread_id == params.thread_id)
        .collect();
    let mut turns: Vec<InocodeStatsTurn> = codex_inocode_stats::turn_speeds(&bounds, &records)
        .into_iter()
        .map(|t| InocodeStatsTurn {
            turn_id: t.turn_id,
            started_at: t.started_at,
            completed_at: t.completed_at,
            duration_ms: t.duration_ms.map(num),
            time_to_first_token_ms: t.time_to_first_token_ms.map(num),
            requests: num(t.requests),
            output_tokens: num(t.output_tokens),
            avg_tokens_per_second: t.avg_tok_per_s,
            speed_samples: num(t.speed_samples),
            model: t.model,
            provider: t.provider,
        })
        .collect();
    if let Some(limit) = params.limit.and_then(|l| usize::try_from(l).ok()) {
        let skip = turns.len().saturating_sub(limit);
        turns.drain(..skip);
    }
    InocodeStatsTurnsResponse { turns }
}

pub(crate) async fn handle(request: ClientRequest) -> InocodeResult {
    match request {
        ClientRequest::InocodeStatsTurns { params, .. } => Ok(Some(
            turns_in(
                &StatsStore::from_env(),
                &codex_inocode_flags::inocode_home().join("sessions"),
                &params,
            )
            .into(),
        )),
        ClientRequest::InocodeStatsSummary { params, .. } => {
            Ok(Some(summary_in(&StatsStore::from_env(), &params).into()))
        }
        ClientRequest::InocodeStatsHourly { params, .. } => {
            Ok(Some(hourly_in(&StatsStore::from_env(), &params).into()))
        }
        ClientRequest::InocodeStatsDaily { params, .. } => {
            Ok(Some(daily_in(&StatsStore::from_env(), &params).into()))
        }
        ClientRequest::InocodeStatsEstimate { params, .. } => {
            Ok(Some(estimate_in(&StatsStore::from_env(), &params).into()))
        }
        ClientRequest::InocodeStatsSeedFromCodexHistory { params, .. } => {
            let dir = params
                .path
                .filter(|d| !d.is_empty())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(codex_inocode_stats::default_sessions_dir);
            Ok(Some(start_seed(StatsStore::from_env(), dir).into()))
        }
        other => Err(unknown_method(other.method_name())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn past_turns_come_from_the_rollout_and_the_records_of_the_thread() {
        let home = tempfile::tempdir().unwrap();
        let day = home.path().join("sessions").join("2026").join("11").join("12");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(
            day.join("rollout-2026-11-12T00-00-00-thread-1.jsonl"),
            concat!(
                r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"a","started_at":1000}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"a","started_at":1000,"completed_at":1010,"duration_ms":10000,"time_to_first_token_ms":500}}"#,
                "\n",
            ),
        )
        .unwrap();
        let store = StatsStore::at(home.path());
        let mut mine = record(0, 0.0);
        mine.thread_id = "thread-1".into();
        mine.ts_ms = 1_004_000;
        mine.output = 200;
        mine.gen_ms = Some(2_000);
        mine.tok_per_s = codex_inocode_stats::RequestRecord::speed(200, Some(2_000));
        let mut other = mine.clone();
        other.thread_id = "thread-2".into();
        store.append_all(&[mine, other]).unwrap();
        let sessions = home.path().join("sessions");
        let response = turns_in(
            &store,
            &sessions,
            &InocodeStatsTurnsParams { thread_id: "thread-1".into(), limit: None },
        );
        assert_eq!(response.turns.len(), 1);
        let turn = &response.turns[0];
        assert_eq!(turn.requests, 1);
        assert_eq!(turn.output_tokens, 200);
        assert_eq!(turn.avg_tokens_per_second.map(|v| v.round() as i64), Some(100));
        assert_eq!(turn.time_to_first_token_ms, Some(500));
        // A thread without a rollout has no turns.
        let none = turns_in(
            &store,
            &sessions,
            &InocodeStatsTurnsParams { thread_id: "missing".into(), limit: None },
        );
        assert!(none.turns.is_empty());
    }
    use codex_inocode_stats::LimitSample;
    use codex_inocode_stats::RequestRecord;

    fn record(i: u64, five: f64) -> RequestRecord {
        let now = codex_inocode_stats::now_ms();
        RequestRecord {
            ts_ms: now - 1000 + i,
            source: "live".into(),
            thread_id: "t".into(),
            model: "gpt-x".into(),
            provider: "openai".into(),
            account: Some("main".into()),
            limit_id: Some("codex".into()),
            uncached_input: 100_000,
            cached_input: 0,
            output: 10_000,
            reasoning: 0,
            gen_ms: Some(2_000),
            tok_per_s: RequestRecord::speed(10_000, Some(2_000)),
            five_hour: Some(LimitSample {
                used_percent: five,
                resets_at: Some(4_000_000_000),
            }),
            weekly: None,
        }
    }

    fn store_with_history() -> (tempfile::TempDir, StatsStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = StatsStore::at(dir.path());
        // Each request costs 1 % of the 5-hour window.
        let rows: Vec<RequestRecord> = (0..12).map(|i| record(i, 10.0 + i as f64)).collect();
        store.append_all(&rows).expect("append");
        (dir, store)
    }

    #[test]
    fn summary_hourly_daily_and_estimate_come_from_the_records() {
        let (_dir, store) = store_with_history();
        let s = summary_in(
            &store,
            &InocodeStatsSummaryParams {
                days: 3,
                model: None,
                provider: None,
            },
        );
        assert_eq!(s.models.len(), 1);
        let m = &s.models[0];
        assert_eq!(m.requests, 12);
        assert!((m.quota_used.primary_percent.expect("used") - 11.0).abs() < 1e-6);
        assert!(!m.insufficient);
        assert!(m.quota_per_million.is_some());
        assert_eq!(m.avg_tokens_per_second.map(|v| v.round() as i64), Some(5000));

        let none = summary_in(
            &store,
            &InocodeStatsSummaryParams {
                days: 3,
                model: Some("other".into()),
                provider: None,
            },
        );
        assert!(none.models.is_empty());

        let h = hourly_in(
            &store,
            &InocodeStatsHourlyParams {
                days: 3,
                model: Some("gpt-x".into()),
            },
        );
        assert_eq!(h.series.len(), 1);
        assert_eq!((h.series[0].hours.len(), h.series[0].weekdays.len()), (24, 7));
        assert_eq!(h.series[0].hours.iter().map(|r| r.samples).sum::<i64>(), 12);

        let d = daily_in(
            &store,
            &InocodeStatsDailyParams { days: 3 },
        );
        assert_eq!(d.days.iter().map(|r| r.requests).sum::<i64>(), 12);

        let e = estimate_in(
            &store,
            &InocodeStatsEstimateParams {
                model: "gpt-x".into(),
                tokens: codex_app_server_protocol::InocodeStatsTokens {
                    uncached_input: 100_000,
                    cached_input: 0,
                    output: 10_000,
                },
                days: None,
            },
        );
        assert!(!e.insufficient);
        assert!((e.primary_percent.expect("percent") - 1.0).abs() < 0.1);
        assert!((e.seconds.expect("seconds") - 2.0).abs() < 0.1);
    }

    #[test]
    fn an_unknown_model_has_an_insufficient_estimate() {
        let (_dir, store) = store_with_history();
        let e = estimate_in(
            &store,
            &InocodeStatsEstimateParams {
                model: "nobody".into(),
                tokens: codex_app_server_protocol::InocodeStatsTokens {
                    uncached_input: 1,
                    cached_input: 0,
                    output: 1,
                },
                days: Some(7),
            },
        );
        assert!(e.insufficient);
        assert!(e.seconds.is_none());
    }
}
