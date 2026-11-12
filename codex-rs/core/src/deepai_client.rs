//! Calling DeepAI models (`deepai` provider, `https://api.deepai.org/deepai_code`).
//!
//! The key comes from the account pool's `deepai` group and travels as the `api-key` header,
//! next to the lane, instance, machine and conversation headers. Which wire a model uses is the
//! catalog's decision (`api: "openai-responses"` → Responses, otherwise Chat Completions). A busy
//! session (HTTP 409 `session_busy`) is waited out; a model that consumes Theos is called by one
//! request at a time. Errors are shown in Japanese with their code and HTTP status.

use codex_api::ApiError;
use codex_api::Provider;
use codex_api::ResponseEvent;
use codex_api::ResponseStream;
use codex_api::TransportError;
use codex_inocode_deepai as deepai;
use codex_login::account_pool;
use codex_protocol::error::CodexErr;
use codex_protocol::error::UnexpectedResponseError;
use http::HeaderValue;
use std::time::Duration;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::mpsc;

pub(crate) fn is_deepai(provider: &Provider) -> bool {
    deepai::is_deepai_url(&provider.base_url)
}

/// What one attempt needs: the provider with the DeepAI headers, the wire, and the turn-taking
/// slot for a metered model.
pub(crate) struct Attempt {
    pub(crate) provider: Provider,
    pub(crate) responses: bool,
    permit: Option<OwnedSemaphorePermit>,
}

/// The API key of the account in use, read now (it can change between attempts).
async fn active_key() -> Option<String> {
    let manager = account_pool::global_for(deepai::PROVIDER_ID)?.active_auth_manager()?;
    manager.auth().await?.api_key().map(str::to_string)
}

pub(crate) async fn prepare(
    provider: &Provider,
    model: &str,
    conversation_key: Option<&str>,
) -> Attempt {
    let metered = deepai::catalog::model_is_metered(model);
    let permit = deepai::busy::metered_slot(metered).await;
    let key = active_key().await;
    if metered && let Some(key) = &key {
        // Before a model call: the check-in is renewed when it is past half of its interval.
        deepai::theos::global().ensure_checked_in(key).await;
    }
    deepai::theos::note_model_call();
    let mut provider = provider.clone();
    for (name, value) in deepai::headers::common(conversation_key) {
        if let Ok(value) = HeaderValue::from_str(&value) {
            provider.headers.insert(name, value);
        }
    }
    if let Some(key) = key
        && let Ok(mut value) = HeaderValue::from_str(&key)
    {
        value.set_sensitive(true);
        provider.headers.insert(deepai::headers::API_KEY, value);
    }
    Attempt {
        provider,
        responses: deepai::catalog::model_uses_responses(model),
        permit,
    }
}

impl Attempt {
    /// Keeps the turn-taking slot until the stream ends. Free models have no slot to keep.
    pub(crate) fn hold_slot(mut self, mut stream: ResponseStream) -> ResponseStream {
        let Some(permit) = self.permit.take() else {
            return stream;
        };
        let (tx, rx) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);
        let routing_affinity = stream.routing_affinity.take();
        let upstream_request_id = stream.upstream_request_id.take();
        let interrupt = stream.interrupt.take();
        tokio::spawn(async move {
            let _slot = permit;
            while let Some(event) = stream.rx_event.recv().await {
                if tx.send(event).await.is_err() {
                    break;
                }
            }
        });
        ResponseStream {
            routing_affinity,
            rx_event: rx,
            upstream_request_id,
            interrupt,
        }
    }
}

fn http_error(error: &ApiError) -> Option<deepai::errors::DeepAiError> {
    let ApiError::Transport(TransportError::Http { status, body, .. }) = error else {
        return None;
    };
    Some(deepai::errors::DeepAiError::from_response(
        status.as_u16(),
        body.as_deref().unwrap_or_default(),
    ))
}

/// How long to wait before sending the request again, when the answer was "session busy".
pub(crate) fn busy_wait(error: &ApiError) -> Option<Duration> {
    let ApiError::Transport(TransportError::Http { headers, .. }) = error else {
        return None;
    };
    let retry_after = headers
        .as_ref()
        .and_then(|map| map.get(http::header::RETRY_AFTER))
        .and_then(|value| value.to_str().ok());
    deepai::busy::busy_wait(&http_error(error)?, retry_after)
}

/// The answer to show for a refusal DeepAI explains with a code (or a plain 401, 402, 426).
pub(crate) fn user_error(error: &ApiError) -> Option<CodexErr> {
    let parsed = http_error(error)?;
    let known = parsed.code.is_some() || matches!(parsed.status, 401 | 402 | 426);
    if !known {
        return None;
    }
    let ApiError::Transport(TransportError::Http { status, body, .. }) = error else {
        return None;
    };
    Some(CodexErr::UnexpectedStatus(UnexpectedResponseError {
        status: *status,
        body: body.clone().unwrap_or_default(),
        user_message: Some(parsed.user_message()),
        url: None,
        cf_ray: None,
        request_id: None,
        identity_authorization_error: None,
        identity_error_code: None,
    }))
}
