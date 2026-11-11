//! The model catalog of the Claude provider (`wire_api = "anthropic_messages"`).
//!
//! The list is read live from `GET /v1/models` with the account pool's Claude credential (OAuth
//! or API key), kept in `models_cache_claude.json` for an hour, and falls back to a bundled
//! snapshot when nothing can be read. Nothing is filtered out: every model the endpoint returns
//! is listed. A new model appears as soon as the endpoint returns it; only the snapshot names
//! models.
//!
//! The snapshot is ported from OpenCodex `providers/registry/model-seeds.ts` (the Anthropic ids,
//! context windows, output ceilings and the effort ladder).

use codex_login::account_pool;
use codex_login::claude_oauth::RemoteModel;
use codex_models_manager::CatalogModel;
use codex_models_manager::ModelOrigin;
use codex_models_manager::ModelsManagerConfig;
use codex_models_manager::manager::ModelsManager;
use codex_models_manager::manager::ModelsManagerFuture;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::manager::StaticModelsManager;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::openai_models::InputModality;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_http_client::HttpClientFactory;
use codex_login::AuthManager;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use tokio::sync::Mutex;
use tokio::sync::TryLockError;

const CACHE_FILE: &str = "models_cache_claude.json";
/// How long a successful read is used before the next one.
const CACHE_TTL_SECS: i64 = 3600;
/// After a failed read, wait this long before trying again.
const RETRY_AFTER_SECS: i64 = 60;
const CLAUDE_GROUP: &str = "claude";

/// The ids the bundled snapshot names: `(id, display name, context window, max output tokens)`.
const SNAPSHOT: [(&str, &str, i64, i64); 12] = [
    ("claude-fable-5-1", "Claude Fable 5.1", 1_000_000, 128_000),
    ("claude-fable-5", "Claude Fable 5", 1_000_000, 128_000),
    ("claude-sonnet-5-5", "Claude Sonnet 5.5", 1_000_000, 128_000),
    ("claude-sonnet-5", "Claude Sonnet 5", 1_000_000, 128_000),
    ("claude-opus-5-5", "Claude Opus 5.5", 1_000_000, 128_000),
    ("claude-opus-5", "Claude Opus 5", 1_000_000, 128_000),
    ("claude-opus-4-8", "Claude Opus 4.8", 1_000_000, 128_000),
    ("claude-opus-4-7", "Claude Opus 4.7", 1_000_000, 128_000),
    ("claude-opus-4-6", "Claude Opus 4.6", 1_000_000, 128_000),
    ("claude-sonnet-4-6", "Claude Sonnet 4.6", 1_000_000, 128_000),
    ("claude-haiku-5-5", "Claude Haiku 5.5", 1_000_000, 128_000),
    ("claude-haiku-4-5", "Claude Haiku 4.5", 200_000, 64_000),
];

/// The bundled list, used until (or unless) the endpoint answers.
pub(crate) fn snapshot() -> Vec<RemoteModel> {
    SNAPSHOT
        .iter()
        .map(|(id, name, context, output)| RemoteModel {
            id: (*id).to_string(),
            display_name: Some((*name).to_string()),
            created_at: None,
            max_input_tokens: Some(*context),
            max_tokens: Some(*output),
            image_input: Some(true),
            thinking: Some(true),
        })
        .collect()
}

/// `claude-opus-5-5` -> `Claude Opus 5.5`, for a model that names no display name.
fn display_name_of(id: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut digits: Vec<&str> = Vec::new();
    for part in id.split('-') {
        if part.chars().all(|c| c.is_ascii_digit()) && !part.is_empty() && part.len() <= 2 {
            digits.push(part);
        } else {
            if !digits.is_empty() {
                parts.push(digits.join("."));
                digits.clear();
            }
            let mut chars = part.chars();
            let word = match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            };
            parts.push(word);
        }
    }
    if !digits.is_empty() {
        parts.push(digits.join("."));
    }
    parts.join(" ")
}

