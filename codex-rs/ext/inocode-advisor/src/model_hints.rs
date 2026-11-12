//! A short line per model about its benchmarks and list price, for the model list the main agent
//! reads when it chooses a model for delegated work. The app-server sets the lines from the model
//! catalog's answer; the spawn tool's description reads them. Nothing is set until the catalog has
//! been read once in the process, and then the description simply has no line.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;

fn table() -> &'static Mutex<HashMap<String, String>> {
    static TABLE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    TABLE.get_or_init(Default::default)
}

fn key(model: &str) -> String {
    model.trim().to_lowercase()
}

/// Sets the line of a model, or removes it with `None`. Other models keep theirs.
pub fn set_model_hint(model: &str, line: Option<String>) {
    if let Ok(mut map) = table().lock() {
        match line {
            Some(line) => map.insert(key(model), line),
            None => map.remove(&key(model)),
        };
    }
}

/// The line for a model, if one was set.
pub fn model_hint(model: &str) -> Option<String> {
    table().lock().ok()?.get(&key(model)).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_found_by_model_id_and_set_per_model() {
        set_model_hint("GPT-X", Some("coding 71 (rank 3 of 40)".to_string()));
        set_model_hint("other", Some("speed 90 tok/s".to_string()));
        assert_eq!(model_hint("gpt-x").as_deref(), Some("coding 71 (rank 3 of 40)"));
        set_model_hint("gpt-x", None);
        assert_eq!(model_hint("gpt-x"), None);
        assert_eq!(model_hint("other").as_deref(), Some("speed 90 tok/s"));
        set_model_hint("other", None);
    }
}
