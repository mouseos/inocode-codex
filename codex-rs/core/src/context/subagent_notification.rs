use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::input_text;
use codex_protocol::protocol::AgentStatus;

use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SubagentNotification {
    pub(crate) agent_reference: String,
    pub(crate) status: AgentStatus,
    /// What the host recorded about the child (files changed, commands run), next to what the
    /// child says it did. Absent when nothing was tracked.
    pub(crate) host_facts: Option<String>,
}

impl SubagentNotification {
    pub(crate) fn new(agent_reference: impl Into<String>, status: AgentStatus) -> Self {
        Self {
            agent_reference: agent_reference.into(),
            status,
            host_facts: None,
        }
    }

    pub(crate) fn with_host_facts(mut self, facts: Option<String>) -> Self {
        self.host_facts = facts;
        self
    }
}

impl ContextualUserFragment for SubagentNotification {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.subagent_notification".to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<subagent_notification>", "</subagent_notification>")
    }

    fn body(&self) -> ContentItem {
        let mut body = serde_json::json!({
            "agent_path": &self.agent_reference,
            "status": &self.status,
        });
        if let Some(facts) = &self.host_facts {
            body["host_facts"] = serde_json::Value::String(facts.clone());
        }
        input_text(format!("\n{body}\n"))
    }
}
