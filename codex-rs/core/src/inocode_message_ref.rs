//! Resolves message references (`#<turn>.<item>` or an item id) against a rollout file without
//! loading the whole history.
//!
//! Numbering is the file's own: turn N is the Nth distinct `turn_id` in the order a turn start
//! first appears in the file. Lines copied by a fork repeat an existing `turn_id` and do not count
//! again; rolled-back turns keep their numbers. The item number is the position inside the turn,
//! counted by the same history builder that serves `thread/read`.
//!
//! A small append-only sidecar (`<rollout>.msgidx`) stores the byte offset at which each turn
//! starts and how far the file was scanned. It is created by one streaming pass and later only
//! extended. A resolution seeks to the turn and reads just that turn. Offsets address the original
//! JSONL bytes, so compressed (zstd) rollouts work through the seekable reader.

use codex_app_server_protocol::MessageRole;
use codex_app_server_protocol::MessageSelector;
use codex_app_server_protocol::ResolvedMessage;
use codex_app_server_protocol::build_turns_from_rollout_items;
use codex_app_server_protocol::resolve_message;
use codex_protocol::protocol::EventMsg;
use codex_rollout::RolloutItem;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashSet;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;
use std::path::PathBuf;

/// Where a turn starts in the decoded JSONL bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnSpan {
    /// 1-based, in order of first appearance.
    pub turn: usize,
    pub turn_id: String,
    pub start: u64,
}

/// The turn starts of a rollout and how far the file has been scanned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageIndex {
    pub spans: Vec<TurnSpan>,
    /// Bytes scanned so far (always at a line boundary).
    pub through: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum SidecarLine {
    Span(TurnSpan),
    Through { through: u64 },
}

fn sidecar_path(rollout: &Path) -> PathBuf {
    let mut name = codex_rollout::plain_rollout_path(rollout).into_os_string();
    name.push(".msgidx");
    PathBuf::from(name)
}

fn load_sidecar(path: &Path) -> MessageIndex {
    let Ok(file) = std::fs::File::open(path) else {
        return MessageIndex::default();
    };
    let mut index = MessageIndex::default();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        match serde_json::from_str::<SidecarLine>(&line) {
            Ok(SidecarLine::Span(span)) => {
                // A well-formed sidecar numbers turns 1, 2, 3...; anything else is stale.
                if span.turn != index.spans.len() + 1 {
                    return MessageIndex::default();
                }
                index.spans.push(span);
            }
            Ok(SidecarLine::Through { through }) => index.through = through,
            Err(_) => break, // a torn last line: the scan resumes from the last `through`
        }
    }
    index
}

fn append_sidecar(path: &Path, lines: &[SidecarLine]) {
    use std::io::Write;
    let mut text = String::new();
    for l in lines {
        if let Ok(s) = serde_json::to_string(l) {
            text.push_str(&s);
            text.push('\n');
        }
    }
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(text.as_bytes()));
}

/// `Some(turn_id)` when `line` is a turn start.
fn turn_start_id(line: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(line).ok()?;
    if !(text.contains("\"task_started\"") || text.contains("\"turn_started\"")) {
        return None;
    }
    match codex_rollout::parse_rollout_line(text).ok()?.item {
        RolloutItem::EventMsg(EventMsg::TurnStarted(started)) => Some(started.turn_id),
        _ => None,
    }
}

/// Scans `reader` from `index.through` and adds the turn starts found; returns the new lines for
/// the sidecar.
fn extend_index<R: Read + Seek>(
    reader: &mut R,
    index: &mut MessageIndex,
) -> std::io::Result<Vec<SidecarLine>> {
    reader.seek(SeekFrom::Start(index.through))?;
    let mut seen: HashSet<String> = index.spans.iter().map(|s| s.turn_id.clone()).collect();
    let mut reader = BufReader::with_capacity(1 << 20, reader);
    let mut pos = index.through;
    let mut buf = Vec::new();
    let mut added = Vec::new();
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 || buf.last() != Some(&b'\n') {
            break; // end of file, or a line still being written
        }
        if let Some(turn_id) = turn_start_id(&buf)
            && seen.insert(turn_id.clone())
        {
            let span = TurnSpan {
                turn: index.spans.len() + 1,
                turn_id,
                start: pos,
            };
            added.push(SidecarLine::Span(span.clone()));
            index.spans.push(span);
        }
        pos += n as u64;
    }
    if pos != index.through {
        index.through = pos;
        added.push(SidecarLine::Through { through: pos });
    }
    Ok(added)
}

