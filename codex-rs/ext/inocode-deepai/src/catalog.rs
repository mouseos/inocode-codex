//! The model catalog: `GET {BASE}/models`, read without a sign-in and checked before use.
//! The checks are those of DeepAI Code 0.3.35 (`decodeDeepAIModels` in its server bundle).
//!
//! No list of models is bundled. The only fallback is a single minimal entry, so that the first
//! screen after a start is not empty when the endpoint cannot be reached.

use serde::Deserialize;
use serde::Serialize;
use std::sync::RwLock;

const MAX_MODELS: usize = 100;

/// One model of the catalog, as the endpoint describes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModel {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub input: Vec<String>,
    pub context_window: i64,
    pub max_tokens: i64,
    /// The old mark of a Theos-consuming model.
    #[serde(default)]
    pub premier: Option<bool>,
    /// `openai-responses` for a model called in the Responses format; absent for Chat Completions.
    #[serde(default)]
    pub api: Option<String>,
    /// Theos consumed by one request, as a string (`"0"` is free).
    #[serde(default)]
    pub theos_per_request: Option<String>,
    /// Per-second consumption by reasoning level (the older form); kept as it came.
    #[serde(default)]
    pub theos_per_second: Option<serde_json::Value>,
}

const LEVELS: [&str; 4] = ["none", "low", "medium", "high"];

impl CatalogModel {
    /// Free: `theosPerRequest` is `"0"` and the model is not marked `premier`.
    pub fn is_free(&self) -> bool {
        self.theos_per_request
            .as_deref()
            .is_some_and(|amount| theos_number(amount) == Some(0.0))
            && self.premier != Some(true)
    }

    /// The rate the catalog gives, when it gives a readable one: `Some(true)` for a rate with a
    /// non-zero digit (a per-request price, else the per-level rates), `Some(false)` for zero,
    /// `None` when the catalog carries no usable price (`hasMeteredRate` of the original).
    fn metered_by_rate(&self) -> Option<bool> {
        let nonzero = |amount: &str| amount.bytes().any(|b| (b'1'..=b'9').contains(&b));
        if let Some(amount) = &self.theos_per_request {
            return theos_number(amount).map(|_| nonzero(amount));
        }
        let rates: Vec<&serde_json::Value> = self
            .theos_per_second
            .as_ref()
            .and_then(serde_json::Value::as_object)
            .map(|map| map.values().collect())
            .unwrap_or_default();
        if rates.is_empty() {
            return None;
        }
        let texts: Option<Vec<&str>> = rates
            .iter()
            .map(|rate| rate.as_str().filter(|text| theos_number(text).is_some()))
            .collect();
        texts.map(|texts| texts.iter().any(|text| nonzero(text)))
    }

    /// Consumes Theos, by the original client's rule: the catalog's rate when it has one, else
    /// whether the model is marked `premier` (a model with neither counts as free here; a model
    /// missing from the catalog counts as consuming, see `model_is_metered`).
    pub fn is_metered(&self) -> bool {
        self.metered_by_rate().unwrap_or(self.premier == Some(true))
    }

    pub fn takes_images(&self) -> bool {
        self.input.iter().any(|kind| kind == "image")
    }

    /// Called in the Responses format.
    pub fn uses_responses(&self) -> bool {
        self.api.as_deref() == Some("openai-responses")
    }
}

/// A validated catalog.
#[derive(Clone, Debug, PartialEq)]
pub struct Catalog {
    pub models: Vec<CatalogModel>,
    pub default: Option<String>,
}

