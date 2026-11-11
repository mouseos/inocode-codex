use serde::Deserialize;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Quota usage reported with a response, for one window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitSample {
    /// 0–100.
    pub used_percent: f64,
    /// Unix seconds at which the window resets.
    pub resets_at: Option<i64>,
}

/// One model request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestRecord {
    pub ts_ms: u64,
    /// `live` (recorded by the agent) or `codex-seed` (imported from Codex history).
    pub source: String,
    pub thread_id: String,
    pub model: String,
    pub provider: String,
    /// Account alias (never an e-mail address).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    pub uncached_input: u64,
    pub cached_input: u64,
    pub output: u64,
    #[serde(default)]
    pub reasoning: u64,
    /// Seconds spent generating, in ms: from the first streamed event to completion
    /// (tool waits are not included).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gen_ms: Option<u64>,
    /// Output tokens per second of generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tok_per_s: Option<f64>,
    /// The 5-hour window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<LimitSample>,
    /// The weekly window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly: Option<LimitSample>,
}

impl RequestRecord {
    /// `output / (gen_ms / 1000)`; none when the generation was too short to measure.
    pub fn speed(output: u64, gen_ms: Option<u64>) -> Option<f64> {
        let ms = gen_ms?;
        (ms >= 200 && output > 0).then(|| output as f64 / (ms as f64 / 1000.0))
    }

    pub fn total_tokens(&self) -> u64 {
        self.uncached_input + self.cached_input + self.output
    }
}

/// Splits rate-limit windows into the 5-hour and the weekly one by their length in minutes
/// (providers disagree on which of primary/secondary is which).
pub fn classify_window(
    window_minutes: Option<i64>,
    sample: LimitSample,
    five_hour: &mut Option<LimitSample>,
    weekly: &mut Option<LimitSample>,
) {
    match window_minutes {
        Some(m) if m <= 1440 => *five_hour = Some(sample),
        Some(_) => *weekly = Some(sample),
        None => {
            if five_hour.is_none() {
                *five_hour = Some(sample);
            }
        }
    }
}

pub struct StatsStore {
    path: PathBuf,
    write_lock: Mutex<()>,
}

impl StatsStore {
    pub fn at(inocode_home: &Path) -> Self {
        Self {
            path: inocode_home.join("stats").join("requests.jsonl"),
            write_lock: Mutex::new(()),
        }
    }

    pub fn from_env() -> Self {
        Self::at(&codex_inocode_flags::inocode_home())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, record: &RequestRecord) -> std::io::Result<()> {
        self.append_all(std::slice::from_ref(record))
    }

    pub fn append_all(&self, records: &[RequestRecord]) -> std::io::Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let mut buf = String::new();
        for r in records {
            buf.push_str(&serde_json::to_string(r).map_err(std::io::Error::other)?);
            buf.push('\n');
        }
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(buf.as_bytes())
    }

    /// Records at or after `since_ms`, oldest first. Unparseable lines are skipped.
    pub fn read_since(&self, since_ms: u64) -> Vec<RequestRecord> {
        let Ok(file) = std::fs::File::open(&self.path) else {
            return Vec::new();
        };
        let mut out: Vec<RequestRecord> = BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter_map(|line| serde_json::from_str::<RequestRecord>(&line).ok())
            .filter(|r| r.ts_ms >= since_ms)
            .collect();
        out.sort_by_key(|r| r.ts_ms);
        out
    }
}
