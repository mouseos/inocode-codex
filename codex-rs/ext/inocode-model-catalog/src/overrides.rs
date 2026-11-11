//! Prices the user set by hand. A fetch never touches this file.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use serde::Deserialize;
use serde::Serialize;

const FILE: &str = "model-price-overrides.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PriceOverride {
    /// USD per 1M tokens.
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    /// Unix seconds.
    pub set_at: i64,
}

pub(crate) fn key(provider: &str, model_id: &str) -> String {
    format!("{}/{}", provider.trim().to_ascii_lowercase(), model_id.trim())
}

pub fn read_overrides(home: &Path) -> BTreeMap<String, PriceOverride> {
    std::fs::read_to_string(home.join(FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Sets (or, with `None`, clears) the user's own price for a model.
pub fn write_override(
    home: &Path,
    provider: &str,
    model_id: &str,
    value: Option<PriceOverride>,
) -> io::Result<()> {
    let mut all = read_overrides(home);
    let key = key(provider, model_id);
    match value {
        Some(value) => {
            all.insert(key, value);
        }
        None => {
            all.remove(&key);
        }
    }
    std::fs::create_dir_all(home)?;
    let text = serde_json::to_string_pretty(&all).map_err(io::Error::other)?;
    let temp = home.join(format!("{FILE}.tmp"));
    std::fs::write(&temp, text)?;
    std::fs::rename(&temp, home.join(FILE))
}
