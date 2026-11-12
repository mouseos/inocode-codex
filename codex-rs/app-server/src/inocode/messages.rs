//! `inocode/messages/list` and the `inocode/messages/appended` notification: the verbatim
//! agent-to-agent log of a conversation (`<INOCODE_HOME>/messages/<root thread>.jsonl`), with
//! telegraphic messages broken into kind, confidence and fields.

use super::InocodeResult;
use super::unknown_method;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeMessage;
use codex_app_server_protocol::InocodeMessageConfidence;
use codex_app_server_protocol::InocodeMessageField;
use codex_app_server_protocol::InocodeMessageKind;
use codex_app_server_protocol::InocodeMessagesAppendedNotification;
use codex_app_server_protocol::InocodeMessagesListParams;
use codex_app_server_protocol::InocodeMessageRole;
use codex_app_server_protocol::InocodeMessagesListResponse;
use codex_app_server_protocol::InocodeMessagesResolveParams;
use codex_app_server_protocol::InocodeMessagesResolveResponse;
use codex_app_server_protocol::InocodeParsedMessage;
use codex_inocode_protocol::Confidence;
use codex_inocode_protocol::Kind;
use codex_inocode_protocol::LoggedMessage;
use std::path::Path;

const DEFAULT_LIMIT: usize = 200;

pub(crate) fn to_message(root_thread_id: &str, m: &LoggedMessage) -> InocodeMessage {
    let parsed = codex_inocode_protocol::parse_view(&m.text).map(|v| InocodeParsedMessage {
        kind: match v.kind {
            Kind::Ask => InocodeMessageKind::Ask,
            Kind::Answer => InocodeMessageKind::Answer,
            Kind::Blocked => InocodeMessageKind::Blocked,
            Kind::Result => InocodeMessageKind::Result,
            Kind::Handoff => InocodeMessageKind::Handoff,
        },
        conf: match v.conf {
            Confidence::High => InocodeMessageConfidence::High,
            Confidence::Med => InocodeMessageConfidence::Med,
            Confidence::Low => InocodeMessageConfidence::Low,
        },
        fields: v
            .fields
            .into_iter()
            .map(|(key, value)| InocodeMessageField { key, value })
            .collect(),
    });
    InocodeMessage {
        ts_ms: i64::try_from(m.ts_ms).unwrap_or(i64::MAX),
        thread_id: root_thread_id.to_string(),
        from: m.from.clone(),
        to: m.to.clone(),
        delivery: m.delivery.clone(),
        text: m.text.clone(),
        summary: summary_of(&m.text, parsed.as_ref()),
        parsed,
        id: None,
        reply_to: m.reply_to.clone(),
        reply_thread: m.reply_thread.clone(),
        from_thread: m.from_thread.clone(),
        to_thread: m.to_thread.clone(),
    }
}

/// One line for a list view.
fn summary_of(text: &str, parsed: Option<&InocodeParsedMessage>) -> String {
    let from_fields = parsed.and_then(|p| {
        ["summary", "q", "a", "reason", "task"]
            .iter()
            .find_map(|key| p.fields.iter().find(|f| f.key == *key))
            .map(|f| f.value.clone())
    });
    let line = from_fields
        .or_else(|| text.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_string))
        .unwrap_or_default();
    let line = line.lines().next().unwrap_or_default();
    line.chars().take(120).collect()
}