fn effort(level: ReasoningEffort, description: &str) -> ReasoningEffortPreset {
    ReasoningEffortPreset {
        effort: level,
        description: description.to_string(),
    }
}

/// The model's catalog entry. Claude models take function tools only (no `apply_patch` freeform
/// tool, no hosted web search), accept the effort ladder, and do not take the Responses
/// `reasoning.summary` parameter.
pub(crate) fn model_info(remote: &RemoteModel, priority: i32) -> ModelInfo {
    static TEMPLATE: std::sync::OnceLock<ModelInfo> = std::sync::OnceLock::new();
    let template = TEMPLATE.get_or_init(|| {
        codex_models_manager::model_info::model_info_from_slug("claude-template")
    });
    let mut info = template.clone();
    info.slug = remote.id.clone();
    info.display_name = remote
        .display_name
        .clone()
        .unwrap_or_else(|| display_name_of(&remote.id));
    info.description = None;
    info.supported_reasoning_levels = vec![
        effort(ReasoningEffort::Low, "Fast, light reasoning"),
        effort(ReasoningEffort::Medium, "Balanced reasoning"),
        effort(ReasoningEffort::High, "Deeper reasoning"),
        effort(ReasoningEffort::XHigh, "Extra-deep reasoning"),
        effort(ReasoningEffort::Max, "Maximum reasoning"),
    ];
    info.default_reasoning_level = Some(ReasoningEffort::Medium);
    info.visibility = ModelVisibility::List;
    info.supported_in_api = true;
    info.priority = priority;
    info.supports_reasoning_summary_parameter = false;
    info.support_verbosity = false;
    info.default_verbosity = None;
    info.apply_patch_tool_type = None;
    info.supports_search_tool = false;
    info.context_window = remote.max_input_tokens.or(info.context_window);
    info.max_context_window = info.context_window;
    info.input_modalities = if remote.image_input == Some(false) {
        vec![InputModality::Text]
    } else {
        vec![InputModality::Text, InputModality::Image]
    };
    info
}

fn models_of(remote: &[RemoteModel]) -> Vec<ModelInfo> {
    remote
        .iter()
        .enumerate()
        .map(|(index, model)| model_info(model, i32::try_from(index).unwrap_or(i32::MAX)))
        .collect()
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    fetched_at: i64,
    models: Vec<RemoteModel>,
}

struct State {
    inner: Arc<StaticModelsManager>,
    /// Unix seconds of the last successful read (or of the cache it came from).
    fetched_at: Option<i64>,
    last_attempt: Option<i64>,
    from_remote: bool,
}

/// The Claude provider's catalog manager.
#[derive(Debug)]
pub(crate) struct ClaudeModelsManager {
    base_url: String,
    cache_path: Option<PathBuf>,
    state: RwLock<State>,
    fetch_lock: Mutex<()>,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State")
            .field("fetched_at", &self.fetched_at)
            .field("from_remote", &self.from_remote)
            .finish()
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn static_manager(remote: &[RemoteModel]) -> Arc<StaticModelsManager> {
    Arc::new(StaticModelsManager::new(
        /*auth_manager*/ None,
        ModelsResponse {
            models: models_of(remote),
        },
        ModelOrigin::Provider,
    ))
}

impl ClaudeModelsManager {
    /// `codex_home` places the cache file; `None` keeps everything in memory.
    pub(crate) fn new(base_url: String, codex_home: Option<PathBuf>) -> Self {
        let cache_path = codex_home.map(|home| home.join(CACHE_FILE));
        let mut remote = snapshot();
        let mut fetched_at = None;
        let mut from_remote = false;
        if let Some(cached) = cache_path.as_deref().and_then(read_cache)
            && !cached.models.is_empty()
        {
            remote = cached.models;
            fetched_at = Some(cached.fetched_at);
            from_remote = true;
        }
        Self {
            base_url,
            cache_path,
            state: RwLock::new(State {
                inner: static_manager(&remote),
                fetched_at,
                last_attempt: None,
                from_remote,
            }),
            fetch_lock: Mutex::new(()),
        }
    }

