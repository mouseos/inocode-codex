//! Which benchmark metrics answer which use. The default table is below; the user can change it
//! with `model-use-map.json` in the InoCode home: `{"coding": ["metric", ...], ...}`. A use named
//! there replaces that use's list (an empty list leaves the use without metrics); other uses keep
//! the defaults. Metric names are letters, digits and `_` only.

use std::collections::BTreeMap;
use std::path::Path;

const DEFAULT: [(&str, &[&str]); 7] = [
    (
        "coding",
        &[
            "livebench_coding",
            "vals_vibe_code",
            "tbench_4_0_accuracy",
            "scale_swe_bench_pro_v2",
            "cursorbench_4_0_score",
            "epoch_frontierswe",
        ],
    ),
    (
        "reasoning",
        &["livebench_reasoning", "epoch_gpqa_diamond", "epoch_arc_agi"],
    ),
    (
        "math",
        &["epoch_otis_mock_aime", "epoch_frontiermath", "epoch_proofbench"],
    ),
    (
        "agentic",
        &["livebench_long_agentic", "livebench_browser", "epoch_apex_agents"],
    ),
    // No dedicated long-context metric was found in the sources; the user can add one.
    ("long_context", &[]),
    ("general", &["aa_intelligence_index"]),
    (
        "speed",
        &["or_output_tokens_per_second", "or_time_to_first_token_s"],
    ),
];

pub(crate) fn valid_metric(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 80
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

pub fn load_use_map(home: &Path) -> BTreeMap<String, Vec<String>> {
    let mut map: BTreeMap<String, Vec<String>> = DEFAULT
        .iter()
        .map(|(use_name, metrics)| {
            (
                (*use_name).to_string(),
                metrics.iter().map(|m| (*m).to_string()).collect(),
            )
        })
        .collect();
    let custom = std::fs::read_to_string(home.join("model-use-map.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<BTreeMap<String, Vec<String>>>(&text).ok());
    for (use_name, metrics) in custom.into_iter().flatten() {
        let use_ok = !use_name.is_empty()
            && use_name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if use_ok {
            let metrics = metrics.into_iter().filter(|m| valid_metric(m)).collect();
            map.insert(use_name, metrics);
        }
    }
    map
}
