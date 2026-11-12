//! The model catalog of the DeepAI provider.
//!
//! The list is read live from `GET {base}/models` (no sign-in needed) and checked by
//! `codex-inocode-deepai`; a catalog that fails the checks is not used. There is no bundled list:
//! when nothing can be read, the one minimal entry from that crate keeps the first screen from
//! being empty. The first model is the catalog's `default`. Which models are free, and how many
//! Theos a request costs, is carried by the same catalog (`isFree`, `theosPerRequest` in
//! `inocode/models/enrich`).

use codex_http_client::HttpClientFactory;
use codex_inocode_deepai::catalog;
use codex_inocode_deepai::catalog::Catalog;
use codex_inocode_deepai::catalog::CatalogModel as DeepAiModel;
use codex_login::AuthManager;
use codex_models_manager::CatalogModel;
use codex_models_manager::ModelOrigin;
use codex_models_manager::ModelsManagerConfig;
use codex_models_manager::manager::ModelsManager;
use codex_models_manager::manager::ModelsManagerFuture;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::manager::StaticModelsManager;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::openai_models::ApplyPatchToolType;
use codex_protocol::openai_models::InputModality;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use std::sync::Arc;
use std::sync::RwLock;
use tokio::sync::Mutex;
use tokio::sync::TryLockError;

/// How long a successful read is used before the next one.
const TTL_SECS: i64 = 3600;
/// After a failed read, wait this long before trying again.
const RETRY_AFTER_SECS: i64 = 60;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn effort(level: ReasoningEffort, description: &str) -> ReasoningEffortPreset {
    ReasoningEffortPreset {
        effort: level,
        description: description.to_string(),
    }
}

/// The catalog entry of one model. A model called in the Responses format keeps the generic
/// entry's tool settings; a Chat Completions model takes function tools only (`apply_patch` is
/// carried as a function with one text input; the deferred-tool search as a function).
pub(crate) fn model_info(model: &DeepAiModel, priority: i32, is_default: bool) -> ModelInfo {
    static TEMPLATE: std::sync::OnceLock<ModelInfo> = std::sync::OnceLock::new();
    let template = TEMPLATE.get_or_init(|| {
        codex_models_manager::model_info::model_info_from_slug("deepai-template")
    });
    let mut info = template.clone();
    info.slug = model.id.clone();
    info.display_name = model.name.clone();
    info.description = None;
    info.supported_reasoning_levels = if model.reasoning {
        // The same ladder Codex offers (low .. max, default medium). What DeepAI accepts is
        // narrower (none, low, medium, high): the request clamps (`deepai_client`).
        vec![
            effort(ReasoningEffort::Low, "Fast, light reasoning"),
            effort(ReasoningEffort::Medium, "Balanced reasoning"),
            effort(ReasoningEffort::High, "Deeper reasoning"),
            effort(ReasoningEffort::XHigh, "Extra-deep reasoning"),
            effort(ReasoningEffort::Max, "Maximum reasoning"),
        ]
    } else {
        Vec::new()
    };
    info.default_reasoning_level = model.reasoning.then_some(ReasoningEffort::Medium);
    info.visibility = ModelVisibility::List;
    info.supported_in_api = true;
    // The catalog's default goes first in the picker.
    info.priority = if is_default { -1 } else { priority };
    info.supports_reasoning_summary_parameter = false;
    info.support_verbosity = false;
    info.default_verbosity = None;
    if !model.uses_responses() {
        info.apply_patch_tool_type = Some(ApplyPatchToolType::Freeform);
        info.supports_search_tool = true;
    }
    info.context_window = Some(model.context_window);
    info.max_context_window = info.context_window;
    info.input_modalities = if model.takes_images() {
        vec![InputModality::Text, InputModality::Image]
    } else {
        vec![InputModality::Text]
    };
    info
}

fn models_of(catalog: &Catalog) -> Vec<ModelInfo> {
    let default = catalog.default_model().map(|m| m.id.clone());
    catalog
        .models
        .iter()
        .enumerate()
        .map(|(index, model)| {
            model_info(
                model,
                i32::try_from(index).unwrap_or(i32::MAX),
                default.as_deref() == Some(model.id.as_str()),
            )
        })
        .collect()
}

fn static_manager(catalog: &Catalog) -> Arc<StaticModelsManager> {
    Arc::new(StaticModelsManager::new(
        /*auth_manager*/ None,
        ModelsResponse {
            models: models_of(catalog),
        },
        ModelOrigin::Provider,
    ))
}

struct State {
    inner: Arc<StaticModelsManager>,
    fetched_at: Option<i64>,
    last_attempt: Option<i64>,
    from_remote: bool,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State")
            .field("fetched_at", &self.fetched_at)
            .field("from_remote", &self.from_remote)
            .finish()
    }
}

