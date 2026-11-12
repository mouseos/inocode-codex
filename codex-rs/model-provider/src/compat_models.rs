//! The model list of an OpenAI-compatible provider you configure yourself
//! (`[model_providers.<id>]` with a `base_url` and no `model_catalog_url`).
//!
//! The provider's own `GET {base_url}/models` (the public `{"data": [{"id": ...}]}` shape) is
//! read with the provider's key and listed as it is. Nothing is filtered or renamed. If the list
//! cannot be read (no key, no connection, an error status, an empty answer), the catalog that the
//! provider had before this existed is used, so nothing that worked stops working. The key is read
//! from the provider's own settings for the request and is never stored, logged or returned.

use codex_http_client::HttpClientFactory;
use codex_login::AuthManager;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_models_manager::CatalogModel;
use codex_models_manager::ModelOrigin;
use codex_models_manager::ModelsManagerConfig;
use codex_models_manager::manager::ModelsManager;
use codex_models_manager::manager::ModelsManagerFuture;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::manager::SharedModelsManager;
use codex_models_manager::manager::StaticModelsManager;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::openai_models::ApplyPatchToolType;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use serde_json::Value;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::TryLockError;

const TTL_SECS: i64 = 3600;
const RETRY_AFTER_SECS: i64 = 60;
const TIMEOUT: Duration = Duration::from_secs(8);

/// A provider this manager is for: it has its own base URL, is not one of the built-in kinds and
/// brings no catalog URL of its own.
pub fn is_custom_provider(info: &ModelProviderInfo) -> bool {
    applies_to(info)
}

pub(crate) fn applies_to(info: &ModelProviderInfo) -> bool {
    info.base_url.is_some()
        && info.model_catalog_url.is_none()
        && !info.is_openai()
        && !info.is_amazon_bedrock()
        && !info.is_amazon_bedrock_runtime()
        && !info.is_gcp()
        && info.aws.is_none()
        && info.wire_api != WireApi::AnthropicMessages
        && !info
            .base_url
            .as_deref()
            .is_some_and(codex_inocode_deepai::is_deepai_url)
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The key the provider's own settings give, read now: the literal token, else the variable.
fn key_of(info: &ModelProviderInfo) -> Option<String> {
    info.experimental_bearer_token
        .as_deref()
        .cloned()
        .filter(|token| !token.trim().is_empty())
        .or_else(|| {
            info.env_key
                .as_deref()
                .and_then(|name| std::env::var(name).ok())
                .filter(|value| !value.trim().is_empty())
        })
}

/// One listed model: id, and a context size when the provider says one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Listed {
    pub id: String,
    pub name: Option<String>,
    pub context: Option<i64>,
    /// `Some(false)` only when the provider says outright that the model does not reason.
    pub reasoning: Option<bool>,
}

/// Reads the public `/models` answer. Entries without a string `id` are skipped.
pub(crate) fn parse_list(body: &Value) -> Vec<Listed> {
    let items = body
        .get("data")
        .or_else(|| body.get("models"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    items
        .iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.trim().to_string();
            if id.is_empty() || !seen.insert(id.clone()) {
                return None;
            }
            let context = ["context_length", "context_window", "max_model_len", "max_context_length"]
                .iter()
                .find_map(|key| item.get(*key).and_then(Value::as_i64))
                .filter(|n| *n > 0);
            let name = ["name", "display_name"]
                .iter()
                .find_map(|key| item.get(*key).and_then(Value::as_str))
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_string);
            // Only an explicit "no" counts: a model whose answer says nothing is offered the
            // standard levels (a server that does not know the setting ignores it).
            let reasoning = ["reasoning", "supports_reasoning", "reasoning_supported"]
                .iter()
                .find_map(|key| item.get(*key).and_then(Value::as_bool))
                .or_else(|| {
                    item.get("capabilities")
                        .and_then(|c| c.get("reasoning"))
                        .and_then(Value::as_bool)
                });
            Some(Listed {
                id,
                name,
                context,
                reasoning,
            })
        })
        .collect()
}

