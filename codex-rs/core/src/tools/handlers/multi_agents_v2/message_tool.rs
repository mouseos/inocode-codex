//! Shared argument parsing and dispatch for the v2 agent messaging tools.
//!
//! `send_message` and `followup_task` share the same submission path and differ only in whether the
//! resulting `InterAgentCommunication` should wake the target immediately.

use super::analytics::ToolCallAnalytics;
use super::*;
use crate::TurnStartOptions;
use crate::agent::api::AgentInput;
use crate::agent::api::AgentTarget;
use crate::agent::api::SendRequest;
use crate::agent::child_config::build_agent_resume_config;
use crate::agent::types::MessageDeliveryMode;
use crate::tools::context::FunctionToolOutput;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
/// Input for the MultiAgentV2 `send_message` tool.
pub(crate) struct SendMessageArgs {
    pub(crate) target: String,
    pub(crate) message: String,
    /// The message this one answers: `#<turn>.<item>` in the sender's own history.
    #[serde(default)]
    pub(crate) reply_to: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
/// Input for the MultiAgentV2 `followup_task` tool.
pub(crate) struct FollowupTaskArgs {
    pub(crate) target: String,
    pub(crate) message: String,
    /// The message this one answers: `#<turn>.<item>` in the sender's own history.
    #[serde(default)]
    pub(crate) reply_to: Option<String>,
}

pub(super) fn message_content(message: String) -> Result<String, FunctionCallError> {
    if message.trim().is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "Empty message can't be sent to an agent".to_string(),
        ));
    }
    Ok(message)
}

/// Handles the shared MultiAgentV2 message flow for both `send_message` and `followup_task`.
pub(super) async fn handle_message_string_tool(
    invocation: ToolInvocation,
    mode: MessageDeliveryMode,
    target: String,
    message: String,
    reply_to: Option<String>,
    analytics: &mut ToolCallAnalytics,
) -> Result<FunctionToolOutput, FunctionCallError> {
    let message = message_content(message)?;
    let reply_to = reply_to.map(|r| r.trim().to_string()).filter(|r| !r.is_empty());
    if let Some(reply) = &reply_to
        && !codex_inocode_protocol::is_valid_message_ref(reply)
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "reply_to must look like `#12.3` (turn.item in your own history), got `{reply}`"
        )));
    }
    let ToolInvocation {
        session,
        turn,
        call_id,
        source,
        ..
    } = invocation;
    // A reply carries the id of the message it answers (and whose history that id is in), so the
    // receiver can read it with `read_message`.
    let message = match &reply_to {
        Some(reply) => {
            let hint = reply_hint(&session, reply).await;
            codex_inocode_protocol::attach_reply(
                &message,
                reply,
                Some(&session.thread_id.to_string()),
                hint.as_deref(),
            )
        }
        None => message,
    };
    let logged_text = message.clone();
    let receiver_thread_id = resolve_agent_target(&session, &turn, &target).await?;
    analytics.set_receiver(receiver_thread_id);
    let resume_config =
        build_agent_resume_config(&turn).map_err(FunctionCallError::RespondToModel)?;
    let receipt = session
        .services
        .agent_control
        .send(SendRequest {
            caller: session.thread_id,
            target: AgentTarget::Id(receiver_thread_id),
            resume_config,
            input: AgentInput::Message {
                message: agent_message_from_tool(message, &source),
                mode,
            },
            start_options: TurnStartOptions {
                parent_turn_id: (mode == MessageDeliveryMode::TriggerTurn)
                    .then(|| turn.sub_id.clone()),
                root_turn_id: turn.turn_metadata_state.root_turn_id(),
                turn_trigger: turn.turn_metadata_state.current_turn_trigger(),
                cyber_access_program: turn.cyber_access_program,
                ..Default::default()
            },
        })
        .await
        .map_err(|err| collab_v2_agent_error(receiver_thread_id, err))?;
    let receiver_agent_path = receipt.metadata.agent_path.ok_or_else(|| {
        FunctionCallError::RespondToModel("target agent is missing an agent_path".to_string())
    })?;
    crate::inocode_loop::log_agent_message_with(
        session.thread_id,
        turn.session_source
            .get_agent_path()
            .map(|path| path.to_string())
            .as_deref()
            .unwrap_or("/root"),
        receiver_agent_path.as_str(),
        if mode == MessageDeliveryMode::TriggerTurn {
            "followup_task"
        } else {
            "send_message"
        },
        &logged_text,
        crate::inocode_loop::AgentMessageMeta {
            reply_thread: reply_to.is_some().then(|| session.thread_id.to_string()),
            reply_to,
            from_thread: Some(session.thread_id.to_string()),
            to_thread: Some(receiver_thread_id.to_string()),
        },
    );
    emit_sub_agent_activity(
        &session,
        &turn,
        SubAgentActivityItem {
            model: None,
            reasoning_effort: None,
            id: call_id,
            agent_thread_id: receiver_thread_id,
            agent_path: receiver_agent_path,
            kind: SubAgentActivityKind::Interacted,
        },
    )
    .await;

    Ok(FunctionToolOutput::from_text(String::new(), Some(true)))
}

/// The first words of the message `reply` names in the sender's own history, for the one-line
/// marker in front of a free-text reply. Best effort: none when it cannot be read.
async fn reply_hint(session: &crate::session::session::Session, reply: &str) -> Option<String> {
    let selector = codex_app_server_protocol::parse_message_ref(reply);
    if matches!(selector, codex_app_server_protocol::MessageSelector::ItemId(_)) {
        return None;
    }
    let _ = session.flush_rollout().await;
    let path = session.current_rollout_path().await.ok().flatten()?;
    let found = tokio::task::spawn_blocking(move || {
        crate::inocode_message_ref::resolve_in_rollout(&path, &selector)
    })
    .await
    .ok()?
    .ok()??;
    let text: String = found.0.text.chars().take(20).collect();
    Some(text.replace('\n', " "))
}
