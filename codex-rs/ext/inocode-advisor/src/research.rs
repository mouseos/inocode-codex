//! A statement of fact for research that is not recorded: the agent looked things up several
//! times and recorded none of it in the evidence database. Nothing is stopped or required; the
//! statement is made once, and again only after a record has been made and more lookups follow.

/// Lookups without a record after which the statement is made.
const LOOKUPS_BEFORE_STATEMENT: u32 = 3;

#[derive(Debug, Clone, Default)]
pub struct ResearchTracker {
    lookups: u32,
    stated: bool,
}

fn is_lookup(tool: &str) -> bool {
    let name = tool.to_ascii_lowercase();
    if name.contains("evidence") || name.contains("knowledge") {
        return false;
    }
    ["web_search", "fetch", "shopping_search", "media_search"]
        .iter()
        .any(|w| name.contains(w))
}

impl ResearchTracker {
    /// Takes a finished call into account. Returns the statement when it is time to make it.
    pub fn observe(&mut self, tool: &str, success: bool) -> Option<String> {
        let name = tool.to_ascii_lowercase();
        if name.contains("record_evidence") && success {
            self.lookups = 0;
            self.stated = false;
            return None;
        }
        if success && is_lookup(tool) {
            self.lookups += 1;
            if self.lookups >= LOOKUPS_BEFORE_STATEMENT && !self.stated {
                self.stated = true;
                return Some(format!(
                    "{} lookups so far and none of their results is recorded in the evidence database (record_evidence keeps a claim with its verbatim quote and source).",
                    self.lookups
                ));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_statement_comes_once_after_three_unrecorded_lookups() {
        let mut t = ResearchTracker::default();
        assert_eq!(t.observe("web_search", true), None);
        assert_eq!(t.observe("web_search", true), None);
        let said = t.observe("web_search", true).expect("said");
        assert!(said.starts_with("3 lookups"));
        assert_eq!(t.observe("web_search", true), None);
    }

    #[test]
    fn a_record_resets_it_and_other_calls_do_not_count() {
        let mut t = ResearchTracker::default();
        for _ in 0..2 {
            t.observe("web_search", true);
        }
        t.observe("exec_command", true);
        t.observe("search_knowledge", true);
        t.observe("web_search", false);
        assert_eq!(t.observe("record_evidence", true), None);
        for _ in 0..2 {
            assert_eq!(t.observe("web_search", true), None);
        }
        assert!(t.observe("fetch_url", true).is_some());
    }
}