pub(crate) fn list_in(
    home: &Path,
    params: &InocodeMessagesListParams,
) -> InocodeMessagesListResponse {
    let root = codex_inocode_protocol::root_of(&params.thread_id);
    let mut logged = codex_inocode_protocol::read_log(home, &root);
    if root != params.thread_id {
        // A child id was passed: its own log (older versions keyed by sender) is included too.
        logged.extend(codex_inocode_protocol::read_log(home, &params.thread_id));
    }
    if let Some(since) = params.since_ts_ms {
        let since = u64::try_from(since).unwrap_or(0);
        logged.retain(|m| m.ts_ms >= since);
    }
    logged.sort_by_key(|m| m.ts_ms);
    let limit = if params.limit == 0 {
        DEFAULT_LIMIT
    } else {
        params.limit as usize
    };
    let offset = params
        .cursor
        .as_deref()
        .and_then(|c| c.parse::<usize>().ok())
        .unwrap_or(0);
    let messages: Vec<InocodeMessage> = logged
        .iter()
        .skip(offset)
        .take(limit)
        .map(|m| to_message(&params.thread_id, m))
        .collect();
    let next_cursor =
        (offset + messages.len() < logged.len()).then(|| (offset + messages.len()).to_string());
    InocodeMessagesListResponse {
        messages,
        next_cursor,
    }
}