pub(crate) fn model_info(listed: &Listed, priority: i32, wire: WireApi) -> ModelInfo {
    static TEMPLATE: std::sync::OnceLock<ModelInfo> = std::sync::OnceLock::new();
    let template = TEMPLATE.get_or_init(|| {
        codex_models_manager::model_info::model_info_from_slug("compat-template")
    });
    let mut info = template.clone();
    info.slug = listed.id.clone();
    info.display_name = listed.name.clone().unwrap_or_else(|| listed.id.clone());
    info.description = None;
    info.visibility = ModelVisibility::List;
    info.supported_in_api = true;
    info.priority = priority;
    if listed.reasoning != Some(false) {
        let level = |effort: ReasoningEffort, description: &str| ReasoningEffortPreset {
            effort,
            description: description.to_string(),
        };
        // The ladder Codex offers (low .. max, default medium); the value is sent as chosen.
        info.supported_reasoning_levels = vec![
            level(ReasoningEffort::Low, "Fast, light reasoning"),
            level(ReasoningEffort::Medium, "Balanced reasoning"),
            level(ReasoningEffort::High, "Deeper reasoning"),
            level(ReasoningEffort::XHigh, "Extra-deep reasoning"),
            level(ReasoningEffort::Max, "Maximum reasoning"),
        ];
        info.default_reasoning_level = Some(ReasoningEffort::Medium);
    } else {
        info.supported_reasoning_levels = Vec::new();
        info.default_reasoning_level = None;
    }
    if let Some(context) = listed.context {
        info.context_window = Some(context);
        info.max_context_window = info.context_window;
    }
    // Function tools only: the freeform tool and the deferred-tool search go as functions
    // (the chat wire's adapter, and for Responses the request bridge in `codex-api`), so the
    // third-party tools are loaded on demand instead of being declared on every request.
    info.apply_patch_tool_type = Some(ApplyPatchToolType::Freeform);
    info.supports_search_tool = true;
    if wire == WireApi::ChatCompletions {
        info.supports_reasoning_summary_parameter = false;
    }
    info
}

struct State {
    remote: Option<Arc<StaticModelsManager>>,
    fetched_at: Option<i64>,
    last_attempt: Option<i64>,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State")
            .field("fetched_at", &self.fetched_at)
            .finish()
    }
}

#[derive(Debug)]
pub(crate) struct CompatModelsManager {
    info: ModelProviderInfo,
    /// What served this provider before: used until, and unless, the list is read.
    fallback: SharedModelsManager,
    state: RwLock<State>,
    fetch_lock: Mutex<()>,
}

impl CompatModelsManager {
    pub(crate) fn new(info: ModelProviderInfo, fallback: SharedModelsManager) -> Self {
        Self {
            info,
            fallback,
            state: RwLock::new(State {
                remote: None,
                fetched_at: None,
                last_attempt: None,
            }),
            fetch_lock: Mutex::new(()),
        }
    }

    fn remote(&self) -> Option<Arc<StaticModelsManager>> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remote
            .clone()
    }

    async fn fetch(&self) -> Result<Vec<Listed>, String> {
        let base = self
            .info
            .base_url
            .as_deref()
            .ok_or_else(|| "no base URL".to_string())?
            .trim_end_matches('/')
            .to_string();
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "the client could not be built".to_string())?;
        let mut request = client.get(format!("{base}/models")).header("Accept", "application/json");
        if let Some(key) = key_of(&self.info) {
            request = request.header("Authorization", format!("Bearer {key}"));
        }
        for (name, value) in self.info.http_headers.iter().flatten() {
            request = request.header(name, value.as_str());
        }
        for (name, variable) in self.info.env_http_headers.iter().flatten() {
            if let Ok(value) = std::env::var(variable) {
                request = request.header(name, value);
            }
        }
        let response = request
            .send()
            .await
            .map_err(|_| "the model list could not be reached".to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("the model list answered HTTP {}", status.as_u16()));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|_| "the model list was not JSON".to_string())?;
        Ok(parse_list(&body))
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
            let tried = state.last_attempt.is_some_and(|t| at - t < RETRY_AFTER_SECS);
            if !force && (fresh || tried) {
                return;
            }
        }
        self.state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_attempt = Some(at);
        match self.fetch().await {
            Ok(listed) if !listed.is_empty() => {
                let infos: Vec<ModelInfo> = listed
                    .iter()
                    .enumerate()
                    .map(|(i, l)| model_info(l, i32::try_from(i).unwrap_or(i32::MAX), self.info.wire_api))
                    .collect();
                let manager = Arc::new(StaticModelsManager::new(
                    /*auth_manager*/ None,
                    ModelsResponse { models: infos },
                    ModelOrigin::Provider,
                ));
                let mut state = self
                    .state
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.remote = Some(manager);
                state.fetched_at = Some(at);
            }
            Ok(_) => tracing::debug!("the provider's model list was empty; keeping the current one"),
            Err(error) => tracing::debug!("the provider's model list was not read: {error}"),
        }
    }
}