/// The index of `rollout`, built or extended on demand.
pub fn load_index(rollout: &Path) -> std::io::Result<MessageIndex> {
    let sidecar = sidecar_path(rollout);
    let mut reader = codex_rollout::open_rollout_seekable_reader(rollout, || false)?;
    let mut index = load_sidecar(&sidecar);
    let len = reader.seek(SeekFrom::End(0))?;
    if index.through > len {
        // The file is shorter than the index remembers (rewritten): start again.
        index = MessageIndex::default();
        let _ = std::fs::remove_file(&sidecar);
    }
    let added = extend_index(&mut reader, &mut index)?;
    append_sidecar(&sidecar, &added);
    Ok(index)
}

/// Reads the lines of one turn: from its start to the start of the next one (or the end).
fn read_turn<R: Read + Seek>(
    reader: &mut R,
    index: &MessageIndex,
    turn: usize,
) -> std::io::Result<Vec<RolloutItem>> {
    let span = &index.spans[turn - 1];
    let end = index
        .spans
        .get(turn)
        .map_or(index.through, |next| next.start);
    reader.seek(SeekFrom::Start(span.start))?;
    let mut bytes = Vec::new();
    reader
        .take(end.saturating_sub(span.start))
        .read_to_end(&mut bytes)?;
    Ok(bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
        .filter_map(|l| codex_rollout::parse_rollout_line_bytes(l).ok())
        .map(|line| line.item)
        .collect())
}

fn resolve_in_turn(
    items: &[RolloutItem],
    span: &TurnSpan,
    selector: &MessageSelector,
) -> Option<ResolvedMessage> {
    let mut turns = build_turns_from_rollout_items(items);
    if turns.iter().any(|t| t.id == span.turn_id) {
        turns.retain(|t| t.id == span.turn_id);
    }
    let local = match selector {
        MessageSelector::Position { item, .. } => MessageSelector::Position {
            turn: 1,
            item: *item,
        },
        other => other.clone(),
    };
    // Only the first turn of the slice is the one named; items of the others are not this turn's.
    turns.truncate(1);
    let mut found = resolve_message(&turns, &local)?;
    found.turn_number = span.turn;
    Some(found)
}

/// An agent-to-agent message received in a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailEntry {
    /// `#<turn>.@<n>`.
    pub id: String,
    pub author: String,
    pub recipient: String,
    pub content: String,
}

fn mail_of(items: &[RolloutItem]) -> Vec<(String, String, String)> {
    items
        .iter()
        .filter_map(|i| match i {
            RolloutItem::InterAgentCommunication(c) => Some((
                c.author.to_string(),
                c.recipient.to_string(),
                c.content.clone(),
            )),
            _ => None,
        })
        .collect()
}

/// Every agent-to-agent message in the rollout with its id, oldest first. Blocking; reads one
/// turn at a time.
pub fn mail_in_rollout(rollout: &Path) -> std::io::Result<Vec<MailEntry>> {
    let index = load_index(rollout)?;
    let mut reader = codex_rollout::open_rollout_seekable_reader(rollout, || false)?;
    let mut out = Vec::new();
    for span in &index.spans {
        let items = read_turn(&mut reader, &index, span.turn)?;
        for (n, (author, recipient, content)) in mail_of(&items).into_iter().enumerate() {
            out.push(MailEntry {
                id: format!("#{}.@{}", span.turn, n + 1),
                author,
                recipient,
                content,
            });
        }
    }
    Ok(out)
}

