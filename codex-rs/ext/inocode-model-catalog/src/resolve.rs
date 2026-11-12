//! Finding a model in the public sources under its common name.
//!
//! A model offered through another provider (DeepAI, an OpenAI-compatible gateway) carries the
//! upstream model's name, sometimes written a little differently. The sources key a model by
//! `provider/model`, so the lookup first tries the provider as given and then the provider the
//! model's name belongs to (`claude-…` is Anthropic's, `glm-…` is Z.ai's, and so on).
//!
//! Only these steps are taken, each of them exact apart from the written form:
//! 1. the provider as given: the id as it is, or with case and `.`/`-` between digits ignored;
//! 2. the provider the name belongs to: the same, then a `v` before the version number
//!    (`mimo-2.6-flash` is `mimo-v2.6-flash`), no `-` before it (`qwen-3.8-flash` is
//!    `qwen3.8-flash`), without a trailing `-instruct`/`-latest`, and
//!    finally the id plus one date suffix (`-1017`, `-20251001`) **when exactly one such entry
//!    exists**.
//!
//! There is no nearest-name guess: a name that none of these reaches has no scores and no
//! price (empty, never another model's numbers). `matched_by` says which step matched.

use crate::ModelRef;
use crate::normalize::normalize_model;
use crate::normalize::source_provider;
use crate::normalize::split_provider;

/// What was found: the source's provider name, the model's id there, and how it was reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub provider: String,
    pub model: String,
    pub how: &'static str,
}

/// The provider a model name belongs to, in the sources' naming. Names that no vendor prefix
/// explains (an aggregator's own models) have none.
pub(crate) fn upstream_provider(model_id: &str) -> Option<&'static str> {
    let lower = model_id.trim().to_ascii_lowercase();
    let table: [(&str, &str); 12] = [
        ("claude", "anthropic"),
        ("gpt-", "openai"),
        ("gemini", "google"),
        ("glm", "z-ai"),
        ("deepseek", "deepseek"),
        ("kimi", "moonshotai"),
        ("mimo", "xiaomi"),
        ("qwen", "qwen"),
        ("minimax", "minimax"),
        ("muse", "meta"),
        ("solar", "upstage"),
        ("grok", "x-ai"),
    ];
    table
        .iter()
        .find(|(prefix, _)| lower.starts_with(prefix))
        .map(|(_, provider)| *provider)
}

