//! Host-side records for the fixed goal: edit requests waiting for the user's approval, and the
//! evidence the model reports when it declares a goal complete.
//!
//! Neither blocks anything. `request_goal_edit` writes nothing to the goal; it stores the request
//! and tells the client, which shows the card. Completion evidence is recorded and shown; the
//! goal still completes when the model says so.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::PoisonError;

use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadGoalStatus;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::broadcast;

/// A new objective the model asked for. Nothing changes until the user approves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalEditRequest {
    pub thread_id: ThreadId,
    pub request_id: String,
    pub current_objective: String,
    pub new_objective: String,
    /// The model's own reason (a claim, not a fact).
    pub reason: String,
    pub status: ThreadGoalStatus,
    pub token_budget: Option<i64>,
    pub tokens_used: i64,
    pub time_used_seconds: i64,
    pub requested_at: i64,
}

/// One piece of evidence the model gives for a requirement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalEvidenceItem {
    /// `file`, `command`, `output`, `other` (free text).
    #[serde(default)]
    pub kind: String,
    /// The path that was looked at, the command line that was run, or the text.
    pub detail: String,
    /// Exit code, for a command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
}

/// The model's report for one requirement of the goal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalRequirementReport {
    pub requirement: String,
    /// The model's own claim about it (for example `met`, `partly met`, `not met`).
    #[serde(default)]
    pub claim: String,
    #[serde(default)]
    pub evidence: Vec<GoalEvidenceItem>,
}

/// A fact the host observed during the goal (not the model's words): a tool call it ran, as
/// the model issued it, and how it ended. What the host cannot see is left empty (unknown).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalHostFact {
    /// The tool as routed by the host.
    pub tool: String,
    /// `command` (a shell command), `file_change` (a patch), `mcp` or `other`.
    pub kind: String,
    /// The command line, or the paths a patch touched, verbatim from the call's arguments.
    pub detail: String,
    /// `completed`, `failed`, `blocked`, `aborted` or `unknown` (still running).
    pub outcome: String,
    /// The tool output's own success marker; unknown when the host did not get one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    /// The host's outcome carries no exit code today, so this stays unknown (`None`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
}

/// What the model reported when it declared the goal complete (the model's claims, as given).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalCompletionEvidence {
    pub thread_id: String,
    pub goal_id: String,
    pub objective: String,
    pub recorded_at: i64,
    /// The model's per-requirement claims and evidence, as it gave them.
    pub requirements: Vec<GoalRequirementReport>,
    /// What the host observed while the goal was active, kept apart from the model's claims.
    #[serde(default)]
    pub host_facts: Vec<GoalHostFact>,
}

#[derive(Debug)]
pub(crate) struct GoalReviewHub {
    pending: Mutex<HashMap<ThreadId, GoalEditRequest>>,
    evidence: Mutex<HashMap<ThreadId, GoalCompletionEvidence>>,
    edit_tx: broadcast::Sender<GoalEditRequest>,
    evidence_tx: broadcast::Sender<GoalCompletionEvidence>,
    /// Calls that started and have not finished: (thread, call id).
    open_calls: Mutex<HashMap<(ThreadId, String), (String, GoalHostFact)>>,
    /// Facts of the goal that is active on each thread: (goal id, facts).
    facts: Mutex<HashMap<ThreadId, (String, Vec<GoalHostFact>)>>,
}

/// The newest facts kept per thread.
const MAX_HOST_FACTS: usize = 500;

impl Default for GoalReviewHub {
    fn default() -> Self {
        Self {
            pending: Mutex::default(),
            evidence: Mutex::default(),
            edit_tx: broadcast::channel(32).0,
            evidence_tx: broadcast::channel(32).0,
            open_calls: Mutex::default(),
            facts: Mutex::default(),
        }
    }
}

impl GoalReviewHub {
    pub(crate) fn request_edit(&self, request: GoalEditRequest) {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(request.thread_id, request.clone());
        let _ = self.edit_tx.send(request);
    }

