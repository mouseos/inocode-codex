//! The model catalog: `GET {BASE}/models`, read without a sign-in and checked before use.
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

impl CatalogModel {
    /// Free: `theosPerRequest` is `"0"` and the model is not `premier`.
    pub fn is_free(&self) -> bool {
        self.theos_per_request
            .as_deref()
            .is_some_and(|amount| theos_number(amount) == Some(0.0))
            && self.premier == Some(false)
    }

    /// Consumes Theos. The rule of the original client: with `theosPerRequest` the amount
    /// decides; else any positive `theosPerSecond`; else `premier`.
    pub fn is_metered(&self) -> bool {
        if let Some(amount) = self.theos_per_request.as_deref() {
            return theos_number(amount).is_none_or(|n| n > 0.0);
        }
        if let Some(rates) = &self.theos_per_second {
            let positive = |value: &serde_json::Value| match value {
                serde_json::Value::String(text) => theos_number(text).is_some_and(|n| n > 0.0),
                serde_json::Value::Number(number) => number.as_f64().is_some_and(|n| n > 0.0),
                _ => false,
            };
            let any = match rates {
                serde_json::Value::Object(map) => map.values().any(positive),
                other => positive(other),
            };
            if any {
                return true;
            }
        }
        // No `premier` at all counts as consuming (the original client's reading).
        self.premier != Some(false)
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
        if model.context_window <= 0
            || model.max_tokens <= 0
            || model.max_tokens > model.context_window
        {
            return Err(format!("`{}` has inconsistent token limits", model.id));
        }
        if models.iter().any(|seen: &CatalogModel| seen.id == model.id) {
            return Err(format!("`{}` is listed twice", model.id));
        }
        if model.name.is_empty()
            || model.input.is_empty()
            || model.input.iter().any(|kind| kind != "text" && kind != "image")
            || !model.input.iter().any(|kind| kind == "text")
        {
            return Err(format!("`{}` has an invalid name or input", model.id));
        }
        if model.api.as_deref().is_some_and(|api| api != "openai-completions" && api != "openai-responses") {
            return Err(format!("`{}` names an unknown api", model.id));
        }
        // An unreadable `theosPerRequest` is not rejected: the model counts as metered.
        models.push(model);
    }
    // The default must name a listed model, else the whole catalog is refused.
    let default = wire
        .default
        .filter(|id| models.iter().any(|model| &model.id == id))
        .ok_or_else(|| "the default model is not in the catalog".to_string())?;
    Ok(Catalog {
        models,
        default: Some(default),
    })
}

/// The single entry used when nothing can be read.
pub fn fallback() -> Catalog {
    Catalog {
        models: vec![CatalogModel {
            id: "glm-5.3-flash".to_string(),
            name: "GLM 5.3 Flash".to_string(),
            reasoning: true,
            input: vec!["text".to_string()],
            context_window: 128_000,
            max_tokens: 16_000,
            premier: Some(false),
            api: None,
            theos_per_request: Some("0".to_string()),
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
    let response = crate::http_client()?
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .header("X-DeepAI-Code-Theos", crate::LANE_VERSION)
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
        assert!(by("").is_metered(), "no premier at all counts as consuming");
        assert!(!by(r#","premier":false"#).is_metered());
        assert!(by(r#","premier":true"#).is_metered());
        assert!(by(r#","theosPerSecond":{"high":"0.5"}"#).is_metered());
        assert!(!by(r#","premier":false,"theosPerSecond":{"high":"0"}"#).is_metered());
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
        assert!(parse(&format!(r#"{{"models":[{}],"default":"x"}}"#, model(""))).is_err());
        assert!(parse(&format!(r#"{{"models":[{}]}}"#, model(""))).is_err(), "no default");
        let no_text = model("").replace(r#""input":["text","image"]"#, r#""input":["image"]"#);
        assert!(parse(&format!(r#"{{"models":[{no_text}],"default":"m"}}"#)).is_err());
        let bad_api = model(r#","api":"weird""#);
        assert!(parse(&format!(r#"{{"models":[{bad_api}],"default":"m"}}"#)).is_err());
        assert!(
            parse(r#"{"models":[{"id":"a b","name":"x","input":["text"],"contextWindow":10,"maxTokens":5}],"default":"a b"}"#)
                .is_err()
        );
        assert!(
            parse(r#"{"models":[{"id":"a","name":"x","input":["text"],"contextWindow":10,"maxTokens":50}],"default":"a"}"#)
                .is_err()
        );
        // An unreadable theosPerRequest is not a reason to refuse; the model counts as metered.
        let odd = parse(&format!(
            r#"{{"models":[{}],"default":"m"}}"#,
            model(r#","theosPerRequest":"x""#)
        ))
        .expect("accepted");
        assert!(odd.models[0].is_metered() && !odd.models[0].is_free());
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