    fn current(&self) -> Arc<StaticModelsManager> {
        Arc::clone(
            &self
                .state
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .inner,
        )
    }

    fn origin(&self) -> ModelOrigin {
        if self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .from_remote
        {
            ModelOrigin::Provider
        } else {
            ModelOrigin::Bundled
        }
    }

    /// Reads the list again when `force`, or when the last read is older than the cache's
    /// lifetime. A failed read keeps what is there (the cache or the snapshot).
    async fn refresh(&self, force: bool) {
        let _guard = self.fetch_lock.lock().await;
        let at = now();
        {
            let state = self
                .state
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let fresh = state.fetched_at.is_some_and(|t| at - t < CACHE_TTL_SECS);
            let tried_lately = state.last_attempt.is_some_and(|t| at - t < RETRY_AFTER_SECS);
            if (!force && fresh) || (!force && tried_lately) {
                return;
            }
        }
        let Some(pool) = account_pool::global_for(CLAUDE_GROUP) else {
            return;
        };
        {
            self.state
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .last_attempt = Some(at);
        }
        match pool.fetch_claude_models(&self.base_url).await {
            Ok(models) if !models.is_empty() => {
                tracing::info!("claude model list read: {} models", models.len());
                if let Some(path) = &self.cache_path {
                    write_cache(path, at, &models);
                }
                let mut state = self
                    .state
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.inner = static_manager(&models);
                state.fetched_at = Some(at);
                state.from_remote = true;
            }
            Ok(_) => tracing::debug!("claude model list was empty; keeping the current one"),
            Err(error) => tracing::debug!("claude model list read failed: {error}"),
        }
    }
}

fn read_cache(path: &std::path::Path) -> Option<CacheFile> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_cache(path: &std::path::Path, fetched_at: i64, models: &[RemoteModel]) {
    let file = CacheFile {
        fetched_at,
        models: models.to_vec(),
    };
    if let Ok(text) = serde_json::to_string(&file) {
        let temp = path.with_extension("json.tmp");
        if std::fs::write(&temp, text).is_ok() {
            let _ = std::fs::rename(&temp, path);
        }
    }
}

impl ModelsManager for ClaudeModelsManager {
    fn ensure_initialized(
        &self,
        _http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ()> {
        Box::pin(async move { self.refresh(/*force*/ false).await })
    }

    fn model_catalog_with_origins(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, Vec<CatalogModel>> {
        Box::pin(async move {
            match refresh_strategy {
                RefreshStrategy::Online => self.refresh(/*force*/ true).await,
                RefreshStrategy::OnlineIfUncached => self.refresh(/*force*/ false).await,
                RefreshStrategy::InMemory | RefreshStrategy::Offline => {}
            }
            let origin = self.origin();
            self.current()
                .model_catalog_with_origins(RefreshStrategy::InMemory, http_client_factory)
                .await
                .into_iter()
                .map(|entry| CatalogModel {
                    model: entry.model,
                    origin,
                })
                .collect()
        })
    }

    fn refresh_after_auth_change(
        &self,
        _http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ()> {
        Box::pin(async move { self.refresh(/*force*/ true).await })
    }

    fn get_remote_models(&self) -> ModelsManagerFuture<'_, Vec<ModelInfo>> {
        Box::pin(async move { self.current().get_remote_models().await })
    }

    fn try_get_remote_models(&self) -> Result<Vec<ModelInfo>, TryLockError> {
        self.current().try_get_remote_models()
    }

    fn auth_manager(&self) -> Option<&AuthManager> {
        None
    }

    fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        self.current().list_collaboration_modes()
    }

    fn get_model_info<'a>(
        &'a self,
        model: &'a str,
        config: &'a ModelsManagerConfig,
    ) -> ModelsManagerFuture<'a, CatalogModel> {
        Box::pin(async move { self.current().get_model_info(model, config).await })
    }

    fn refresh_if_new_etag(
        &self,
        _etag: String,
        _http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ()> {
        Box::pin(async {})
    }
}

#[cfg(test)]
#[path = "claude_models_tests.rs"]
mod tests;