/// Forwards newly logged messages as `inocode/messages/appended`. Call once at startup.
pub(crate) fn spawn_notifier(
    outgoing: std::sync::Arc<crate::outgoing_message::OutgoingMessageSender>,
) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let mut rx = codex_inocode_protocol::subscribe_messages();
    handle.spawn(async move {
        loop {
            match rx.recv().await {
                Ok((root, message)) => {
                    outgoing
                        .send_server_notification(
                            codex_app_server_protocol::ServerNotification::InocodeMessagesAppended(
                                InocodeMessagesAppendedNotification {
                                    message: to_message(&root, &message),
                                    thread_id: root,
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

fn response_of(
    thread_id: &str,
    found: codex_app_server_protocol::ResolvedMessage,
    max_chars: Option<i64>,
) -> InocodeMessagesResolveResponse {
    let limit = max_chars.and_then(|n| usize::try_from(n).ok());
    let (text, truncated) = match limit {
        Some(n) if found.text.chars().count() > n => (found.text.chars().take(n).collect(), true),
        _ => (found.text, false),
    };
    InocodeMessagesResolveResponse {
        thread_id: thread_id.to_string(),
        turn_number: i64::try_from(found.turn_number).unwrap_or(i64::MAX),
        item_number: i64::try_from(found.item_number).unwrap_or(i64::MAX),
        turn_id: found.turn_id,
        item_id: found.item_id,
        role: match found.role {
            codex_app_server_protocol::MessageRole::User => InocodeMessageRole::User,
            codex_app_server_protocol::MessageRole::Assistant => InocodeMessageRole::Assistant,
            codex_app_server_protocol::MessageRole::Tool => InocodeMessageRole::Tool,
            codex_app_server_protocol::MessageRole::Agent => InocodeMessageRole::Agent,
        },
        text,
        truncated,
        from: found.from,
        to: found.to,
    }
}

/// Finds the message `params.id` names in the rollout at `path` (indexed, one turn read).
pub(crate) async fn resolve_at(
    path: std::path::PathBuf,
    params: &InocodeMessagesResolveParams,
) -> Result<InocodeMessagesResolveResponse, JSONRPCErrorError> {
    let (id, turn_id, item_index) = (
        params.id.clone().filter(|i| !i.trim().is_empty()),
        params.turn_id.clone(),
        params.item_index,
    );
    let described = id
        .clone()
        .or_else(|| turn_id.clone().map(|t| format!("turn {t}")))
        .unwrap_or_default();
    let looked_up = tokio::task::spawn_blocking(move || {
        let selector = match (id, turn_id, item_index) {
            (Some(id), _, _) => codex_app_server_protocol::parse_message_ref(&id),
            (None, Some(turn_id), Some(index)) => {
                let Some(turn) = codex_core::inocode_message_ref::turn_number_of(&path, &turn_id)?
                else {
                    return Ok(None);
                };
                codex_app_server_protocol::MessageSelector::Position {
                    turn,
                    item: usize::try_from(index).unwrap_or(0) + 1,
                }
            }
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "give `id`, or `turnId` with `itemIndex`",
                ));
            }
        };
        codex_core::inocode_message_ref::resolve_in_rollout(&path, &selector)
    })
    .await
    .map_err(|err| internal_error(format!("failed to read the history: {err}")))?
    .map_err(|err| {
        if err.kind() == std::io::ErrorKind::InvalidInput {
            invalid_params(err.to_string())
        } else {
            internal_error(format!("failed to read the history: {err}"))
        }
    })?;
    let (found, _) = looked_up
        .ok_or_else(|| invalid_params(format!("no message found for {described}")))?;
    Ok(response_of(&params.thread_id, found, params.max_chars))
}

async fn resolve(
    params: InocodeMessagesResolveParams,
) -> Result<InocodeMessagesResolveResponse, JSONRPCErrorError> {
    let codex_home = codex_core::config::find_codex_home()
        .map_err(|err| internal_error(format!("failed to find the Codex home: {err}")))?;
    let path = codex_rollout::find_thread_path_by_id_str(&codex_home, &params.thread_id, None)
        .await
        .map_err(|err| {
            internal_error(format!("failed to locate thread {}: {err}", params.thread_id))
        })?
        .ok_or_else(|| invalid_params(format!("thread not found: {}", params.thread_id)))?;
    resolve_at(path, &params).await
}

/// Gives each message the id it has in the receiver's history, matching it to the agent message
/// recorded there (same sender, same text; the n-th such message goes to the n-th match).
pub(crate) fn assign_ids(
    messages: &mut [InocodeMessage],
    mail_by_thread: &std::collections::HashMap<String, Vec<codex_core::inocode_message_ref::MailEntry>>,
) {
    let mut used: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for m in messages.iter_mut() {
        let Some(thread) = m.to_thread.clone() else {
            continue;
        };
        let Some(entries) = mail_by_thread.get(&thread) else {
            continue;
        };
        let wanted = m.text.trim();
        let hit = entries.iter().find(|e| {
            !used.contains(&(thread.clone(), e.id.clone()))
                && e.author == m.from
                && (e.content.trim() == wanted || e.content.contains(wanted))
        });
        if let Some(entry) = hit {
            used.insert((thread, entry.id.clone()));
            m.id = Some(entry.id.clone());
        }
    }
}

/// Reads the receivers' histories (one rollout per receiving thread) and sets the ids.
async fn with_ids(mut response: InocodeMessagesListResponse) -> InocodeMessagesListResponse {
    let Ok(codex_home) = codex_core::config::find_codex_home() else {
        return response;
    };
    let mut mail = std::collections::HashMap::new();
    let threads: std::collections::HashSet<String> = response
        .messages
        .iter()
        .filter_map(|m| m.to_thread.clone())
        .collect();
    for thread in threads {
        let Ok(Some(path)) =
            codex_rollout::find_thread_path_by_id_str(&codex_home, &thread, None).await
        else {
            continue;
        };
        if let Ok(Ok(entries)) = tokio::task::spawn_blocking(move || {
            codex_core::inocode_message_ref::mail_in_rollout(&path)
        })
        .await
        {
            mail.insert(thread, entries);
        }
    }
    assign_ids(&mut response.messages, &mail);
    response
}

const QUOTE_DEFAULT_CHARS: usize = 4000;

async fn rollout_of(thread_id: &str) -> Option<std::path::PathBuf> {
    let codex_home = codex_core::config::find_codex_home().ok()?;
    codex_rollout::find_thread_path_by_id_str(&codex_home, thread_id, None)
        .await
        .ok()
        .flatten()
}

async fn cwd_of(path: &std::path::Path) -> Option<std::path::PathBuf> {
    codex_rollout::read_session_meta_line(path)
        .await
        .ok()
        .map(|line| line.meta.cwd.to_path_buf())
}

fn state_only(message_ref: codex_app_server_protocol::InocodeMessageRef, state: &str) -> codex_app_server_protocol::InocodeResolvedMessageRef {
    codex_app_server_protocol::InocodeResolvedMessageRef {
        message_ref,
        state: state.to_string(),
        turn_number: None,
        item_number: None,
        turn_id: None,
        item_id: None,
        role: None,
        text: None,
        truncated: false,
        total_chars: None,
        range: None,
    }
}

/// Resolves many pointers at once. A pointer that cannot be read is a state, not an error.
async fn resolve_many(
    params: codex_app_server_protocol::InocodeMessagesResolveManyParams,
) -> codex_app_server_protocol::InocodeMessagesResolveManyResponse {
    let limit = params
        .max_chars
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(QUOTE_DEFAULT_CHARS);
    let caller_cwd = match &params.caller_thread_id {
        Some(t) => match rollout_of(t).await {
            Some(p) => cwd_of(&p).await,
            None => None,
        },
        None => None,
    };
    let mut items = Vec::new();
    for r in params.refs {
        let Some(thread) = r.thread_id.clone().or_else(|| params.caller_thread_id.clone()) else {
            items.push(state_only(r, "notFound"));
            continue;
        };
        let Some(path) = rollout_of(&thread).await else {
            items.push(state_only(r, "notFound"));
            continue;
        };
        // another workplace: only what the user attached as a card
        let other_thread = params.caller_thread_id.as_deref() != Some(thread.as_str());
        if other_thread && !r.user_attached {
            let same = match (&caller_cwd, cwd_of(&path).await) {
                (Some(a), Some(b)) => *a == b,
                _ => false,
            };
            if !same {
                items.push(state_only(r, "outOfScope"));
                continue;
            }
        }
        let resolve_params = InocodeMessagesResolveParams {
            thread_id: thread.clone(),
            id: Some(r.id.clone()),
            turn_id: None,
            item_index: None,
            max_chars: None,
        };
        let Ok(found) = resolve_at(path, &resolve_params).await else {
            items.push(state_only(r, "notFound"));
            continue;
        };
        let chars: Vec<char> = found.text.chars().collect();
        let (start, end) = match r.range.as_deref() {
            Some([s, e]) if *s >= 0 && *e >= *s => (
                usize::try_from(*s).unwrap_or(0).min(chars.len()),
                usize::try_from(*e).unwrap_or(0).min(chars.len()),
            ),
            _ => (0, chars.len()),
        };
        let selected = &chars[start..end];
        let total = selected.len();
        let truncated = total > limit;
        let text: String = selected.iter().take(limit).collect();
        items.push(codex_app_server_protocol::InocodeResolvedMessageRef {
            range: r.range.clone().filter(|_| selected.len() != chars.len() || start != 0),
            message_ref: r,
            state: "ok".to_string(),
            turn_number: Some(found.turn_number),
            item_number: Some(found.item_number),
            turn_id: Some(found.turn_id),
            item_id: Some(found.item_id),
            role: Some(found.role),
            text: Some(text),
            truncated,
            total_chars: i64::try_from(total).ok(),
        });
    }
    codex_app_server_protocol::InocodeMessagesResolveManyResponse { items }
}

/// `[引用 #12.3 thread:<id> 40-95]`: a pointer to a quoted message (the form a card takes in the
/// user's message). Returns the reference, or None for any other line.
pub(crate) fn parse_quote_line(line: &str) -> Option<codex_app_server_protocol::InocodeMessageRef> {
    let inner = line.trim().strip_prefix("[引用")?.strip_suffix(']')?;
    let mut id = None;
    let mut thread_id = None;
    let mut range = None;
    for token in inner.split_whitespace() {
        if token.starts_with('#') {
            id = Some(token.to_string());
        } else if let Some(t) = token.strip_prefix("thread:") {
            thread_id = Some(t.to_string());
        } else if let Some((a, b)) = token.split_once('-')
            && let (Ok(a), Ok(b)) = (a.parse::<i64>(), b.parse::<i64>())
        {
            range = Some(vec![a, b]);
        }
    }
    Some(codex_app_server_protocol::InocodeMessageRef {
        thread_id,
        id: id?,
        range,
        // a card the user put in the message: readable even from another workplace
        user_attached: true,
    })
}

/// The originals of the quotes in a turn's input, for the model: one untrusted context entry per
/// pointer (a quote is information, not an instruction). The user's message keeps only the
/// pointers. A pointer that cannot be read says so; nothing stops.
pub(crate) async fn quote_context(
    thread_id: &str,
    input: &[codex_app_server_protocol::UserInput],
) -> Vec<(String, String)> {
    let refs: Vec<_> = input
        .iter()
        .filter_map(|item| match item {
            codex_app_server_protocol::UserInput::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .flat_map(str::lines)
        .filter_map(parse_quote_line)
        .collect();
    if refs.is_empty() {
        return Vec::new();
    }
    let resolved = resolve_many(codex_app_server_protocol::InocodeMessagesResolveManyParams {
        caller_thread_id: Some(thread_id.to_string()),
        refs,
        max_chars: None,
    })
    .await;
    resolved
        .items
        .into_iter()
        .enumerate()
        .map(|(n, item)| {
            let r = &item.message_ref;
            let mut head = format!("[引用 {}", r.id);
            if let Some(t) = &r.thread_id {
                head.push_str(&format!(" thread:{t}"));
            }
            if let Some(range) = &r.range
                && let [a, b] = range.as_slice()
            {
                head.push_str(&format!(" {a}-{b}"));
            }
            let body = match item.state.as_str() {
                "ok" => {
                    let who = match item.role {
                        Some(InocodeMessageRole::User) => " 自分",
                        Some(InocodeMessageRole::Assistant) => " AI",
                        _ => "",
                    };
                    let mut text = item.text.clone().unwrap_or_default();
                    if item.truncated {
                        text.push_str("\n(…上限で切れています。続きは read_message で読めます)");
                    }
                    format!("{head}{who}]\n{text}")
                }
                "outOfScope" => format!("{head}] (別の作業場のメッセージです。読めません)"),
                _ => format!("{head}] (メッセージが見つかりません)"),
            };
            (format!("quote-{}", n + 1), body)
        })
        .collect()
}

pub(crate) async fn handle(request: ClientRequest) -> InocodeResult {
    match request {
        ClientRequest::InocodeMessagesResolveMany { params, .. } => Ok(Some(resolve_many(params).await.into())),
        ClientRequest::InocodeMessagesResolve { params, .. } => Ok(Some(resolve(params).await?.into())),
        ClientRequest::InocodeMessagesList { params, .. } => Ok(Some(
            with_ids(list_in(&codex_inocode_flags::inocode_home(), &params))
                .await
                .into(),
        )),
        other => Err(unknown_method(other.method_name())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_pointer_lines_are_read_with_thread_and_range() {
        let full = parse_quote_line("[引用 #1.2 thread:abc-123 40-95]").unwrap();
        assert_eq!(full.id, "#1.2");
        assert_eq!(full.thread_id.as_deref(), Some("abc-123"));
        assert_eq!(full.range, Some(vec![40, 95]));
        assert!(full.user_attached, "a card the user put in the message is attached");
        let plain = parse_quote_line("  [引用 #12.3]  ").unwrap();
        assert_eq!((plain.id.as_str(), plain.thread_id, plain.range), ("#12.3", None, None));
        // not a pointer: ordinary text, a reply line, a pointer without an id
        assert!(parse_quote_line("引用 #1.2").is_none());
        assert!(parse_quote_line("[返信 #1.2「hi」]").is_none());
        assert!(parse_quote_line("[引用 thread:x]").is_none());
        assert!(parse_quote_line("この 2 つを踏まえて直して").is_none());
    }

    fn logged(ts: u64, delivery: &str, text: &str) -> LoggedMessage {
        LoggedMessage {
            ts_ms: ts,
            from: "/root/a".into(),
            to: "/root/b".into(),
            delivery: delivery.into(),
            text: text.into(),
            ..Default::default()
        }
    }

    #[test]
    fn list_keeps_text_verbatim_and_parses_telegraphic_messages() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tree_root = "msg-root-1";
        codex_inocode_protocol::link_thread("msg-child-1", tree_root);
        let ask = "ASK\nq: which crate owns Foo?\nconf: med";
        for (ts, d, t) in [
            (30, "final", "child finished"),
            (10, "spawn", "Port wait.rs"),
            (20, "send_message", ask),
        ] {
            codex_inocode_protocol::log_message(dir.path(), tree_root, &logged(ts, d, t))
                .expect("log");
        }
        let res = list_in(
            dir.path(),
            &InocodeMessagesListParams {
                thread_id: tree_root.into(),
                since_ts_ms: None,
                limit: 10,
                cursor: None,
            },
        );
        let deliveries: Vec<&str> = res.messages.iter().map(|m| m.delivery.as_str()).collect();
        assert_eq!(deliveries, ["spawn", "send_message", "final"]);
        let ask_msg = &res.messages[1];
        assert_eq!(ask_msg.text, ask);
        let parsed = ask_msg.parsed.as_ref().expect("parsed");
        assert_eq!(parsed.kind, InocodeMessageKind::Ask);
        assert_eq!(parsed.conf, InocodeMessageConfidence::Med);
        assert_eq!(parsed.fields[0].key, "q");
        assert!(res.messages[0].parsed.is_none());
        // Asking with a child id reads the root's conversation.
        let via_child = list_in(
            dir.path(),
            &InocodeMessagesListParams {
                thread_id: "msg-child-1".into(),
                since_ts_ms: Some(20),
                limit: 1,
                cursor: None,
            },
        );
        assert_eq!(via_child.messages.len(), 1);
        assert_eq!(via_child.next_cursor.as_deref(), Some("1"));
    }

    #[test]
    fn a_reply_keeps_its_target_threads_and_a_one_line_summary() {
        let mut m = logged(
            9,
            "send_message",
            "ANSWER
a: use WAL
reply_to: #4.2
reply_thread: thr-a
conf: high",
        );
        m.reply_to = Some("#4.2".into());
        m.reply_thread = Some("thr-a".into());
        m.from_thread = Some("thr-a".into());
        m.to_thread = Some("thr-b".into());
        let out = to_message("root", &m);
        assert_eq!(out.reply_to.as_deref(), Some("#4.2"));
        assert_eq!(out.reply_thread.as_deref(), Some("thr-a"));
        assert_eq!(out.to_thread.as_deref(), Some("thr-b"));
        assert_eq!(out.summary, "use WAL");
        let free = to_message("root", &logged(10, "send_message", "[返信 #1.1「hi」 thread:t]
ok
more"));
        assert_eq!(free.summary, "[返信 #1.1「hi」 thread:t]");
        assert!(free.reply_to.is_none() && free.id.is_none());
    }

    #[test]
    fn a_message_gets_the_id_it_has_in_the_receivers_history() {
        use codex_core::inocode_message_ref::MailEntry;
        let mut a = logged(1, "send_message", "ASK
q: where?
conf: low");
        a.from_thread = Some("thr-a".into());
        a.to_thread = Some("thr-b".into());
        let mut b = logged(2, "send_message", "ASK
q: where?
conf: low");
        b.to_thread = Some("thr-b".into());
        let mut lone = to_message("root", &logged(3, "final", "done"));
        let mut messages = vec![to_message("root", &a), to_message("root", &b)];
        let entry = |id: &str| MailEntry {
            id: id.into(),
            author: "/root/a".into(),
            recipient: "/root/b".into(),
            content: "ASK
q: where?
conf: low".into(),
        };
        let mail = std::collections::HashMap::from([(
            "thr-b".to_string(),
            vec![entry("#2.@1"), entry("#5.@1")],
        )]);
        assign_ids(&mut messages, &mail);
        // Two identical messages take the two matches in order.
        assert_eq!(messages[0].id.as_deref(), Some("#2.@1"));
        assert_eq!(messages[1].id.as_deref(), Some("#5.@1"));
        assign_ids(std::slice::from_mut(&mut lone), &mail);
        assert!(lone.id.is_none());
    }
}