    pub(crate) fn pending_edit(&self, thread_id: ThreadId) -> Option<GoalEditRequest> {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&thread_id)
            .cloned()
    }

    /// Removes and returns the pending request when `request_id` is the one that is waiting.
    pub(crate) fn take_pending_edit(
        &self,
        thread_id: ThreadId,
        request_id: &str,
    ) -> Option<GoalEditRequest> {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        if pending
            .get(&thread_id)
            .is_some_and(|request| request.request_id == request_id)
        {
            pending.remove(&thread_id)
        } else {
            None
        }
    }

    pub(crate) fn record_evidence(&self, evidence: GoalCompletionEvidence) {
        if let Ok(thread_id) = ThreadId::from_string(&evidence.thread_id) {
            self.evidence
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(thread_id, evidence.clone());
        }
        let _ = self.evidence_tx.send(evidence);
    }

    pub(crate) fn evidence(&self, thread_id: ThreadId) -> Option<GoalCompletionEvidence> {
        self.evidence
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&thread_id)
            .cloned()
    }

    pub(crate) fn note_tool_start(
        &self,
        thread_id: ThreadId,
        goal_id: &str,
        call_id: &str,
        fact: GoalHostFact,
    ) {
        self.open_calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((thread_id, call_id.to_string()), (goal_id.to_string(), fact));
    }

    pub(crate) fn note_tool_finish(
        &self,
        thread_id: ThreadId,
        call_id: &str,
        outcome: &str,
        success: Option<bool>,
    ) {
        let Some((goal_id, mut fact)) = self
            .open_calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(thread_id, call_id.to_string()))
        else {
            return;
        };
        fact.outcome = outcome.to_string();
        fact.success = success;
        let mut facts = self.facts.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = facts
            .entry(thread_id)
            .or_insert_with(|| (goal_id.clone(), Vec::new()));
        if entry.0 != goal_id {
            *entry = (goal_id, Vec::new());
        }
        entry.1.push(fact);
        if entry.1.len() > MAX_HOST_FACTS {
            let extra = entry.1.len() - MAX_HOST_FACTS;
            entry.1.drain(..extra);
        }
    }

    /// The facts recorded for `goal_id` on `thread_id` (empty when none, or for another goal).
    pub(crate) fn host_facts(&self, thread_id: ThreadId, goal_id: &str) -> Vec<GoalHostFact> {
        self.facts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&thread_id)
            .filter(|(id, _)| id == goal_id)
            .map(|(_, facts)| facts.clone())
            .unwrap_or_default()
    }

    pub(crate) fn subscribe_edits(&self) -> broadcast::Receiver<GoalEditRequest> {
        self.edit_tx.subscribe()
    }

    pub(crate) fn subscribe_evidence(&self) -> broadcast::Receiver<GoalCompletionEvidence> {
        self.evidence_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(thread_id: ThreadId, id: &str) -> GoalEditRequest {
        GoalEditRequest {
            thread_id,
            request_id: id.to_string(),
            current_objective: "old".into(),
            new_objective: "new".into(),
            reason: "because".into(),
            status: ThreadGoalStatus::Active,
            token_budget: Some(100),
            tokens_used: 10,
            time_used_seconds: 5,
            requested_at: 1,
        }
    }

    #[test]
    fn pending_edit_is_kept_until_resolved_and_only_the_waiting_id_resolves() {
        let hub = GoalReviewHub::default();
        let thread_id = ThreadId::new();
        let mut rx = hub.subscribe_edits();
        hub.request_edit(request(thread_id, "a"));
        assert_eq!(rx.try_recv().unwrap().request_id, "a");
        assert_eq!(hub.pending_edit(thread_id).unwrap().new_objective, "new");
        // A newer request replaces the older one; the old id no longer resolves.
        hub.request_edit(request(thread_id, "b"));
        assert!(hub.take_pending_edit(thread_id, "a").is_none());
        assert_eq!(hub.take_pending_edit(thread_id, "b").unwrap().request_id, "b");
        assert!(hub.pending_edit(thread_id).is_none());
    }

    #[test]
    fn evidence_is_recorded_per_thread() {
        let hub = GoalReviewHub::default();
        let thread_id = ThreadId::new();
        let evidence = GoalCompletionEvidence {
            thread_id: thread_id.to_string(),
            goal_id: "g".into(),
            objective: "o".into(),
            recorded_at: 1,
            requirements: vec![GoalRequirementReport {
                requirement: "tests pass".into(),
                claim: "met".into(),
                evidence: vec![GoalEvidenceItem {
                    kind: "command".into(),
                    detail: "cargo test".into(),
                    exit_code: Some(0),
                }],
            }],
            host_facts: Vec::new(),
        };
        hub.record_evidence(evidence.clone());
        assert_eq!(hub.evidence(thread_id), Some(evidence));
    }
}