impl ModelsManager for CompatModelsManager {
    fn ensure_initialized(&self, http_client_factory: HttpClientFactory) -> ModelsManagerFuture<'_, ()> {
        Box::pin(async move {
            self.refresh(/*force*/ false).await;
            if self.remote().is_none() {
                self.fallback.ensure_initialized(http_client_factory).await;
            }
        })
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
            match self.remote() {
                Some(remote) => {
                    remote
                        .model_catalog_with_origins(RefreshStrategy::InMemory, http_client_factory)
                        .await
                }
                None => {
                    self.fallback
                        .model_catalog_with_origins(refresh_strategy, http_client_factory)
                        .await
                }
            }
        })
    }

    fn refresh_after_auth_change(&self, http_client_factory: HttpClientFactory) -> ModelsManagerFuture<'_, ()> {
        Box::pin(async move {
            self.refresh(/*force*/ true).await;
            if self.remote().is_none() {
                self.fallback.refresh_after_auth_change(http_client_factory).await;
            }
        })
    }

    fn get_remote_models(&self) -> ModelsManagerFuture<'_, Vec<ModelInfo>> {
        Box::pin(async move {
            match self.remote() {
                Some(remote) => remote.get_remote_models().await,
                None => self.fallback.get_remote_models().await,
            }
        })
    }

    fn try_get_remote_models(&self) -> Result<Vec<ModelInfo>, TryLockError> {
        match self.remote() {
            Some(remote) => remote.try_get_remote_models(),
            None => self.fallback.try_get_remote_models(),
        }
    }

    fn auth_manager(&self) -> Option<&AuthManager> {
        self.fallback.auth_manager()
    }

    fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        self.fallback.list_collaboration_modes()
    }

    fn get_model_info<'a>(
        &'a self,
        model: &'a str,
        config: &'a ModelsManagerConfig,
    ) -> ModelsManagerFuture<'a, CatalogModel> {
        Box::pin(async move {
            if let Some(remote) = self.remote()
                && remote
                    .get_remote_models()
                    .await
                    .iter()
                    .any(|m| m.slug == model)
            {
                return remote.get_model_info(model, config).await;
            }
            self.fallback.get_model_info(model, config).await
        })
    }

    fn refresh_if_new_etag(&self, etag: String, http_client_factory: HttpClientFactory) -> ModelsManagerFuture<'_, ()> {
        self.fallback.refresh_if_new_etag(etag, http_client_factory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_model_provider_info::create_oss_provider_with_base_url;
    use serde_json::json;

    #[test]
    fn the_public_list_is_read_as_it_is() {
        let body = json!({"data": [
            {"id": "deepseek/deepseek-v4.1-flash", "context_length": 128000},
            {"id": "a", "name": "A model"},
            {"id": "a"},
            {"id": ""},
            {"object": "model"}
        ]});
        let listed = parse_list(&body);
        assert_eq!(listed.len(), 2, "duplicates and entries without an id are skipped");
        assert_eq!(listed[0].context, Some(128_000));
        assert_eq!(listed[1].name.as_deref(), Some("A model"));
        assert!(parse_list(&json!({"error": "x"})).is_empty());
    }

    #[test]
    fn it_applies_to_a_configured_provider_with_its_own_base_url_only() {
        let mut custom = create_oss_provider_with_base_url("https://api.example.test/v1", WireApi::Responses);
        assert!(applies_to(&custom));
        custom.model_catalog_url = Some("https://x.test/catalog".into());
        assert!(!applies_to(&custom), "a provider with its own catalog URL keeps it");
        let deepai = create_oss_provider_with_base_url(codex_model_provider_info::DEEPAI_BASE_URL, WireApi::ChatCompletions);
        assert!(!applies_to(&deepai));
        let claude = codex_model_provider_info::create_claude_provider();
        assert!(!applies_to(&claude));
        assert!(!applies_to(&ModelProviderInfo::create_openai_provider(None)));
    }

    #[test]
    fn the_key_comes_from_the_providers_own_settings() {
        let mut info = create_oss_provider_with_base_url("https://api.example.test/v1", WireApi::Responses);
        assert_eq!(key_of(&info), None);
        info.experimental_bearer_token = Some("tok".into());
        assert_eq!(key_of(&info).as_deref(), Some("tok"));
    }

    #[tokio::test]
    async fn the_list_is_read_with_the_key_and_replaces_the_fallback() {
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::header;
        use wiremock::matchers::method;
        use wiremock::matchers::path;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer secret-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"data": [{"id": "model-a"}, {"id": "model-b"}]}),
            ))
            .mount(&server)
            .await;
        let mut info = create_oss_provider_with_base_url(&format!("{}/v1", server.uri()), WireApi::Responses);
        info.experimental_bearer_token = Some("secret-token".into());
        let fallback: SharedModelsManager = Arc::new(StaticModelsManager::new(
            None,
            ModelsResponse { models: vec![] },
            ModelOrigin::Bundled,
        ));
        let manager = CompatModelsManager::new(info, fallback);
        manager.refresh(true).await;
        let models = manager.get_remote_models().await;
        let slugs: Vec<&str> = models.iter().map(|m| m.slug.as_str()).collect();
        assert_eq!(slugs, ["model-a", "model-b"]);
    }

    #[tokio::test]
    async fn a_list_that_cannot_be_read_leaves_the_fallback_in_place() {
        let info = create_oss_provider_with_base_url("http://127.0.0.1:1/v1", WireApi::Responses);
        let fallback: SharedModelsManager = Arc::new(StaticModelsManager::new(
            None,
            ModelsResponse { models: vec![model_info(&Listed { id: "kept".to_string(), name: None, context: None, reasoning: None }, 0, WireApi::Responses)] },
            ModelOrigin::Bundled,
        ));
        let manager = CompatModelsManager::new(info, fallback);
        manager.refresh(true).await;
        let models = manager.get_remote_models().await;
        assert_eq!(models.iter().map(|m| m.slug.as_str()).collect::<Vec<_>>(), ["kept"]);
    }

    #[test]
    fn a_listed_model_gets_the_standard_reasoning_levels_unless_it_says_it_does_not_reason() {
        let body = json!({"data": [
            {"id": "plain"},
            {"id": "yes", "reasoning": true},
            {"id": "no", "reasoning": false},
            {"id": "caps-no", "capabilities": {"reasoning": false}}
        ]});
        let infos: Vec<ModelInfo> = parse_list(&body)
            .iter()
            .map(|l| model_info(l, 0, WireApi::Responses))
            .collect();
        let levels = |slug: &str| {
            infos
                .iter()
                .find(|i| i.slug == slug)
                .map(|i| i.supported_reasoning_levels.len())
        };
        assert_eq!(levels("plain"), Some(5), "no statement: offered (low .. max)");
        assert_eq!(levels("yes"), Some(5));
        assert_eq!(levels("no"), Some(0));
        assert_eq!(levels("caps-no"), Some(0));
        let plain = infos.iter().find(|i| i.slug == "plain").expect("plain");
        assert_eq!(plain.default_reasoning_level, Some(ReasoningEffort::Medium));
    }
}
