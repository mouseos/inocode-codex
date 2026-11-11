#![allow(clippy::expect_used)]

use super::ClaudeModelsManager;
use super::display_name_of;
use super::model_info;
use super::snapshot;
use codex_login::claude_oauth::RemoteModel;
use codex_models_manager::manager::ModelsManager;
use codex_protocol::openai_models::InputModality;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ReasoningEffort;
use pretty_assertions::assert_eq;

#[test]
fn a_model_without_a_display_name_gets_one_from_its_id() {
    assert_eq!(display_name_of("claude-opus-5-5"), "Claude Opus 5.5");
    assert_eq!(display_name_of("claude-haiku-4-5"), "Claude Haiku 4.5");
    assert_eq!(display_name_of("claude-sonnet-5"), "Claude Sonnet 5");
}

#[test]
fn the_snapshot_lists_every_family_and_filters_nothing() {
    let models = snapshot();
    assert_eq!(models.len(), 12);
    for family in ["fable", "opus", "sonnet", "haiku"] {
        assert!(
            models.iter().any(|model| model.id.contains(family)),
            "{family} is listed"
        );
    }
    for model in &models {
        assert_eq!(model_info(model, 0).visibility, ModelVisibility::List);
    }
}

#[test]
fn a_catalog_entry_is_built_from_what_the_endpoint_says() {
    let remote = RemoteModel {
        id: "claude-brand-new-9".to_string(),
        display_name: Some("Claude Brand New 9".to_string()),
        created_at: None,
        max_input_tokens: Some(2_000_000),
        max_tokens: Some(64_000),
        image_input: Some(false),
        thinking: Some(true),
    };
    let info = model_info(&remote, 3);
    assert_eq!(info.slug, "claude-brand-new-9");
    assert_eq!(info.display_name, "Claude Brand New 9");
    assert_eq!(info.priority, 3);
    assert_eq!(info.context_window, Some(2_000_000));
    assert_eq!(info.input_modalities, vec![InputModality::Text]);
    assert_eq!(info.default_reasoning_level, Some(ReasoningEffort::Medium));
    assert_eq!(info.supported_reasoning_levels.len(), 5);
    assert_eq!(info.apply_patch_tool_type, None, "function tools only");
    assert!(!info.supports_search_tool);
    assert!(!info.supports_reasoning_summary_parameter);

    let bare = RemoteModel {
        id: "claude-x".to_string(),
        display_name: None,
        created_at: None,
        max_input_tokens: None,
        max_tokens: None,
        image_input: None,
        thinking: None,
    };
    let info = model_info(&bare, 0);
    assert_eq!(info.display_name, "Claude X");
    assert_eq!(
        info.input_modalities,
        vec![InputModality::Text, InputModality::Image]
    );
}

#[test]
fn a_manager_starts_from_the_snapshot_and_then_from_a_fresh_cache() {
    let home = tempfile::tempdir().expect("tempdir");
    let manager =
        ClaudeModelsManager::new("https://api.anthropic.com".to_string(), Some(home.path().into()));
    assert_eq!(manager.try_get_remote_models().expect("models").len(), 12);

    let cached = vec![RemoteModel {
        id: "claude-cached-1".to_string(),
        display_name: None,
        created_at: None,
        max_input_tokens: None,
        max_tokens: None,
        image_input: None,
        thinking: None,
    }];
    super::write_cache(
        &home.path().join("models_cache_claude.json"),
        chrono::Utc::now().timestamp(),
        &cached,
    );
    let again =
        ClaudeModelsManager::new("https://api.anthropic.com".to_string(), Some(home.path().into()));
    let slugs: Vec<String> = again
        .try_get_remote_models()
        .expect("models")
        .into_iter()
        .map(|model| model.slug)
        .collect();
    assert_eq!(slugs, vec!["claude-cached-1".to_string()]);
}
