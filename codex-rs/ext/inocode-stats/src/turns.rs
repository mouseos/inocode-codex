//! Speed of past turns: where each turn began and ended comes from the thread's rollout
//! (`task_started` / `task_complete`: times, duration, time to the first token); how much was
//! generated, and in how long, comes from the request records that fall inside the turn.
//! Nothing is guessed: a turn without a measured request has no speed, and a rollout line of an
//! older shape is skipped.

use crate::RequestRecord;
use crate::agg::avg_speed;
use serde_json::Value;

/// One turn as the rollout recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnBounds {
    pub turn_id: String,
    /// Unix seconds.
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub duration_ms: Option<u64>,
    pub time_to_first_token_ms: Option<u64>,
}

/// One turn with its speed.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnSpeed {
    pub turn_id: String,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub duration_ms: Option<u64>,
    pub time_to_first_token_ms: Option<u64>,
    /// Requests of the thread that finished inside the turn.
    pub requests: u64,
    /// Output tokens of those requests.
    pub output_tokens: u64,
    /// Output tokens over generation seconds of the measured requests; `None` when no request of
    /// the turn was measured.
    pub avg_tok_per_s: Option<f64>,
    /// Requests that had a measured speed.
    pub speed_samples: u64,
    /// The model of the most requests in the turn.
    pub model: Option<String>,
    pub provider: Option<String>,
}

fn secs(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(Value::as_i64)
}

fn millis(value: &Value, key: &str) -> Option<u64> {
    value
        .get(key)
        .and_then(Value::as_i64)
        .and_then(|v| u64::try_from(v).ok())
}

/// The turns of a rollout, oldest first. A turn that never completed has no end.
pub fn parse_turns(rollout: &str) -> Vec<TurnBounds> {
    let mut turns: Vec<TurnBounds> = Vec::new();
    for line in rollout.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(payload) = value.get("payload") else {
            continue;
        };
        let kind = payload.get("type").and_then(Value::as_str);
        let Some(turn_id) = payload.get("turn_id").and_then(Value::as_str) else {
            continue;
        };
        match kind {
            Some("task_started") => turns.push(TurnBounds {
                turn_id: turn_id.to_string(),
                started_at: secs(payload, "started_at"),
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
            Some("task_complete") => {
                let index = match turns.iter().rposition(|t| t.turn_id == turn_id) {
                    Some(index) => index,
                    None => {
                        turns.push(TurnBounds {
                            turn_id: turn_id.to_string(),
                            started_at: secs(payload, "started_at"),
                            completed_at: None,
                            duration_ms: None,
                            time_to_first_token_ms: None,
                        });
                        turns.len() - 1
                    }
                };
                let turn = &mut turns[index];
                turn.started_at = turn.started_at.or_else(|| secs(payload, "started_at"));
                turn.completed_at = secs(payload, "completed_at");
                turn.duration_ms = millis(payload, "duration_ms");
                turn.time_to_first_token_ms = millis(payload, "time_to_first_token_ms");
            }
            _ => {}
        }
    }
    turns
}

/// Slack after a turn's end for a request record written just after it (the record is written
/// when the response completes, the turn ends a moment later).
const END_SLACK_MS: u64 = 2_000;

/// The speed of each turn, from the records of its thread.
pub fn turn_speeds(bounds: &[TurnBounds], records: &[RequestRecord]) -> Vec<TurnSpeed> {
    bounds
        .iter()
        .map(|turn| {
            let from = turn
                .started_at
                .and_then(|s| u64::try_from(s).ok())
                .map(|s| s * 1000);
            let to = turn
                .completed_at
                .and_then(|s| u64::try_from(s).ok())
                .map(|s| s * 1000 + END_SLACK_MS);
            let inside: Vec<&RequestRecord> = match (from, to) {
                (Some(from), Some(to)) => records
                    .iter()
                    .filter(|r| r.ts_ms >= from && r.ts_ms <= to)
                    .collect(),
                _ => Vec::new(),
            };
            let (avg, samples) = avg_speed(inside.iter().copied());
            let mut by_model: std::collections::HashMap<(&str, &str), usize> = Default::default();
            for r in &inside {
                *by_model.entry((&r.model, &r.provider)).or_default() += 1;
            }
            let top = by_model.into_iter().max_by_key(|(_, n)| *n).map(|(k, _)| k);
            TurnSpeed {
                turn_id: turn.turn_id.clone(),
                started_at: turn.started_at,
                completed_at: turn.completed_at,
                duration_ms: turn.duration_ms,
                time_to_first_token_ms: turn.time_to_first_token_ms,
                requests: inside.len() as u64,
                output_tokens: inside.iter().map(|r| r.output).sum(),
                avg_tok_per_s: avg,
                speed_samples: samples,
                model: top.map(|(m, _)| m.to_string()),
                provider: top.map(|(_, p)| p.to_string()),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(ts_ms: u64, output: u64, gen_ms: Option<u64>) -> RequestRecord {
        RequestRecord {
            ts_ms,
            source: "live".into(),
            thread_id: "t".into(),
            model: "m".into(),
            provider: "p".into(),
            account: None,
            limit_id: None,
            uncached_input: 10,
            cached_input: 0,
            output,
            reasoning: 0,
            gen_ms,
            tok_per_s: RequestRecord::speed(output, gen_ms),
            five_hour: None,
            weekly: None,
        }
    }

    const ROLLOUT: &str = concat!(
        r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"a","started_at":1000}}"#,
        "\n",
        r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"a","started_at":1000,"completed_at":1010,"duration_ms":10000,"time_to_first_token_ms":900}}"#,
        "\n",
        "not json\n",
        r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"b","started_at":1020}}"#,
        "\n",
        r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"b","started_at":1020,"completed_at":1030,"duration_ms":9000}}"#,
        "\n",
        r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"c","started_at":1040}}"#,
        "\n",
    );

    #[test]
    fn turns_are_read_from_the_rollout_and_an_open_turn_has_no_end() {
        let turns = parse_turns(ROLLOUT);
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].duration_ms, Some(10_000));
        assert_eq!(turns[0].time_to_first_token_ms, Some(900));
        assert_eq!(turns[1].time_to_first_token_ms, None);
        assert_eq!(turns[2].completed_at, None);
    }

    #[test]
    fn a_turn_speed_comes_only_from_measured_requests_inside_it() {
        let turns = parse_turns(ROLLOUT);
        let records = vec![
            record(1_003_000, 100, Some(1_000)),
            record(1_008_000, 300, Some(3_000)),
            // Outside every turn.
            record(1_015_000, 50, Some(500)),
            // Inside turn b but not measured.
            record(1_025_000, 80, None),
        ];
        let speeds = turn_speeds(&turns, &records);
        assert_eq!(speeds[0].requests, 2);
        assert_eq!(speeds[0].output_tokens, 400);
        assert_eq!(speeds[0].speed_samples, 2);
        assert_eq!(speeds[0].avg_tok_per_s.map(|v| v.round() as i64), Some(100));
        assert_eq!(speeds[0].model.as_deref(), Some("m"));
        // No measured request: no speed, not a guess from the duration.
        assert_eq!(speeds[1].requests, 1);
        assert_eq!(speeds[1].avg_tok_per_s, None);
        // A turn that did not end cannot be matched.
        assert_eq!(speeds[2].requests, 0);
        assert_eq!(speeds[2].avg_tok_per_s, None);
    }
}