fn resolve_mail(items: &[RolloutItem], span: &TurnSpan, n: usize) -> Option<ResolvedMessage> {
    let (author, recipient, content) = mail_of(items).into_iter().nth(n.checked_sub(1)?)?;
    Some(ResolvedMessage {
        turn_number: span.turn,
        item_number: n,
        turn_id: span.turn_id.clone(),
        item_id: format!("mail-{}.{n}", span.turn),
        role: MessageRole::Agent,
        text: content,
        from: Some(author),
        to: Some(recipient),
    })
}

/// The file-wide number of the turn with `turn_id`. Blocking.
pub fn turn_number_of(rollout: &Path, turn_id: &str) -> std::io::Result<Option<usize>> {
    Ok(load_index(rollout)?
        .spans
        .iter()
        .find(|s| s.turn_id == turn_id)
        .map(|s| s.turn))
}

/// True when the lines read at a span start are not that turn's start: the sidecar belongs to an
/// older version of the file (it was replaced or rewritten).
fn is_stale(items: &[RolloutItem], span: &TurnSpan) -> bool {
    !matches!(
        items.first(),
        Some(RolloutItem::EventMsg(EventMsg::TurnStarted(s))) if s.turn_id == span.turn_id
    )
}

/// Resolves `selector` in the rollout at `rollout`. Blocking; call from `spawn_blocking`.
/// A sidecar that no longer matches the file is thrown away and rebuilt once.
pub fn resolve_in_rollout(
    rollout: &Path,
    selector: &MessageSelector,
) -> std::io::Result<Option<(ResolvedMessage, usize)>> {
    match resolve_once(rollout, selector)? {
        Resolution::Done(found) => Ok(found),
        Resolution::Stale => {
            let _ = std::fs::remove_file(sidecar_path(rollout));
            match resolve_once(rollout, selector)? {
                Resolution::Done(found) => Ok(found),
                Resolution::Stale => Ok(None),
            }
        }
    }
}

enum Resolution {
    Done(Option<(ResolvedMessage, usize)>),
    Stale,
}

