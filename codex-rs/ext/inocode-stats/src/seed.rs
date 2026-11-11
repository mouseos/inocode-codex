//! Seeds the statistics from Codex's own session history (read-only).
//!
//! `~/.codex/sessions/**/*.jsonl` holds `token_count` events with the usage of each request and
//! the account's rate limits. Files can be hundreds of megabytes, so they are streamed line by
//! line and only the few line kinds that matter are parsed.

use crate::record::LimitSample;
use crate::record::RequestRecord;
use crate::record::StatsStore;
use crate::record::classify_window;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;

pub const SEED_SOURCE: &str = "codex-seed";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeedReport {
    pub files: u64,
    pub bytes: u64,
    /// Requests added to the statistics.
    pub imported: u64,
    /// Requests skipped because an earlier import already holds them.
    pub duplicates: u64,
}

/// `~/.codex/sessions` (or `$CODEX_HOME/sessions`).
pub fn default_sessions_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("CODEX_HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home).join("sessions");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
        .join("sessions")
}

fn u(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn limit(window: &Value) -> Option<(Option<i64>, LimitSample)> {
    let used_percent = window.get("used_percent")?.as_f64()?;
    Some((
        window.get("window_minutes").and_then(Value::as_i64),
        LimitSample {
            used_percent,
            resets_at: window.get("resets_at").and_then(Value::as_i64),
        },
    ))
}

fn thread_of(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    // rollout-<date>T<time>-<uuid>
    stem.get(stem.len().saturating_sub(36)..).unwrap_or(stem).to_string()
}

fn requests_of_file(path: &Path) -> std::io::Result<Vec<RequestRecord>> {
    let mut reader = BufReader::with_capacity(1 << 20, std::fs::File::open(path)?);
    let thread_id = thread_of(path);
    let mut model = String::from("unknown");
    let mut prev_total = 0u64;
    let mut out = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        // Invalid UTF-8 in one line must not stop the file.
        let mut bytes = Vec::new();
        if reader.read_until(b'\n', &mut bytes)? == 0 {
            break;
        }
        line.push_str(&String::from_utf8_lossy(&bytes));
        let is_context = line.contains("\"type\":\"turn_context\"");
        let is_count = !is_context && line.contains("\"type\":\"token_count\"");
        if !is_context && !is_count {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(payload) = value.get("payload") else {
            continue;
        };
        if is_context {
            if let Some(m) = payload.get("model").and_then(Value::as_str) {
                model = m.to_string();
            }
            continue;
        }
        let Some(info) = payload.get("info").filter(|i| !i.is_null()) else {
            continue;
        };
        let total = info
            .get("total_token_usage")
            .map(|t| u(t, "total_tokens"))
            .unwrap_or(0);
        if total == 0 || total == prev_total {
            continue; // repeated reading without a new request
        }
        prev_total = total;
        let Some(last) = info.get("last_token_usage") else {
            continue;
        };
        let input = u(last, "input_tokens");
        let cached = u(last, "cached_input_tokens").min(input);
        let Some(ts) = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .and_then(|t| u64::try_from(t.timestamp_millis()).ok())
        else {
            continue;
        };
        let (mut five_hour, mut weekly) = (None, None);
        if let Some(limits) = payload.get("rate_limits") {
            for key in ["primary", "secondary"] {
                if let Some((minutes, sample)) = limits.get(key).and_then(limit) {
                    classify_window(minutes, sample, &mut five_hour, &mut weekly);
                }
            }
        }
        out.push(RequestRecord {
            ts_ms: ts,
            source: SEED_SOURCE.into(),
            thread_id: thread_id.clone(),
            model: model.clone(),
            provider: "openai".into(),
            account: None,
            limit_id: payload
                .get("rate_limits")
                .and_then(|l| l.get("limit_id"))
                .and_then(Value::as_str)
                .map(str::to_string),
            uncached_input: input - cached,
            cached_input: cached,
            output: u(last, "output_tokens"),
            reasoning: u(last, "reasoning_output_tokens"),
            gen_ms: None,
            tok_per_s: None,
            five_hour,
            weekly,
        });
    }
    Ok(out)
}

/// Imports every `*.jsonl` under `sessions_dir` into `store`. Safe to run again: requests already
/// imported (same time, thread and output) are skipped.
pub fn seed_from_codex(store: &StatsStore, sessions_dir: &Path) -> std::io::Result<SeedReport> {
    seed_with_progress(store, sessions_dir, &mut |_| {})
}

/// Progress after each file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeedProgress {
    pub files_done: u64,
    pub files_total: u64,
    pub samples_added: u64,
}

