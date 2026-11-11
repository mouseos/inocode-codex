use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use serde::Deserialize;
use serde::Serialize;

use crate::estimate::Estimate;

/// Tokens used per agent thread, recorded by the sessions themselves.
fn thread_tokens() -> &'static Mutex<HashMap<String, u64>> {
    static M: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Adds `tokens` to the running total of agent thread `thread_id`.
pub fn add_thread_tokens(thread_id: &str, tokens: u64) {
    let mut m = thread_tokens().lock().unwrap_or_else(|e| e.into_inner());
    let e = m.entry(thread_id.to_string()).or_insert(0);
    *e = e.saturating_add(tokens);
}

fn tokens_of(thread_id: &str) -> u64 {
    thread_tokens()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(thread_id)
        .copied()
        .unwrap_or(0)
}

/// Subscribe to team changes; each event is the root thread id whose roster changed.
pub fn subscribe() -> tokio::sync::broadcast::Receiver<String> {
    events().subscribe()
}

fn events() -> &'static tokio::sync::broadcast::Sender<String> {
    static S: OnceLock<tokio::sync::broadcast::Sender<String>> = OnceLock::new();
    S.get_or_init(|| tokio::sync::broadcast::channel(64).0)
}

pub(crate) fn notify_changed(root_thread_id: &str) {
    let _ = events().send(root_thread_id.to_string());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Running,
    Waiting,
    Blocked,
    Done,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamAgent {
    pub id: String,
    pub name: String,
    pub owned_paths: Vec<String>,
    pub state: AgentState,
    pub tokens: u64,
    /// RFC 3339 timestamp.
    pub started_at: String,
    /// The agent's display name (nickname), shown together with model and reasoning effort.
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Thread id of the agent that spawned this one (the line to draw in a relation diagram).
    #[serde(default)]
    pub parent_id: Option<String>,
}

/// Messages sent from one thread to another, counted. Only what the message tools actually
/// saw: an id the host did not know is `None`, never guessed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamMessageEdge {
    #[serde(default)]
    pub from_id: Option<String>,
    #[serde(default)]
    pub to_id: Option<String>,
    pub count: u64,
    pub last_ts_ms: u64,
    /// `send_message` or `followup_task`.
    pub last_delivery: String,
}

/// Response body of `inocode/team/status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamStatus {
    pub agents: Vec<TeamAgent>,
    pub estimate: Estimate,
    #[serde(default)]
    pub messages: Vec<TeamMessageEdge>,
}

/// Thread-safe roster of agents for one thread.
#[derive(Debug, Default)]
pub struct TeamRoster {
    root: String,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    agents: Vec<TeamAgent>,
    estimate: Option<Estimate>,
    messages: Vec<TeamMessageEdge>,
}

impl TeamRoster {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn upsert(&self, agent: TeamAgent) {
        let mut g = self.lock();
        match g.agents.iter_mut().find(|a| a.id == agent.id) {
            Some(slot) => *slot = agent,
            None => g.agents.push(agent),
        }
        drop(g);
        self.changed();
    }

    pub fn set_state(&self, id: &str, state: AgentState) -> bool {
        let mut g = self.lock();
        match g.agents.iter_mut().find(|a| a.id == id) {
            Some(a) if a.state == state => true,
            Some(a) => {
                a.state = state;
                drop(g);
                self.changed();
                true
            }
            None => false,
        }
    }

    fn changed(&self) {
        if !self.root.is_empty() {
            notify_changed(&self.root);
        }
    }

    pub fn add_tokens(&self, id: &str, tokens: u64) {
        let mut g = self.lock();
        if let Some(a) = g.agents.iter_mut().find(|a| a.id == id) {
            a.tokens = a.tokens.saturating_add(tokens);
        }
    }

    /// Counts one message `from_id -> to_id` (either may be unknown).
    pub fn record_message(
        &self,
        from_id: Option<&str>,
        to_id: Option<&str>,
        delivery: &str,
        ts_ms: u64,
    ) {
        let mut g = self.lock();
        match g
            .messages
            .iter_mut()
            .find(|m| m.from_id.as_deref() == from_id && m.to_id.as_deref() == to_id)
        {
            Some(m) => {
                m.count += 1;
                m.last_ts_ms = ts_ms;
                m.last_delivery = delivery.to_string();
            }
            None => g.messages.push(TeamMessageEdge {
                from_id: from_id.map(str::to_string),
                to_id: to_id.map(str::to_string),
                count: 1,
                last_ts_ms: ts_ms,
                last_delivery: delivery.to_string(),
            }),
        }
        drop(g);
        self.changed();
    }