/// The names one provider goes by across the sources (LLMBenchmark.ai and models.dev differ).
pub(crate) fn provider_names(provider: &str) -> Vec<String> {
    match provider {
        "z-ai" => vec!["z-ai", "zai", "zhipuai"],
        "qwen" => vec!["qwen", "alibaba"],
        other => vec![other],
    }
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// `mimo-2.6-flash` -> `mimo-v2.6-flash`: a `v` before the first number that follows a `-`.
fn with_v(id: &str) -> Option<String> {
    let at = id
        .match_indices('-')
        .map(|(i, _)| i)
        .find(|i| id.as_bytes().get(i + 1).is_some_and(u8::is_ascii_digit))?;
    Some(format!("{}v{}", &id[..=at], &id[at + 1..]))
}

/// `qwen-3.8-flash` -> `qwen3.8-flash`: no `-` before the first number that follows one.
fn tight_version(id: &str) -> Option<String> {
    let at = id
        .match_indices('-')
        .map(|(i, _)| i)
        .find(|i| id.as_bytes().get(i + 1).is_some_and(u8::is_ascii_digit))?;
    Some(format!("{}{}", &id[..at], &id[at + 1..]))
}

fn without_suffix(id: &str) -> Option<String> {
    ["-instruct", "-latest"]
        .iter()
        .find_map(|suffix| id.strip_suffix(suffix))
        .map(str::to_string)
}

fn is_date_suffix(rest: &str) -> bool {
    let digits = rest.strip_prefix('-').unwrap_or("");
    (4..=8).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
}

/// Looks `model` up. `ids_of(provider)` gives the model ids a source lists for a provider (its
/// own naming, without the `provider/` part).
pub(crate) fn resolve(
    model: &ModelRef,
    ids_of: &dyn Fn(&str) -> Vec<String>,
) -> Option<Resolved> {
    let direct = source_provider(&model.provider);
    let wanted = normalize_model(&model.model_id);
    let ids = ids_of(&direct);
    if let Some(found) = ids.iter().find(|id| **id == model.model_id) {
        return Some(Resolved {
            provider: direct,
            model: found.clone(),
            how: "exact",
        });
    }
    if let Some(found) = ids.iter().find(|id| normalize_model(id) == wanted) {
        return Some(Resolved {
            provider: direct,
            model: found.clone(),
            how: "normalized_id",
        });
    }

    // The provider the name belongs to: from a `provider/` part, else from the name itself.
    let (prefix, base) = split_provider(&model.model_id);
    let upstream = prefix
        .map(source_provider)
        .or_else(|| upstream_provider(base).map(str::to_string))?;
    let mut variants: Vec<(String, &'static str)> = vec![(base.to_string(), "inferred_provider")];
    if let Some(v) = with_v(base) {
        variants.push((v, "inferred_provider_alias"));
    }
    if let Some(tight) = tight_version(base) {
        variants.push((tight, "inferred_provider_alias"));
    }
    if let Some(stripped) = without_suffix(base) {
        if let Some(v) = with_v(&stripped) {
            variants.push((v, "inferred_provider_alias"));
        }
        if let Some(tight) = tight_version(&stripped) {
            variants.push((tight, "inferred_provider_alias"));
        }
        variants.push((stripped, "inferred_provider_alias"));
    }
    for name in provider_names(&upstream) {
        let ids = ids_of(&name);
        if ids.is_empty() {
            continue;
        }
        for (variant, how) in &variants {
            let wanted = normalize_model(variant);
            if let Some(found) = ids.iter().find(|id| normalize_model(id) == wanted) {
                return Some(Resolved {
                    provider: name,
                    model: found.clone(),
                    how,
                });
            }
        }
        for (variant, _) in &variants {
            let wanted = normalize_model(variant);
            let dated: Vec<&String> = ids
                .iter()
                .filter(|id| {
                    let id = normalize_model(id);
                    id.strip_prefix(&wanted).is_some_and(is_date_suffix)
                })
                .collect();
            if let [only] = dated.as_slice() {
                return Some(Resolved {
                    provider: name,
                    model: (*only).clone(),
                    how: "inferred_provider_dated",
                });
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(provider: &str, id: &str) -> ModelRef {
        ModelRef {
            provider: provider.to_string(),
            model_id: id.to_string(),
            auth_kind: None,
        }
    }

    fn table(entries: &[(&str, &[&str])]) -> impl Fn(&str) -> Vec<String> {
        let owned: Vec<(String, Vec<String>)> = entries
            .iter()
            .map(|(p, ids)| (p.to_string(), ids.iter().map(|i| i.to_string()).collect()))
            .collect();
        move |provider: &str| {
            owned
                .iter()
                .find(|(p, _)| p == provider)
                .map(|(_, ids)| ids.clone())
                .unwrap_or_default()
        }
    }

    #[test]
    fn the_provider_as_given_wins_and_keeps_its_tags() {
        let ids = table(&[("openai", &["gpt-5.4-mini", "gpt-5"])]);
        let found = resolve(&model("openai", "gpt-5.4-mini"), &ids).expect("exact");
        assert_eq!((found.model.as_str(), found.how), ("gpt-5.4-mini", "exact"));
        let found = resolve(&model("openai", "GPT-5-4-mini"), &ids).expect("normalized");
        assert_eq!(found.how, "normalized_id");
    }

    #[test]
    fn a_gateways_model_is_found_under_the_provider_its_name_belongs_to() {
        let ids = table(&[
            ("anthropic", &["claude-fable-5.2", "claude-opus-5.5"]),
            ("xiaomi", &["mimo-v2.6-flash", "mimo-v2.6-pro"]),
            ("deepseek", &["deepseek-v4.1-flash-1017", "deepseek-v4-pro"]),
            ("z-ai", &["glm-5"]),
            ("qwen", &["qwen3.8-flash"]),
            ("zai", &["glm-5.3", "glm-5.3-flash"]),
        ]);
        let by = |provider: &str, id: &str| resolve(&model(provider, id), &ids);
        let a = by("deepai", "claude-fable-5-2").expect("claude");
        assert_eq!((a.provider.as_str(), a.model.as_str(), a.how), ("anthropic", "claude-fable-5.2", "inferred_provider"));
        let m = by("deepai", "mimo-2.6-flash").expect("mimo");
        assert_eq!((m.model.as_str(), m.how), ("mimo-v2.6-flash", "inferred_provider_alias"));
        let d = by("deepai", "deepseek-v4.1-flash").expect("deepseek");
        assert_eq!((d.model.as_str(), d.how), ("deepseek-v4.1-flash-1017", "inferred_provider_dated"));
        let g = by("deepai", "glm-5.3-flash").expect("glm");
        assert_eq!((g.provider.as_str(), g.model.as_str()), ("zai", "glm-5.3-flash"));
        let q = by("deepai", "qwen-3.8-flash").expect("qwen");
        assert_eq!((q.model.as_str(), q.how), ("qwen3.8-flash", "inferred_provider_alias"));
        // A gateway's `provider/model` id names the provider itself.
        let o = by("openscale", "deepseek/deepseek-v4.1-flash").expect("prefixed");
        assert_eq!(o.model, "deepseek-v4.1-flash-1017");
    }

    #[test]
    fn no_nearest_name_is_guessed_and_an_ambiguous_date_is_not_picked() {
        let ids = table(&[
            ("deepseek", &["deepseek-v4-flash-0731", "deepseek-v4-flash-0912"]),
            ("openai", &["gpt-4.1"]),
            ("upstage", &["solar-pro4"]),
        ]);
        assert_eq!(resolve(&model("deepai", "deepseek-v4-flash"), &ids), None, "two dated entries");
        assert_eq!(resolve(&model("deepai", "gpt-6-luna"), &ids), None, "a different model");
        assert_eq!(resolve(&model("deepai", "solar-mini-4"), &ids), None);
        assert_eq!(resolve(&model("deepai", "glyph-cluster"), &ids), None, "no vendor prefix");
        assert_eq!(resolve(&model("openai", "gpt-9-unknown"), &ids), None);
    }
}
