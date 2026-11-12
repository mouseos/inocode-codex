pub use codex_api::ResponseEvent;
use codex_protocol::error::Result;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_tools::ToolSpec;
use futures::Stream;
use serde_json::Value;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// API request payload for a single model turn
#[derive(Debug, Clone)]
pub struct Prompt {
    /// Conversation context input items.
    pub input: Vec<ResponseItem>,

    /// Tool definitions to inject into this request, empty when supplied by history.
    pub(crate) tools: Arc<[ToolSpec]>,

    /// Whether parallel tool calls are permitted for this prompt.
    pub(crate) parallel_tool_calls: bool,

    pub base_instructions: BaseInstructions,

    /// Optional the output schema for the model's response.
    pub output_schema: Option<Value>,

    /// Whether the Responses API should strictly validate `output_schema`.
    pub output_schema_strict: bool,

    pub(crate) prompt_cache_ttl: Option<crate::PromptCacheTtl>,

    pub(crate) cyber_access_program: Option<codex_protocol::turn_input::CyberAccessProgram>,
}

impl Default for Prompt {
    fn default() -> Self {
        Self {
            input: Vec::new(),
            tools: Arc::default(),
            parallel_tool_calls: false,
            base_instructions: BaseInstructions::default(),
            output_schema: None,
            output_schema_strict: true,
            prompt_cache_ttl: None,
            cyber_access_program: None,
        }
    }
}

impl Prompt {
    pub(crate) fn get_formatted_input_for_request(
        &self,
        model_info: &ModelInfo,
    ) -> Vec<ResponseItem> {
        let mut input = self.input.clone();
        normalize_image_details(&mut input, model_info);
        input
    }
}

fn normalize_image_details(items: &mut [ResponseItem], model_info: &ModelInfo) {
    for item in items {
        match item {
            ResponseItem::Message { content, .. } => {
                for content_item in content {
                    if let ContentItem::InputImage { detail, .. } = content_item {
                        normalize_image_detail(detail, model_info);
                    }
                }
            }
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                if let Some(content) = output.content_items_mut() {
                    for content_item in content {
                        if let FunctionCallOutputContentItem::InputImage { detail, .. } =
                            content_item
                        {
                            normalize_image_detail(detail, model_info);
                        }
                    }
                }
            }
            ResponseItem::AdditionalTools { .. }
            | ResponseItem::Reasoning { .. }
            | ResponseItem::AgentMessage { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::ConfigurationUpdate { .. }
            | ResponseItem::CompactionTrigger { .. }
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::Other => {}
        }
    }
}

fn normalize_image_detail(detail: &mut Option<ImageDetail>, model_info: &ModelInfo) {
    if model_info.use_responses_lite {
        *detail = None;
    } else if *detail == Some(ImageDetail::Original) && !model_info.supports_image_detail_original {
        *detail = Some(DEFAULT_IMAGE_DETAIL);
    }
}

pub struct ResponseStream {
    pub(crate) routing_affinity: Option<codex_http_client::ChatGptRoutingAffinity>,
    pub(crate) rx_event: mpsc::Receiver<Result<ResponseEvent>>,
    pub(crate) interrupt: Option<oneshot::Sender<codex_api::ResponseInterrupt>>,
    /// Signals the mapper task that the consumer stopped polling before the
    /// provider stream reached its own terminal event.
    pub(crate) consumer_dropped: CancellationToken,
}

impl Stream for ResponseStream {
    type Item = Result<ResponseEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx_event.poll_recv(cx)
    }
}

impl Drop for ResponseStream {
    fn drop(&mut self) {
        self.consumer_dropped.cancel();
    }
}

/// Providers other than OpenAI's own backend do not know the `agent_message` item type and
/// answer with an error for the whole request. A message between agents (a reply, the advisor, the
/// host's facts) is sent to them as a plain user message that says who sent it. A message whose
/// text only the OpenAI backend can read (encrypted) has nothing to send and is left out.
pub(crate) fn agent_messages_as_plain_messages(input: &mut Vec<ResponseItem>) {
    use codex_protocol::models::AgentMessageInputContent;
    let items = std::mem::take(input);
    for item in items {
        match item {
            ResponseItem::AgentMessage {
                id,
                author,
                recipient,
                content,
                internal_chat_message_metadata_passthrough,
            } => {
                let Some(text) = codex_protocol::models::plaintext_agent_message_content(&content)
                else {
                    // Encrypted parts cannot be read here; keep the readable ones, if any.
                    let readable: Vec<&str> = content
                        .iter()
                        .filter_map(|part| match part {
                            AgentMessageInputContent::InputText { text } => Some(text.as_str()),
                            AgentMessageInputContent::EncryptedContent { .. } => None,
                        })
                        .collect();
                    if readable.is_empty() {
                        continue;
                    }
                    input.push(plain_message(
                        id,
                        &author,
                        &recipient,
                        &readable.join("\n"),
                        internal_chat_message_metadata_passthrough,
                    ));
                    continue;
                };
                input.push(plain_message(
                    id,
                    &author,
                    &recipient,
                    &text,
                    internal_chat_message_metadata_passthrough,
                ));
            }
            other => input.push(other),
        }
    }
}

fn plain_message(
    id: Option<codex_protocol::ResponseItemId>,
    author: &str,
    recipient: &str,
    text: &str,
    internal_chat_message_metadata_passthrough: Option<
        codex_protocol::models::InternalChatMessageMetadataPassthrough,
    >,
) -> ResponseItem {
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: format!("[message from {author} to {recipient}]\n{text}"),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough,
    }
}

#[cfg(test)]
#[path = "client_common_tests.rs"]
mod tests;
