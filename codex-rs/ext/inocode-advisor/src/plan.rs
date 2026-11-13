//! A statement of fact for work without a checklist: a turn has made several tool calls and the
//! agent has not written its checklist (`update_plan`), so the user's checklist panel is empty.
//! Nothing is stopped or required; the statement is made at most once per turn.

/// Tool calls in one turn without a checklist after which the statement is made.
const CALLS_BEFORE_STATEMENT: u32 = 3;

#[derive(Debug, Clone, Default)]
pub struct PlanTracker {
    turn: String,
    calls: u32,
    planned: bool,
    stated: bool,
}

impl PlanTracker {
    /// Takes a finished call of turn `turn` into account. Returns the statement when it is time
    /// to make it.
    pub fn observe(&mut self, turn: &str, tool: &str) -> Option<String> {
        if self.turn != turn {
            *self = PlanTracker { turn: turn.to_string(), ..Default::default() };
        }
        if tool.eq_ignore_ascii_case("update_plan") {
            self.planned = true;
            return None;
        }
        if self.planned || self.stated {
            return None;
        }
        self.calls += 1;
        if self.calls >= CALLS_BEFORE_STATEMENT {
            self.stated = true;
            return Some(format!(
                "{} tool calls so far in this turn and no checklist has been written (update_plan shows the user the steps of this task and which one is in progress; their checklist panel is empty).",
                self.calls
            ));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_statement_comes_once_per_turn_after_three_calls() {
        let mut t = PlanTracker::default();
        assert_eq!(t.observe("t1", "web_search"), None);
        assert_eq!(t.observe("t1", "exec_command"), None);
        let said = t.observe("t1", "web_search").expect("said");
        assert!(said.starts_with("3 tool calls"));
        assert_eq!(t.observe("t1", "web_search"), None);
        // A new turn counts again.
        assert_eq!(t.observe("t2", "web_search"), None);
        assert_eq!(t.observe("t2", "web_search"), None);
        assert!(t.observe("t2", "web_search").is_some());
    }

    #[test]
    fn a_checklist_in_the_turn_means_nothing_is_said() {
        let mut t = PlanTracker::default();
        assert_eq!(t.observe("t1", "update_plan"), None);
        for _ in 0..5 {
            assert_eq!(t.observe("t1", "exec_command"), None);
        }
    }
}