/// An amount of Theos: digits with an optional fractional part (`^\d+(\.\d+)?$`).
pub fn theos_number(text: &str) -> Option<f64> {
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (text, None),
    };
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || fraction.is_some_and(|f| !digits(f)) {
        return None;
    }
    text.parse().ok()
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// Reads and checks the answer of `/models`. A catalog that fails any check is not used at all:
/// no entries or more than 100, a bad id, a `maxTokens` above the context window.
pub fn parse(body: &str) -> Result<Catalog, String> {
    #[derive(Deserialize)]
    struct Wire {
        models: Vec<serde_json::Value>,
        #[serde(default)]
        default: Option<String>,
    }
    let wire: Wire =
        serde_json::from_str(body).map_err(|error| format!("unreadable catalog: {error}"))?;
    if wire.models.is_empty() || wire.models.len() > MAX_MODELS {
        return Err(format!("the catalog has {} entries", wire.models.len()));
    }
    let mut models = Vec::with_capacity(wire.models.len());
    for value in wire.models {
        let model: CatalogModel = serde_json::from_value(value)
            .map_err(|error| format!("a catalog entry is unreadable: {error}"))?;
        if !valid_id(&model.id) {
            return Err("a catalog entry has an invalid id".to_string());
        }
        if models.iter().any(|seen: &CatalogModel| seen.id == model.id) {
            return Err(format!("`{}` is listed twice", model.id));
        }
        if model.name.trim().is_empty()
            || model.name.chars().count() > 100
            || !model.input.iter().any(|kind| kind == "text")
            || model.input.iter().any(|kind| kind != "text" && kind != "image")
        {
            return Err(format!("`{}` has an invalid name or input", model.id));
        }
        if model.context_window <= 0
            || model.max_tokens <= 0
            || model.max_tokens > model.context_window
        {
            return Err(format!("`{}` has inconsistent token limits", model.id));
        }
        // The original accepts only `openai-responses` here; anything else voids the catalog
        // rather than routing a model by an invented wire format.
        if model.api.as_deref().is_some_and(|api| api != "openai-responses") {
            return Err(format!("`{}` names an unknown api", model.id));
        }
        if let Some(amount) = &model.theos_per_request
            && theos_number(amount).is_none()
        {
            return Err(format!("`{}` has an unreadable theosPerRequest", model.id));
        }
        if let Some(rates) = &model.theos_per_second {
            let valid = rates.as_object().is_some_and(|map| {
                map.iter().all(|(level, rate)| {
                    LEVELS.contains(&level.as_str())
                        && rate.as_str().is_some_and(|text| theos_number(text).is_some())
                })
            });
            if !valid {
                return Err(format!("`{}` has an unreadable theosPerSecond", model.id));
            }
        }
        models.push(model);
    }
    // `default` is not part of the original's checks; a missing or unlisted one falls back to
    // the first model (see `Catalog::default_model`).
    let default = wire
        .default
        .filter(|id| models.iter().any(|model| &model.id == id));
    Ok(Catalog { models, default })
}

/// The single entry used when nothing can be read.
pub fn fallback() -> Catalog {
    Catalog {
        models: vec![CatalogModel {
            id: "glm-5.3-flash".to_string(),
            name: "GLM 5.3 Flash".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            context_window: 1_048_576,
            max_tokens: 131_072,
            premier: None,
            api: None,
            // The original's bundled entry carries no price fields.
            theos_per_request: None,
            theos_per_second: None,
        }],
        default: Some("glm-5.3-flash".to_string()),
    }
}

impl Catalog {
    /// The model to start with: the catalog's `default` when it is listed, else the first entry.
    pub fn default_model(&self) -> Option<&CatalogModel> {
        self.default
            .as_deref()
            .and_then(|id| self.models.iter().find(|model| model.id == id))
            .or_else(|| self.models.first())
    }

    pub fn find(&self, id: &str) -> Option<&CatalogModel> {
        self.models.iter().find(|model| model.id == id)
    }
}

