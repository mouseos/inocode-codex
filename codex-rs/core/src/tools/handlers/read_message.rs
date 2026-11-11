//! `read_message`: reads one earlier message of the conversation by its reference.
//!
//! The GUI lets the user reply to a specific message; the reply starts with a line such as
//! `[返信 #12.3「SQLite の並行…」]`. When that message is no longer visible (compaction, fork), the
//! model reads it here. The message is looked up in the rollout, which keeps every item, so
//! compaction does not hide anything from this tool.

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_app_server_protocol::MessageSelector;
use codex_app_server_protocol::parse_message_ref;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use std::collections::BTreeMap;

pub(crate) const READ_MESSAGE_TOOL_NAME: &str = "read_message";

pub struct ReadMessageHandler;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadMessageArgs {
    id: String,
    #[serde(default)]
    thread_id: Option<String>,
}

pub(crate) fn create_read_message_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "id".to_string(),
            JsonSchema::string(Some(
                "The message reference: `#<turn>.<item>` such as `#12.3`, or an item id."
                    .to_string(),
            )),
        ),
        (
            "thread_id".to_string(),
            JsonSchema::string(Some(
                "Optional. Read the message from another thread instead of this one.".to_string(),
            )),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: READ_MESSAGE_TOOL_NAME.to_string(),
        description: "Read the full text of an earlier message of this conversation. \
A user message may begin with a line `[返信 #ID「hint」]`: the user is replying to the message \
with that ID, and the hint is its first words. If that message is not visible in your history \
(after compaction or a fork), call this tool with the ID to read it. Returns the kind of the \
message (user, assistant or tool), its turn number and its text."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["id".to_string()]),
            /*additional_properties*/ Some(false.into()),
        ),
        output_schema: None,
    })
}

impl ToolExecutor<ToolInvocation> for ReadMessageHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(READ_MESSAGE_TOOL_NAME)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        create_read_message_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolInvocation {
                session,
                turn,
                payload,
                ..
            } = invocation;
            let ToolPayload::Function { arguments } = payload else {
                return Err(FunctionCallError::RespondToModel(format!(
                    "{READ_MESSAGE_TOOL_NAME} handler received unsupported payload"
                )));
            };
            let args: ReadMessageArgs = parse_arguments(&arguments)?;
            let selector = parse_message_ref(&args.id);

            let path = match args.thread_id.as_deref().filter(|t| !t.is_empty()) {
                Some(other) if other != session.thread_id.to_string() => {
                    codex_rollout::find_thread_path_by_id_str(
                        &turn.config.codex_home,
                        other,
                        session.state_db().as_deref(),
                    )
                    .await
                    .map_err(|err| {
                        FunctionCallError::RespondToModel(format!(
                            "failed to look up thread {other}: {err}"
                        ))
                    })?
                    .ok_or_else(|| {
                        FunctionCallError::RespondToModel(format!("thread not found: {other}"))
                    })?
                }
                _ => {
                    // The recorder may still hold the newest items.
                    let _ = session.flush_rollout().await;
                    session
                        .current_rollout_path()
                        .await
                        .ok()
                        .flatten()
                        .ok_or_else(|| {
                            FunctionCallError::RespondToModel(
                                "this thread has no saved history to read from".to_string(),
                            )
                        })?
                }
            };
            let looked_up = tokio::task::spawn_blocking({
                let selector = selector.clone();
                move || crate::inocode_message_ref::resolve_in_rollout(&path, &selector)
            })
            .await
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!("failed to read the history: {err}"))
            })?
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!("failed to read the history: {err}"))
            })?;
            let Some((found, _turn_count)) = looked_up else {
                let what = match &selector {
                    MessageSelector::Position { turn, item } => format!("#{turn}.{item}"),
                    MessageSelector::Mail { turn, n } => format!("#{turn}.@{n}"),
                    MessageSelector::ItemId(id) => format!("item id {id}"),
                };
                return Err(FunctionCallError::RespondToModel(format!(
                    "no message found for {what}"
                )));
            };
            // A long text is replaced by a preview and a file path by the same mechanism as any
            // other large tool output when it is recorded in the history.
            let id = match found.role {
                codex_app_server_protocol::MessageRole::Agent => {
                    format!("#{}.@{}", found.turn_number, found.item_number)
                }
                _ => format!("#{}.{}", found.turn_number, found.item_number),
            };
            let route = match (&found.from, &found.to) {
                (Some(from), Some(to)) => format!("from: {from}\nto: {to}\n"),
                _ => String::new(),
            };
            let text = format!(
                "kind: {}\nturn: {}\nid: {} ({})\n{}\n{}",
                found.role.as_str(),
                found.turn_number,
                id,
                found.item_id,
                route,
                found.text
            );
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                text,
                Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for ReadMessageHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_description_explains_the_reply_line() {
        let ToolSpec::Function(tool) = create_read_message_tool() else {
            panic!("function tool expected");
        };
        assert_eq!(tool.name, "read_message");
        assert!(tool.description.contains("[返信 #ID「hint」]"));
        assert!(tool.description.contains("read it"));
    }
}