    pub fn set_estimate(&self, estimate: Estimate) {
        self.lock().estimate = Some(estimate);
        self.changed();
    }

    pub fn status(&self) -> TeamStatus {
        let g = self.lock();
        TeamStatus {
            agents: g
                .agents
                .iter()
                .cloned()
                .map(|mut a| {
                    a.tokens = a.tokens.max(tokens_of(&a.id));
                    a
                })
                .collect(),
            estimate: g
                .estimate
                .clone()
                .unwrap_or_else(|| Estimate::single("no estimate yet")),
            messages: g.messages.clone(),
        }
    }
}

fn existing_roster(thread_id: &str) -> Option<Arc<TeamRoster>> {
    registry().lock().unwrap_or_else(|e| e.into_inner()).get(thread_id).cloned()
}

/// Status of `thread_id` including the agents spawned by its agents, and so on down the tree.
/// The estimate is the one of `thread_id` itself.
pub fn status_tree(thread_id: &str) -> TeamStatus {
    let mut status = roster_for(thread_id).status();
    let mut seen = std::collections::HashSet::from([thread_id.to_string()]);
    let mut queue: Vec<String> = status.agents.iter().map(|a| a.id.clone()).collect();
    while let Some(id) = queue.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(roster) = existing_roster(&id) {
            let child = roster.status();
            queue.extend(child.agents.iter().map(|a| a.id.clone()));
            status.agents.extend(child.agents);
            status.messages.extend(child.messages);
        }
    }
    status
}

fn registry() -> &'static Mutex<HashMap<String, Arc<TeamRoster>>> {
    static MAP: OnceLock<Mutex<HashMap<String, Arc<TeamRoster>>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Process-wide roster of `thread_id`, shared by the agent tools (writers) and
/// `inocode/team/status` (reader).
pub fn roster_for(thread_id: &str) -> Arc<TeamRoster> {
    let mut map = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    map.entry(thread_id.to_string())
        .or_insert_with(|| {
            Arc::new(TeamRoster {
                root: thread_id.to_string(),
                ..Default::default()
            })
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_returns_same_roster() {
        assert!(Arc::ptr_eq(&roster_for("x"), &roster_for("x")));
    }

    #[test]
    fn status_serializes_to_contract_shape() {
        let r = TeamRoster::default();
        r.upsert(TeamAgent {
            id: "a1".into(),
            name: "api".into(),
            owned_paths: vec!["src/api".into()],
            state: AgentState::Running,
            tokens: 10,
            started_at: "2026-11-11T00:00:00Z".into(),
            nickname: Some("Euclid".into()),
            model: Some("m".into()),
            reasoning_effort: Some("high".into()),
            parent_id: Some("root".into()),
        });
        r.record_message(Some("root"), Some("a1"), "send_message", 5);
        r.record_message(Some("root"), Some("a1"), "followup_task", 9);
        r.add_tokens("a1", 5);
        assert!(r.set_state("a1", AgentState::Blocked));
        let v = serde_json::to_value(r.status()).unwrap();
        assert_eq!(v["agents"][0]["ownedPaths"][0], "src/api");
        assert_eq!(v["agents"][0]["state"], "blocked");
        assert_eq!(v["agents"][0]["tokens"], 15);
        assert_eq!(v["estimate"]["decision"], "single");
        assert!(v["estimate"]["costMultiplier"].is_number());
        assert_eq!(v["agents"][0]["parentId"], "root");
        assert_eq!(v["messages"][0]["count"], 2);
        assert_eq!(v["messages"][0]["lastDelivery"], "followup_task");
    }

    #[test]
    fn status_tree_follows_children_of_children() {
        let mk = |id: &str, parent: &str| TeamAgent {
            id: id.into(),
            name: id.into(),
            owned_paths: vec![],
            state: AgentState::Running,
            tokens: 0,
            started_at: String::new(),
            nickname: None,
            model: None,
            reasoning_effort: None,
            parent_id: Some(parent.into()),
        };
        roster_for("tree-root").upsert(mk("tree-a", "tree-root"));
        roster_for("tree-a").upsert(mk("tree-b", "tree-a"));
        roster_for("tree-a").record_message(Some("tree-a"), Some("tree-b"), "send_message", 1);
        let s = status_tree("tree-root");
        let ids: Vec<_> = s.agents.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["tree-a", "tree-b"]);
        assert_eq!(s.messages.len(), 1);
    }
}
