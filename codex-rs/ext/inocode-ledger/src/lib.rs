//! Ledger of the user's own messages.
//!
//! The host stores the **verbatim** text of every user message, in turn order, in
//! `INOCODE_HOME/ledgers/<thread>.jsonl` (append-only, never truncated). After a compaction the
//! messages that did not survive in the retained history are presented again at a fixed place.
//! Short messages are presented in full; a message longer than the preview threshold is presented
//! as its beginning plus the path of a file holding the whole text (so a pasted log does not refill
//! the window, and the model can still read it).
//!
//! The ledger does not decide what counts as an "instruction": it keeps the user's words, nothing
//! the model wrote.

use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

pub const HEADER: &str = "Original user messages (verbatim, oldest first; kept by the host across compactions):";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerEntry {
    /// 1-based position in the thread.
    pub seq: u64,
    pub ts_ms: u64,
    pub text: String,
}

/// What [`render`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    /// Entries presented in full.
    pub full: usize,
    /// Entries presented as a beginning plus a pointer to the whole text.
    pub marked: usize,
}

pub fn inocode_home() -> PathBuf {
    if let Some(v) = std::env::var_os("INOCODE_HOME")
        && !v.is_empty()
    {
        return PathBuf::from(v);
    }
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .unwrap_or_default();
    PathBuf::from(home).join(".inocode")
}

fn safe(thread_id: &str) -> String {
    thread_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn ledger_file(home: &Path, thread_id: &str) -> PathBuf {
    home.join("ledgers").join(format!("{}.jsonl", safe(thread_id)))
}

fn full_text_file(home: &Path, thread_id: &str, seq: u64) -> PathBuf {
    home.join("ledgers").join(safe(thread_id)).join(format!("{seq}.txt"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// All entries of `thread_id`, oldest first.
pub fn entries_at(home: &Path, thread_id: &str) -> Vec<LedgerEntry> {
    let Ok(text) = std::fs::read_to_string(ledger_file(home, thread_id)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<LedgerEntry>(l).ok())
        .collect()
}

/// Appends a user message verbatim. Returns its sequence number, or `None` for a blank message
/// or when the file cannot be written.
pub fn record_at(home: &Path, thread_id: &str, text: &str) -> Option<u64> {
    use std::io::Write;
    if text.trim().is_empty() {
        return None;
    }
    let path = ledger_file(home, thread_id);
    std::fs::create_dir_all(path.parent()?).ok()?;
    let seq = entries_at(home, thread_id).last().map_or(0, |e| e.seq) + 1;
    let line = serde_json::to_string(&LedgerEntry {
        seq,
        ts_ms: now_ms(),
        text: text.to_string(),
    })
    .ok()?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    writeln!(file, "{line}").ok()?;
    Some(seq)
}

pub fn record(thread_id: &str, text: &str) -> Option<u64> {
    record_at(&inocode_home(), thread_id, text)
}

fn char_prefix(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// The text to present again after a compaction. `retained` are the user messages the compacted
/// history still holds verbatim (they are not repeated). `preview_chars` is the length above which
/// a message is presented as a beginning plus the path of its full text.
/// `None` when nothing needs presenting.
pub fn render_at(
    home: &Path,
    thread_id: &str,
    retained: &[String],
    preview_chars: usize,
) -> Option<Rendered> {
    let mut out = String::new();
    let (mut full, mut marked) = (0usize, 0usize);
    for e in entries_at(home, thread_id) {
        let t = e.text.trim();
        if retained.iter().any(|r| r.contains(t)) {
            continue;
        }
        let len = e.text.chars().count();
        if len <= preview_chars {
            out.push_str(&format!("\n[{}] {}\n", e.seq, e.text));
            full += 1;
        } else {
            let path = full_text_file(home, thread_id, e.seq);
            if !path.exists() {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(&path, &e.text);
            }
            out.push_str(&format!(
                "\n[{}] {}\n... ({} chars in total; whole message saved at {})\n",
                e.seq,
                char_prefix(&e.text, preview_chars),
                len,
                path.display()
            ));
            marked += 1;
        }
    }
    if full + marked == 0 {
        return None;
    }
    Some(Rendered {
        text: format!("{HEADER}{out}"),
        full,
        marked,
    })
}

pub fn render(thread_id: &str, retained: &[String], preview_chars: usize) -> Option<Rendered> {
    render_at(&inocode_home(), thread_id, retained, preview_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_verbatim_in_order_and_skips_blank() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(record_at(d.path(), "t1", "first\n  keep spaces "), Some(1));
        assert_eq!(record_at(d.path(), "t1", "   "), None);
        assert_eq!(record_at(d.path(), "t1", "second"), Some(2));
        let e = entries_at(d.path(), "t1");
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].text, "first\n  keep spaces ");
        assert!(entries_at(d.path(), "other").is_empty());
    }

    #[test]
    fn short_in_full_long_as_preview_with_whole_text_saved() {
        let d = tempfile::tempdir().unwrap();
        record_at(d.path(), "t", "do not push");
        let log: String = (0..5000).map(|i| format!("uart line {i}\n")).collect();
        record_at(d.path(), "t", &log);
        let r = render_at(d.path(), "t", &[], 100).unwrap();
        assert_eq!((r.full, r.marked), (1, 1));
        assert!(r.text.contains("[1] do not push"));
        assert!(r.text.contains("uart line 0"));
        assert!(!r.text.contains("uart line 4999"));
        let path = r
            .text
            .split("whole message saved at ")
            .nth(1)
            .unwrap()
            .trim_end_matches([')', '\n']);
        assert_eq!(std::fs::read_to_string(path).unwrap(), log);
        // The ledger file itself keeps the whole text too.
        assert_eq!(entries_at(d.path(), "t")[1].text, log);
    }

    #[test]
    fn retained_messages_are_not_repeated() {
        let d = tempfile::tempdir().unwrap();
        record_at(d.path(), "t", "keep this");
        record_at(d.path(), "t", "lost one");
        let r = render_at(d.path(), "t", &["keep this".to_string()], 100).unwrap();
        assert!(!r.text.contains("keep this") && r.text.contains("lost one"));
        assert!(render_at(d.path(), "t", &["keep this lost one".into()], 100).is_none());
    }
}
