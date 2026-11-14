//! The chat sees InoCode's own tools (knowledge, evidence, prohibitions) as dynamic tool call
//! items: started and completed around the call, with the arguments and the result. The items
//! are kept in the rollout like any other, so a session opened again shows them too (the
//! paginated history is projected from item events only).

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::items::DynamicToolCallItem;
use codex_protocol::items::DynamicToolCallStatus;
use codex_protocol::items::TurnItem;
use std::time::Instant;

pub(crate) struct OwnToolItem {
    call_id: String,
    tool: String,
    arguments: serde_json::Value,
    started: Instant,
}

impl OwnToolItem {
    /// The call has started (the chat shows the row as running).
    pub(crate) async fn start(
        session: &Session,
        turn: &TurnContext,
        call_id: &str,
        tool: &str,
        arguments: &str,
    ) -> Self {
        let arguments = serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::Value::String(arguments.to_string()));
        let me = Self { call_id: call_id.to_string(), tool: tool.to_string(), arguments, started: Instant::now() };
        session.emit_turn_item_started(turn, &me.item(DynamicToolCallStatus::InProgress, None)).await;
        me
    }

    /// The call is over: `output` is what the model gets back (JSON text, or an error).
    pub(crate) async fn finish(self, session: &Session, turn: &TurnContext, output: Result<&str, &str>) {
        let status = if output.is_ok() { DynamicToolCallStatus::Completed } else { DynamicToolCallStatus::Failed };
        let text = match output {
            Ok(t) | Err(t) => t.to_string(),
        };
        let item = self.item(status, Some(text));
        session.emit_turn_item_completed(turn, item).await;
    }

    fn item(&self, status: DynamicToolCallStatus, output: Option<String>) -> TurnItem {
        let done = output.is_some();
        let failed = status == DynamicToolCallStatus::Failed;
        TurnItem::DynamicToolCall(DynamicToolCallItem {
            id: self.call_id.clone(),
            namespace: None,
            tool: self.tool.clone(),
            arguments: self.arguments.clone(),
            status,
            error: if failed { output.clone() } else { None },
            content_items: output.map(|text| vec![DynamicToolCallOutputContentItem::InputText { text }]),
            success: done.then_some(!failed),
            duration: done.then(|| self.started.elapsed()),
        })
    }
}

/// One of InoCode's own tools, wrapped so that each call is an item the chat sees (and keeps).
pub struct OwnTool<H>(pub H);

impl<H> crate::tools::registry::ToolExecutor<crate::tools::context::ToolInvocation> for OwnTool<H>
where
    H: crate::tools::registry::ToolExecutor<crate::tools::context::ToolInvocation>,
{
    fn tool_name(&self) -> codex_tools::ToolName {
        self.0.tool_name()
    }
    fn spec(&self) -> codex_tools::ToolSpec {
        self.0.spec()
    }
    fn supports_parallel_tool_calls(&self) -> bool {
        self.0.supports_parallel_tool_calls()
    }
    fn handle<'a>(&'a self, invocation: crate::tools::context::ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        crate::tools::context::ToolInvocation: 'a,
    {
        Box::pin(async move {
            let session = std::sync::Arc::clone(&invocation.session);
            let turn = std::sync::Arc::clone(&invocation.turn);
            let call_id = invocation.call_id.clone();
            let payload = invocation.payload.clone();
            let raw = match &payload {
                crate::tools::context::ToolPayload::Function { arguments } => arguments.clone(),
                _ => String::new(),
            };
            let tool = self.0.tool_name().to_string();
            let item = OwnToolItem::start(&session, &turn, &call_id, &tool, &raw).await;
            let result = self.0.handle(invocation).await;
            match &result {
                Ok(output) => {
                    let text = match output.to_response_item(&call_id, &payload) {
                        codex_protocol::models::ResponseInputItem::FunctionCallOutput { output, .. } => {
                            output.text_content().unwrap_or_default().to_string()
                        }
                        _ => output.log_output(),
                    };
                    item.finish(&session, &turn, Ok(&text)).await;
                }
                Err(e) => item.finish(&session, &turn, Err(&e.to_string())).await,
            }
            result
        })
    }
}

impl<H> crate::tools::registry::CoreToolRuntime for OwnTool<H> where
    H: crate::tools::registry::ToolExecutor<crate::tools::context::ToolInvocation>
{
}