fn resolve_once(rollout: &Path, selector: &MessageSelector) -> std::io::Result<Resolution> {
    let index = load_index(rollout)?;
    let mut reader = codex_rollout::open_rollout_seekable_reader(rollout, || false)?;
    let total = index.spans.len();
    match selector {
        MessageSelector::Position { turn, .. } => {
            if *turn == 0 || *turn > total {
                return Ok(Resolution::Done(None));
            }
            let items = read_turn(&mut reader, &index, *turn)?;
            if is_stale(&items, &index.spans[turn - 1]) {
                return Ok(Resolution::Stale);
            }
            Ok(Resolution::Done(
                resolve_in_turn(&items, &index.spans[turn - 1], selector).map(|m| (m, total)),
            ))
        }
        MessageSelector::Mail { turn, n } => {
            if *turn == 0 || *turn > total {
                return Ok(Resolution::Done(None));
            }
            let items = read_turn(&mut reader, &index, *turn)?;
            if is_stale(&items, &index.spans[turn - 1]) {
                return Ok(Resolution::Stale);
            }
            Ok(Resolution::Done(
                resolve_mail(&items, &index.spans[turn - 1], *n).map(|m| (m, total)),
            ))
        }
        MessageSelector::ItemId(id) => {
            for span in &index.spans {
                let items = read_turn(&mut reader, &index, span.turn)?;
                if is_stale(&items, span) {
                    return Ok(Resolution::Stale);
                }
                // Cheap filter before building: the id must appear in the turn's raw lines.
                let mentions = items
                    .iter()
                    .any(|i| serde_json::to_string(i).is_ok_and(|s| s.contains(id.as_str())));
                if !mentions {
                    continue;
                }
                if let Some(found) = resolve_in_turn(&items, span, selector) {
                    return Ok(Resolution::Done(Some((found, total))));
                }
            }
            Ok(Resolution::Done(None))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::parse_message_ref;

    fn line(ordinal: u64, payload: serde_json::Value) -> String {
        format!(
            "{}\n",
            serde_json::json!({
                "timestamp": "2026-01-01T00:00:00.000Z",
                "ordinal": ordinal,
                "type": "event_msg",
                "payload": payload,
            })
        )
    }

    fn started(ordinal: u64, turn_id: &str) -> String {
        line(
            ordinal,
            serde_json::json!({"type": "task_started", "turn_id": turn_id,
                "model_context_window": 1000, "collaboration_mode_kind": "default"}),
        )
    }

    fn user(ordinal: u64, text: &str) -> String {
        line(
            ordinal,
            serde_json::json!({"type": "user_message", "message": text, "images": null,
                "local_images": [], "text_elements": []}),
        )
    }

    fn agent(ordinal: u64, text: &str) -> String {
        line(
            ordinal,
            serde_json::json!({"type": "agent_message", "message": text}),
        )
    }

    fn rollout() -> String {
        [
            started(1, "turn-a"),
            user(2, "First question"),
            agent(3, "First answer"),
            started(4, "turn-b"),
            user(5, "Second question"),
            agent(6, "Second answer"),
            // A fork copies the first turn's start again: it must not be counted twice.
            started(7, "turn-a"),
            started(8, "turn-c"),
            user(9, "Third question"),
            agent(10, "Third answer"),
        ]
        .concat()
    }

    fn text_of(path: &Path, reference: &str) -> Option<String> {
        resolve_in_rollout(path, &parse_message_ref(reference))
            .unwrap()
            .map(|(m, _)| m.text)
    }

    #[test]
    fn numbers_are_file_wide_and_forked_copies_are_not_counted_twice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-x.jsonl");
        std::fs::write(&path, rollout()).unwrap();
        let index = load_index(&path).unwrap();
        let ids: Vec<&str> = index.spans.iter().map(|s| s.turn_id.as_str()).collect();
        assert_eq!(ids, ["turn-a", "turn-b", "turn-c"]);
        assert_eq!(text_of(&path, "#1.2").as_deref(), Some("First answer"));
        assert_eq!(text_of(&path, "#2.1").as_deref(), Some("Second question"));
        assert_eq!(text_of(&path, "#3.2").as_deref(), Some("Third answer"));
        assert_eq!(text_of(&path, "#4.1"), None);
        assert_eq!(text_of(&path, "#1.9"), None);
    }

    #[test]
    fn the_sidecar_is_extended_not_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-x.jsonl");
        let first: String = rollout().lines().take(6).map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, &first).unwrap();
        assert_eq!(load_index(&path).unwrap().spans.len(), 2);
        let sidecar = std::fs::read_to_string(sidecar_path(&path)).unwrap();
        assert_eq!(load_index(&path).unwrap().spans.len(), 2);
        assert_eq!(std::fs::read_to_string(sidecar_path(&path)).unwrap(), sidecar);
        // The conversation goes on: only the new part is scanned and appended.
        std::fs::write(&path, rollout()).unwrap();
        let grown = load_index(&path).unwrap();
        assert_eq!(grown.spans.len(), 3);
        assert_eq!(grown.spans[2].turn_id, "turn-c");
        let after = std::fs::read_to_string(sidecar_path(&path)).unwrap();
        assert!(after.starts_with(&sidecar));
        assert_eq!(text_of(&path, "#3.1").as_deref(), Some("Third question"));
    }

    #[test]
    fn a_half_written_last_line_is_left_for_later() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-x.jsonl");
        let mut body = rollout();
        body.push_str("{\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-d\"");
        std::fs::write(&path, &body).unwrap();
        assert_eq!(load_index(&path).unwrap().spans.len(), 3);
    }

    #[test]
    fn a_compressed_rollout_reads_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("rollout-x.jsonl");
        let packed = zstd::encode_all(rollout().as_bytes(), 3).unwrap();
        std::fs::write(dir.path().join("rollout-x.jsonl.zst"), packed).unwrap();
        assert!(!plain.exists());
        assert_eq!(text_of(&plain, "#2.2").as_deref(), Some("Second answer"));
        assert_eq!(text_of(&plain, "#3.1").as_deref(), Some("Third question"));
    }

    #[test]
    fn an_item_id_found_in_a_turn_resolves_with_the_file_wide_turn_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-x.jsonl");
        std::fs::write(&path, rollout()).unwrap();
        let by_pos = resolve_in_rollout(&path, &parse_message_ref("#3.2"))
            .unwrap()
            .unwrap()
            .0;
        // Generated ids ("item-N") are only unique inside one built turn: reading the same
        // position again gives the same id, and a position is always the stable reference.
        let again = resolve_in_rollout(&path, &parse_message_ref("#3.2"))
            .unwrap()
            .unwrap()
            .0;
        assert_eq!(by_pos, again);
        assert_eq!(by_pos.turn_number, 3);
    }

    #[test]
    fn a_replaced_rollout_makes_the_sidecar_stale_and_it_is_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-x.jsonl");
        std::fs::write(&path, rollout()).unwrap();
        assert_eq!(text_of(&path, "#2.2").as_deref(), Some("Second answer"));
        // Another, longer history takes the file's place; the old offsets are wrong now.
        let other = [
            started(1, "other-a"),
            user(2, "padding padding padding padding padding padding padding padding"),
            agent(3, "padding padding padding padding padding padding padding padding"),
            started(4, "other-b"),
            user(5, "Other question"),
            agent(6, "Other answer"),
            started(7, "other-c"),
            user(8, "More"),
            agent(9, "More still"),
        ]
        .concat();
        assert!(other.len() > rollout().len());
        std::fs::write(&path, other).unwrap();
        assert_eq!(text_of(&path, "#2.2").as_deref(), Some("Other answer"));
        assert_eq!(turn_number_of(&path, "other-c").unwrap(), Some(3));
        assert_eq!(turn_number_of(&path, "turn-b").unwrap(), None);
    }

    fn mail_line(content: &str) -> String {
        use codex_protocol::AgentPath;
        use codex_protocol::protocol::InterAgentCommunication;
        let line = codex_rollout::RolloutLine {
            timestamp: "2026-01-01T00:00:00.000Z".into(),
            ordinal: None,
            item: RolloutItem::InterAgentCommunication(InterAgentCommunication::new(
                AgentPath::try_from("/root/a").unwrap(),
                AgentPath::try_from("/root/b").unwrap(),
                Vec::new(),
                content.into(),
                false,
            )),
        };
        format!("{}
", serde_json::to_string(&line).unwrap())
    }

    #[test]
    fn agent_messages_get_turn_wide_ids_and_can_be_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-x.jsonl");
        let body = [
            started(1, "turn-a"),
            user(2, "Hi"),
            mail_line("ASK
q: one
conf: low"),
            mail_line("ANSWER
a: two
conf: low"),
            agent(3, "Hello"),
            started(4, "turn-b"),
            mail_line("RESULT
status: done
summary: three
conf: high"),
        ]
        .concat();
        std::fs::write(&path, body).unwrap();
        let mail = mail_in_rollout(&path).unwrap();
        let ids: Vec<&str> = mail.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["#1.@1", "#1.@2", "#2.@1"]);
        assert_eq!(mail[0].author, "/root/a");
        let (found, _) = resolve_in_rollout(&path, &parse_message_ref("#1.@2"))
            .unwrap()
            .unwrap();
        assert_eq!(found.role, MessageRole::Agent);
        assert_eq!(found.text, "ANSWER
a: two
conf: low");
        assert_eq!(found.from.as_deref(), Some("/root/a"));
        assert_eq!(found.to.as_deref(), Some("/root/b"));
        assert!(resolve_in_rollout(&path, &parse_message_ref("#1.@3")).unwrap().is_none());
        // The ordinary numbering of the turn is unchanged by the mail.
        assert_eq!(text_of(&path, "#1.2").as_deref(), Some("Hello"));
    }
}
