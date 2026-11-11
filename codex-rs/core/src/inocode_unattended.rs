//! Unattended runs: what a refusal says when nobody can answer, and how long the agent waited
//! for the user.

/// The sentence every "cannot ask the user" refusal carries. One place, so the wording cannot
/// drift between refusals. A macro so it can be spliced into `const` strings.
macro_rules! unattended_note {
    () => {
        "No one is available to answer (unattended). If a reversible alternative exists, \
continue with it, and record the decision."
    };
}
pub(crate) use unattended_note;

#[allow(dead_code)]
pub(crate) const UNATTENDED_NOTE: &str = unattended_note!();

/// Records one question put to the user, and how long it was waited on, when dropped (so the
/// time covers the whole wait, whatever the answer).
pub(crate) struct UserQuestionTimer {
    thread_id: String,
    kind: &'static str,
    started: std::time::Instant,
}

impl UserQuestionTimer {
    pub(crate) fn start(thread_id: codex_protocol::ThreadId, kind: &'static str) -> Self {
        Self {
            thread_id: thread_id.to_string(),
            kind,
            started: std::time::Instant::now(),
        }
    }
}

impl Drop for UserQuestionTimer {
    fn drop(&mut self) {
        if codex_inocode_flags::InocodeFlags::load().metrics_enabled {
            codex_inocode_metrics::record_global(
                &self.thread_id,
                codex_inocode_metrics::MetricEvent::UserQuestion {
                    question_kind: self.kind.to_string(),
                    wait_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_note_says_what_to_do() {
        assert!(UNATTENDED_NOTE.contains("reversible alternative"));
        assert!(UNATTENDED_NOTE.contains("record the decision"));
    }
}
