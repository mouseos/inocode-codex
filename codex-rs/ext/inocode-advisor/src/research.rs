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

/// A command that reads something from the web (the agent may look things up with a shell
/// instead of a search tool).
fn command_reads_the_web(args: &str) -> bool {
    let a = args.to_ascii_lowercase();
    let fetches = ["curl", "wget", "invoke-webrequest", "invoke-restmethod", "iwr ", "irm ", "webclient", "httpclient", "start-bitstransfer"]
        .iter()
        .any(|w| a.contains(w));
    fetches && (a.contains("http://") || a.contains("https://") || a.contains("$url") || a.contains("$uri"))
}

/// How many lookups a finished call made: a search or fetch tool counts 1; a command that reads
/// the web counts its URLs (one command may fetch several pages).
fn lookups_in(tool: &str, args: &str) -> u32 {
    let name = tool.to_ascii_lowercase();
    if name.contains("evidence") || name.contains("knowledge") {
        return 0;
    }
    if ["web", "fetch", "shopping_search", "media_search", "browser"].iter().any(|w| name.contains(w)) {
        return 1;
    }
    if (name.contains("exec") || name.contains("shell") || name.contains("command")) && command_reads_the_web(args) {
        let a = args.to_ascii_lowercase();
        let urls = a.matches("https://").count() + a.matches("http://").count();
        return u32::try_from(urls.clamp(1, 20)).unwrap_or(1);
    }
    0
}

impl ResearchTracker {
    /// Takes a finished call into account. Returns the statement when it is time to make it.
    pub fn observe(&mut self, tool: &str, args: &str, success: bool) -> Option<String> {
        let name = tool.to_ascii_lowercase();
        if name.contains("record_evidence") && success {
            self.lookups = 0;
            self.stated = false;
            return None;
        }
        let found = if success { lookups_in(tool, args) } else { 0 };
        if found > 0 {
            self.lookups += found;
            if self.lookups >= LOOKUPS_BEFORE_STATEMENT && !self.stated {
                self.stated = true;
                return Some(format!(
                    "{} lookups so far and none of their results is recorded in the project's evidence pages (record_evidence keeps a claim with its verbatim quote and source, and the user sees it in the evidence tab; files saved in the folder are not shown there).",
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
        assert_eq!(t.observe("web_search", "", true), None);
        assert_eq!(t.observe("web_search", "", true), None);
        let said = t.observe("web_search", "", true).expect("said");
        assert!(said.starts_with("3 lookups"));
        assert_eq!(t.observe("web_search", "", true), None);
    }

    #[test]
    fn a_record_resets_it_and_other_calls_do_not_count() {
        let mut t = ResearchTracker::default();
        for _ in 0..2 {
            t.observe("web_search", "", true);
        }
        t.observe("exec_command", "{\"cmd\":\"ls\"}", true);
        t.observe("search_knowledge", "", true);
        t.observe("web_search", "", false);
        assert_eq!(t.observe("record_evidence", "", true), None);
        for _ in 0..2 {
            assert_eq!(t.observe("web_search", "", true), None);
        }
        assert!(t.observe("fetch_url", "", true).is_some());
    }

    #[test]
    fn commands_that_read_the_web_count() {
        let mut t = ResearchTracker::default();
        let get = r#"{"cmd":"Invoke-WebRequest -Uri https://tokio.rs -OutFile research\tokio.html"}"#;
        assert_eq!(t.observe("exec_command", get, true), None);
        assert_eq!(t.observe("exec_command", r#"{"cmd":"curl -sL https://docs.rs/smol"}"#, true), None);
        assert_eq!(t.observe("exec_command", r#"{"cmd":"Get-Content -Raw research\tokio.html"}"#, true), None);
        assert!(t.observe("_web_run", "{}", true).is_some());
    }

    #[test]
    fn one_command_that_fetches_several_pages_counts_each() {
        let mut t = ResearchTracker::default();
        let many = r#"{"cmd":"$urls = @{'a'='https://tokio.rs/';'b'='https://docs.rs/smol';'c'='https://async.rs'}; foreach ($u in $urls.Values) { Invoke-WebRequest -Uri $u }"}"#;
        assert!(t.observe("exec_command", many, true).expect("said").starts_with("3 lookups"));
    }
}