/// [`seed_from_codex`] reporting progress after every file.
pub fn seed_with_progress(
    store: &StatsStore,
    sessions_dir: &Path,
    progress: &mut dyn FnMut(SeedProgress),
) -> std::io::Result<SeedReport> {
    let mut seen: HashSet<(u64, String, u64)> = store
        .read_since(0)
        .into_iter()
        .filter(|r| r.source == SEED_SOURCE)
        .map(|r| (r.ts_ms, r.thread_id, r.output))
        .collect();
    let mut report = SeedReport::default();
    if !sessions_dir.exists() {
        return Ok(report);
    }
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(sessions_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "jsonl"))
        .map(walkdir::DirEntry::into_path)
        .collect();
    files.sort();
    let files_total = files.len() as u64;
    for path in files {
        report.files += 1;
        report.bytes += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let Ok(records) = requests_of_file(&path) else {
            continue;
        };
        let fresh: Vec<RequestRecord> = records
            .into_iter()
            .filter(|r| {
                let new = seen.insert((r.ts_ms, r.thread_id.clone(), r.output));
                if !new {
                    report.duplicates += 1;
                }
                new
            })
            .collect();
        report.imported += fresh.len() as u64;
        store.append_all(&fresh)?;
        progress(SeedProgress {
            files_done: report.files,
            files_total,
            samples_added: report.imported,
        });
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn count(ts: &str, total: u64, input: u64, cached: u64, out: u64, p: f64, w: f64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"total_tokens":{total}}},"last_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"output_tokens":{out},"reasoning_output_tokens":0,"total_tokens":{}}}}},"rate_limits":{{"primary":{{"used_percent":{p},"window_minutes":300,"resets_at":1000}},"secondary":{{"used_percent":{w},"window_minutes":10080,"resets_at":5000}}}}}}}}"#,
            input + out
        )
    }

    #[test]
    fn imports_requests_with_model_and_limits_and_skips_repeats() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions/2026/01/02");
        std::fs::create_dir_all(&sessions).unwrap();
        let body = [
            r#"{"timestamp":"2026-01-02T00:00:00.000Z","type":"turn_context","payload":{"model":"gpt-x"}}"#.to_string(),
            r#"{"type":"response_item","payload":{"output":"original_token_count 12"}}"#.to_string(),
            count("2026-01-02T00:00:01.000Z", 150, 100, 40, 50, 7.0, 2.0),
            // The same reading repeated: skipped.
            count("2026-01-02T00:00:02.000Z", 150, 100, 40, 50, 7.0, 2.0),
            count("2026-01-02T00:00:09.000Z", 400, 200, 0, 50, 8.0, 2.0),
            "not json {".to_string(),
        ]
        .join("\n");
        std::fs::write(sessions.join("rollout-2026-01-02T09-00-00-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl"), body)
            .unwrap();
        let store = StatsStore::at(&dir.path().join("home"));
        let report = seed_from_codex(&store, &dir.path().join("sessions")).unwrap();
        assert_eq!(report.imported, 2);
        let rows = store.read_since(0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].model, "gpt-x");
        assert_eq!(rows[0].uncached_input, 60);
        assert_eq!(rows[0].cached_input, 40);
        assert_eq!(rows[0].five_hour.unwrap().used_percent, 7.0);
        assert_eq!(rows[0].weekly.unwrap().used_percent, 2.0);
        assert_eq!(rows[0].thread_id, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        // Running it again adds nothing.
        let again = seed_from_codex(&store, &dir.path().join("sessions")).unwrap();
        assert_eq!(again.imported, 0);
        assert_eq!(again.duplicates, 2);
        assert_eq!(store.read_since(0).len(), 2);
    }

    #[test]
    fn a_missing_directory_imports_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = StatsStore::at(dir.path());
        let r = seed_from_codex(&store, &dir.path().join("none")).unwrap();
        assert_eq!(r, SeedReport::default());
    }
}