/// Reads `GET {base_url}/models`.
pub async fn fetch(base_url: &str) -> Result<Catalog, String> {
    // Sent as the original sends it: JSON content type, lane, instance and machine headers, no
    // key, and a 5 second limit.
    let mut request = crate::http_client()?
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .timeout(std::time::Duration::from_secs(5))
        .header("Content-Type", "application/json");
    for (name, value) in crate::headers::common(None) {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("the model list could not be read: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("the model list could not be read: {error}"))?;
    if !status.is_success() {
        return Err(format!("the model list answered HTTP {}", status.as_u16()));
    }
    parse(&body)
}

static CURRENT: RwLock<Option<Catalog>> = RwLock::new(None);

/// Keeps the catalog the process is using, so that the call path can look a model up.
pub fn remember(catalog: &Catalog) {
    *CURRENT
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(catalog.clone());
}

/// The catalog read last in this process (`None` before the first read).
pub fn current() -> Option<Catalog> {
    CURRENT
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Whether `slug` is called in the Responses format. Unknown models use Chat Completions.
pub fn model_uses_responses(slug: &str) -> bool {
    current()
        .and_then(|catalog| catalog.find(slug).map(CatalogModel::uses_responses))
        .unwrap_or(false)
}

/// Whether a request to `slug` consumes Theos. A model that is not in the catalog counts as
/// consuming (the rule of the original client).
pub fn model_is_metered(slug: &str) -> bool {
    current()
        .and_then(|catalog| catalog.find(slug).map(CatalogModel::is_metered))
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn model(extra: &str) -> String {
        format!(
            r#"{{"id":"m","name":"M","reasoning":true,"input":["text","image"],"contextWindow":1000,"maxTokens":100{extra}}}"#
        )
    }

    #[test]
    fn free_means_zero_per_request_and_not_premier() {
        let named = |id: &str, extra: &str| model(extra).replacen("\"m\"", &format!("\"{id}\""), 1);
        let catalog = parse(&format!(
            r#"{{"models":[{},{},{}],"default":"m"}}"#,
            named("m", r#","theosPerRequest":"0","premier":false"#),
            named("n", r#","theosPerRequest":"10","premier":true"#),
            named("o", r#","theosPerRequest":"0","premier":true"#),
        ))
        .expect("valid");
        let flags: Vec<(bool, bool)> = catalog
            .models
            .iter()
            .map(|m| (m.is_free(), m.is_metered()))
            .collect();
        // `o` is not free (marked premier) and, by its price of 0, does not consume either.
        assert_eq!(flags, vec![(true, false), (false, true), (false, false)]);
        assert!(catalog.models[0].takes_images());
    }

    #[test]
    fn metering_follows_the_original_order_of_checks() {
        let by = |extra: &str| {
            parse(&format!(r#"{{"models":[{}],"default":"m"}}"#, model(extra)))
                .expect("valid")
                .models
                .remove(0)
        };
        assert!(!by("").is_metered(), "neither a rate nor premier");
        assert!(by(r#","premier":true"#).is_metered());
        assert!(!by(r#","premier":false"#).is_metered());
        // A readable per-request price decides, whatever `premier` says.
        assert!(!by(r#","theosPerRequest":"0","premier":true"#).is_metered());
        assert!(by(r#","theosPerRequest":"0.5""#).is_metered());
        // Per-level rates only when there is no per-request price.
        assert!(by(r#","theosPerSecond":{"high":"0.5"}"#).is_metered());
        assert!(!by(r#","theosPerSecond":{"high":"0"}"#).is_metered());
        assert!(by(r#","theosPerSecond":{"high":"0","low":"2"}"#).is_metered());
    }

    #[test]
    fn a_catalog_failing_a_check_is_rejected_whole() {
        assert!(parse(r#"{"models":[]}"#).is_err());
        let many = (0..101)
            .map(|i| model("").replacen("\"m\"", &format!("\"m{i}\""), 1))
            .collect::<Vec<_>>()
            .join(",");
        assert!(parse(&format!(r#"{{"models":[{many}],"default":"m0"}}"#)).is_err());
        let twice = format!(r#"{{"models":[{},{}],"default":"m"}}"#, model(""), model(""));
        assert!(parse(&twice).is_err(), "a repeated id");
        // The original does not read `default`: a missing or unlisted one is not a refusal.
        let unlisted = parse(&format!(r#"{{"models":[{}],"default":"x"}}"#, model(""))).expect("ok");
        assert_eq!(unlisted.default, None);
        assert_eq!(unlisted.default_model().map(|m| m.id.as_str()), Some("m"));
        assert!(parse(&format!(r#"{{"models":[{}]}}"#, model(""))).is_ok());
        let bad_rate = model(r#","theosPerSecond":{"extreme":"1"}"#);
        assert!(parse(&format!(r#"{{"models":[{bad_rate}],"default":"m"}}"#)).is_err());
        let no_text = model("").replace(r#""input":["text","image"]"#, r#""input":["image"]"#);
        assert!(parse(&format!(r#"{{"models":[{no_text}],"default":"m"}}"#)).is_err());
        let bad_api = model(r#","api":"openai-completions""#);
        assert!(parse(&format!(r#"{{"models":[{bad_api}],"default":"m"}}"#)).is_err());
        assert!(
            parse(r#"{"models":[{"id":"a b","name":"x","input":["text"],"contextWindow":10,"maxTokens":5}],"default":"a b"}"#)
                .is_err()
        );
        assert!(
            parse(r#"{"models":[{"id":"a","name":"x","input":["text"],"contextWindow":10,"maxTokens":50}],"default":"a"}"#)
                .is_err()
        );
        // An unreadable theosPerRequest voids the catalog, as in the original.
        assert!(
            parse(&format!(
                r#"{{"models":[{}],"default":"m"}}"#,
                model(r#","theosPerRequest":"x""#)
            ))
            .is_err()
        );
    }

    #[test]
    fn the_default_is_the_listed_default_else_the_first() {
        let catalog =
            parse(&format!(r#"{{"models":[{}],"default":"m"}}"#, model(""))).expect("valid");
        assert_eq!(catalog.default_model().map(|m| m.id.as_str()), Some("m"));
        assert_eq!(
            fallback().default_model().map(|m| m.id.as_str()),
            Some("glm-5.3-flash")
        );
    }

    #[test]
    fn theos_amounts_follow_the_documented_shape() {
        assert_eq!(theos_number("12.5"), Some(12.5));
        assert_eq!(theos_number("0"), Some(0.0));
        assert_eq!(theos_number("-1"), None);
        assert_eq!(theos_number("1."), None);
        assert_eq!(theos_number(""), None);
    }
}
