//! Disk cache and polite fetching.
//!
//! Each source is cached under `<home>/cache/model-catalog/`. A cached entry is used as long as it
//! is younger than its window. Reads are spaced, and the same source is not asked again within
//! ten minutes, even after a failure or an explicit refresh. When fetching is off, or a fetch
//! fails, the cached data (however old) is used and marked stale.

use crate::Options;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::Mutex;

/// The same source is not requested again within this many seconds.
const MIN_ATTEMPT_GAP_SECS: i64 = 600;
/// Pause between two network reads of one answer.
const REQUEST_SPACING: Duration = Duration::from_millis(250);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct Loaded<T> {
    pub data: Option<T>,
    pub fetched_at: Option<i64>,
    /// There is data, but it is older than its window.
    pub stale: bool,
}

pub(crate) struct Fetcher {
    dir: PathBuf,
    fetch_enabled: bool,
    refresh: bool,
    client: Option<reqwest::Client>,
    spacing: Mutex<bool>,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Entry<T> {
    fetched_at: i64,
    data: T,
}

impl Fetcher {
    pub(crate) fn new(home: &std::path::Path, options: &Options) -> Self {
        let client = reqwest::Client::builder()
            .user_agent("inocode-model-catalog")
            .timeout(REQUEST_TIMEOUT)
            .build()
            .ok();
        Self {
            dir: home.join("cache").join("model-catalog"),
            fetch_enabled: options.fetch_enabled,
            refresh: options.refresh,
            client,
            spacing: Mutex::new(false),
        }
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    fn attempts(&self) -> BTreeMap<String, i64> {
        std::fs::read_to_string(self.dir.join("attempts.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn note_attempt(&self, key: &str, at: i64) {
        let mut attempts = self.attempts();
        attempts.insert(key.to_string(), at);
        write_atomic(&self.dir.join("attempts.json"), &attempts);
    }

    /// Reads `key` from the cache, or fetches it with `fetch` when it is older than `ttl_secs`.
    pub(crate) async fn load<T, F, Fut>(&self, key: &str, ttl_secs: i64, fetch: F) -> Loaded<T>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce(reqwest::Client) -> Fut,
        Fut: Future<Output = Result<T, String>>,
    {
        let at = now();
        let cached: Option<Entry<T>> = std::fs::read_to_string(self.path(key))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        let fresh = cached.as_ref().is_some_and(|e| at - e.fetched_at < ttl_secs);
        let allowed = self.fetch_enabled
            && (!fresh || self.refresh)
            && self
                .attempts()
                .get(key)
                .is_none_or(|last| at - last >= MIN_ATTEMPT_GAP_SECS);
        if allowed && let Some(client) = self.client.clone() {
            {
                let mut spaced = self.spacing.lock().await;
                if *spaced {
                    tokio::time::sleep(REQUEST_SPACING).await;
                }
                *spaced = true;
            }
            self.note_attempt(key, at);
            // A failed read is not an error to the caller: the cache stands in for it.
            if let Ok(data) = fetch(client).await {
                write_atomic(
                    &self.path(key),
                    &Entry {
                        fetched_at: at,
                        data: &data,
                    },
                );
                return Loaded {
                    data: Some(data),
                    fetched_at: Some(at),
                    stale: false,
                };
            }
        }
        match cached {
            Some(entry) => Loaded {
                fetched_at: Some(entry.fetched_at),
                stale: at - entry.fetched_at >= ttl_secs,
                data: Some(entry.data),
            },
            None => Loaded {
                data: None,
                fetched_at: None,
                stale: false,
            },
        }
    }
}

fn write_atomic<T: Serialize>(path: &std::path::Path, value: &T) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(value) {
        let temp = path.with_extension("json.tmp");
        if std::fs::write(&temp, text).is_ok() {
            let _ = std::fs::rename(&temp, path);
        }
    }
}

/// GET `url` and parse the body as JSON. The error never carries the body.
pub(crate) async fn get_json(
    client: &reqwest::Client,
    url: &str,
) -> Result<serde_json::Value, String> {
    let response = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|_| "request failed".to_string())?;
    if !response.status().is_success() {
        return Err(format!("status {}", response.status().as_u16()));
    }
    let text = response
        .text()
        .await
        .map_err(|_| "unreadable body".to_string())?;
    serde_json::from_str(&text).map_err(|_| "not JSON".to_string())
}