/// The DeepAI provider's catalog manager.
#[derive(Debug)]
pub(crate) struct DeepAiModelsManager {
    base_url: String,
    state: RwLock<State>,
    fetch_lock: Mutex<()>,
}

impl DeepAiModelsManager {
    pub(crate) fn new(base_url: String) -> Self {
        let initial = catalog::current().unwrap_or_else(catalog::fallback);
        let from_remote = catalog::current().is_some();
        Self {
            base_url,
            state: RwLock::new(State {
                inner: static_manager(&initial),
                fetched_at: None,
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

    async fn refresh(&self, force: bool) {
        let _guard = self.fetch_lock.lock().await;
        let at = now();
        {
            let state = self
                .state
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let fresh = state.fetched_at.is_some_and(|t| at - t < TTL_SECS);
            let tried_lately = state.last_attempt.is_some_and(|t| at - t < RETRY_AFTER_SECS);
            if (!force && fresh) || (!force && tried_lately) {
                return;
            }
        }
        self.state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_attempt = Some(at);
        match catalog::fetch(&self.base_url).await {
            Ok(read) => {
                tracing::info!("deepai model list read: {} models", read.models.len());
                catalog::remember(&read);
                let mut state = self
                    .state
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.inner = static_manager(&read);
                state.fetched_at = Some(at);
                state.from_remote = true;
            }
            Err(error) => tracing::debug!("deepai model list read failed: {error}"),
        }
    }
}

impl ModelsManager for DeepAiModelsManager {
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
        Box::pin(async move {
            let inner = self.current();
            let known = inner.get_remote_models().await;
            if known.iter().any(|m| m.slug == model) {
                return inner.get_model_info(model, config).await;
            }
            // A name the list does not have still gets the DeepAI defaults (Chat Completions
            // tools), never the OpenAI-shaped fallback that sends every tool.
            let info = model_info(
                &DeepAiModel {
                    id: model.to_string(),
                    name: model.to_string(),
                    reasoning: false,
                    input: vec!["text".to_string()],
                    context_window: 128_000,
                    max_tokens: 16_000,
                    premier: Some(true),
                    api: None,
                    theos_per_request: None,
                    theos_per_second: None,
                },
                i32::MAX,
                false,
            );
            StaticModelsManager::new(
                /*auth_manager*/ None,
                ModelsResponse { models: vec![info] },
                self.origin(),
            )
            .get_model_info(model, config)
            .await
        })
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
mod tests {
    use super::*;
    use codex_inocode_deepai::catalog::parse;

    fn sample() -> Catalog {
        parse(
            r#"{"models":[
              {"id":"a","name":"A","reasoning":true,"input":["text","image"],"contextWindow":1000,"maxTokens":100,"theosPerRequest":"0"},
              {"id":"b","name":"B","reasoning":false,"input":["text"],"contextWindow":2000,"maxTokens":200,"premier":true,"api":"openai-responses","theosPerRequest":"50"}
            ],"default":"b"}"#,
        )
        .expect("valid")
    }

    #[test]
    fn the_default_model_goes_first_and_the_data_follows_the_catalog() {
        let infos = models_of(&sample());
        assert_eq!(infos.len(), 2);
        let a = &infos[0];
        let b = &infos[1];
        assert!(b.priority < a.priority, "the default is listed first");
        assert_eq!(a.context_window, Some(1000));
        assert_eq!(a.input_modalities, vec![InputModality::Text, InputModality::Image]);
        let ladder: Vec<ReasoningEffort> = a.supported_reasoning_levels.iter().map(|p| p.effort.clone()).collect();
        assert_eq!(
            ladder,
            [ReasoningEffort::Low, ReasoningEffort::Medium, ReasoningEffort::High, ReasoningEffort::XHigh, ReasoningEffort::Max]
        );
        assert_eq!(a.default_reasoning_level, Some(ReasoningEffort::Medium));
        assert!(b.supported_reasoning_levels.is_empty());
        // Chat Completions models carry the freeform and search tools as functions; a
        // Responses-format model keeps the generic settings.
        assert_eq!(a.apply_patch_tool_type, Some(ApplyPatchToolType::Freeform));
        assert!(a.supports_search_tool);
        assert!(!b.supports_search_tool);
    }

    #[tokio::test]
    async fn an_unlisted_name_still_gets_the_deepai_defaults() {
        let manager = DeepAiModelsManager::new("http://127.0.0.1:1".to_string());
        let config = ModelsManagerConfig::default();
        let found = manager.get_model_info("not-in-the-list", &config).await;
        assert_eq!(found.model.slug, "not-in-the-list");
        assert!(found.model.supports_search_tool);
    }
}
