//! Objective-change requests and completion evidence of the fixed goal, forwarded to clients.
//! The state lives in `codex_goal_extension::GoalService`; this file only converts and forwards.

use codex_app_server_protocol::InocodeGoalCompletionEvidence;
use codex_app_server_protocol::InocodeGoalCompletionEvidenceNotification;
use codex_app_server_protocol::InocodeGoalEditRequest;
use codex_app_server_protocol::InocodeGoalEditRequestedNotification;
use codex_app_server_protocol::InocodeGoalEvidenceItem;
use codex_app_server_protocol::InocodeGoalHostFact;
use codex_app_server_protocol::InocodeGoalRequirementReport;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadGoalStatus;
use codex_goal_extension::GoalCompletionEvidence;
use codex_goal_extension::GoalEditRequest;
use codex_goal_extension::GoalService;
use std::sync::Arc;

pub(crate) fn edit_request_to_protocol(request: GoalEditRequest) -> InocodeGoalEditRequest {
    InocodeGoalEditRequest {
        thread_id: request.thread_id.to_string(),
        request_id: request.request_id,
        current_objective: request.current_objective,
        new_objective: request.new_objective,
        reason: request.reason,
        status: ThreadGoalStatus::from(request.status),
        token_budget: request.token_budget,
        tokens_used: request.tokens_used,
        time_used_seconds: request.time_used_seconds,
        requested_at: request.requested_at,
    }
}

pub(crate) fn evidence_to_protocol(evidence: GoalCompletionEvidence) -> InocodeGoalCompletionEvidence {
    InocodeGoalCompletionEvidence {
        thread_id: evidence.thread_id,
        goal_id: evidence.goal_id,
        objective: evidence.objective,
        recorded_at: evidence.recorded_at,
        requirements: evidence
            .requirements
            .into_iter()
            .map(|r| InocodeGoalRequirementReport {
                requirement: r.requirement,
                claim: r.claim,
                evidence: r
                    .evidence
                    .into_iter()
                    .map(|e| InocodeGoalEvidenceItem {
                        kind: e.kind,
                        detail: e.detail,
                        exit_code: e.exit_code,
                    })
                    .collect(),
            })
            .collect(),
        host_facts: evidence
            .host_facts
            .into_iter()
            .map(|f| InocodeGoalHostFact {
                tool: f.tool,
                kind: f.kind,
                detail: f.detail,
                outcome: f.outcome,
                success: f.success,
                exit_code: f.exit_code,
            })
            .collect(),
    }
}

/// Forwards edit requests and completion evidence to clients. Call once at startup.
pub(crate) fn spawn_notifier(
    outgoing: Arc<crate::outgoing_message::OutgoingMessageSender>,
    goal_service: Arc<GoalService>,
) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let mut edit_rx = goal_service.subscribe_goal_edit_requests();
    let edit_out = Arc::clone(&outgoing);
    handle.spawn(async move {
        loop {
            match edit_rx.recv().await {
                Ok(request) => {
                    let p = edit_request_to_protocol(request);
                    edit_out
                        .send_server_notification(ServerNotification::InocodeGoalEditRequested(
                            InocodeGoalEditRequestedNotification {
                                thread_id: p.thread_id,
                                request_id: p.request_id,
                                current_objective: p.current_objective,
                                new_objective: p.new_objective,
                                reason: p.reason,
                                status: p.status,
                                token_budget: p.token_budget,
                                tokens_used: p.tokens_used,
                                time_used_seconds: p.time_used_seconds,
                                requested_at: p.requested_at,
                            },
                        ))
                        .await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    let mut evidence_rx = goal_service.subscribe_completion_evidence();
    handle.spawn(async move {
        loop {
            match evidence_rx.recv().await {
                Ok(evidence) => {
                    let p = evidence_to_protocol(evidence);
                    outgoing
                        .send_server_notification(
                            ServerNotification::InocodeGoalCompletionEvidenceNotice(
                                InocodeGoalCompletionEvidenceNotification {
                                    thread_id: p.thread_id,
                                    goal_id: p.goal_id,
                                    objective: p.objective,
                                    recorded_at: p.recorded_at,
                                    requirements: p.requirements,
                                    host_facts: p.host_facts,
                                },
                            ),
                        )
                        .await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}
