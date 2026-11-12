//! Knowledge redesign, stage 1 (doc 60, contract doc 61): project id, overview, pages, prohibitions
//! with their relaxations, checklists, the ledger view and the read-only legacy data.
//!
//! Everything is a file under `<project>/.inocode/` (Markdown with a YAML front matter, or JSONL
//! for the append-only relaxation history). The functions take the request JSON and return the
//! response JSON of the `inocode/kb/*` methods; `call` is the single entry point.

use crate::record::Kind;
use crate::record::MatchSpec;
use crate::record::Scope;
use crate::record::Severity;
use crate::record::Status;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

pub const CONFLICT: i64 = -32010;
pub const INVALID: i64 = -32602;
pub const NOT_FOUND: i64 = -32004;

#[derive(Debug, Clone)]
pub struct KbError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

type R = Result<Value, KbError>;

fn invalid(reason: impl Into<String>) -> KbError {
    let reason = reason.into();
    KbError {
        code: INVALID,
        message: format!("invalid params: {reason}"),
        data: Some(json!({ "reason": reason })),
    }
}

fn not_found(what: impl Into<String>) -> KbError {
    KbError {
        code: NOT_FOUND,
        message: format!("not found: {}", what.into()),
        data: None,
    }
}

fn conflict(current: &str) -> KbError {
    KbError {
        code: CONFLICT,
        message: "the file changed; read the current version".to_string(),
        data: Some(json!({ "currentRev": current })),
    }
}

fn io(e: std::io::Error) -> KbError {
    KbError {
        code: -32603,
        message: format!("io error: {e}"),
        data: None,
    }
}

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// FNV-1a over the bytes, 8 hex digits: a stable version label of a text.
fn rev_of(parts: &[&str]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for p in parts {
        for b in p.as_bytes().iter().chain(std::iter::once(&0u8)) {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    format!("h{:08x}", (h ^ (h >> 32)) as u32)
}

fn slug(title: &str) -> String {
    let s: String = title
        .trim()
        .chars()
        .map(|c| {
            if c.is_control() || "\\/:*?\"<>|".contains(c) || c.is_whitespace() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let s: String = s.chars().take(100).collect();
    if s.is_empty() { "_".to_string() } else { s }
}

fn read_text(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|t| t.replace("\r\n", "\n"))
}

fn write_text(p: &Path, text: &str) -> Result<(), KbError> {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    std::fs::write(p, text).map_err(io)
}

fn split_front(text: &str) -> Option<(String, String)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let yaml = rest[..end].to_string();
    let after = &rest[end + 4..];
    Some((yaml, after.strip_prefix('\n').unwrap_or(after).to_string()))
}

fn with_front<T: Serialize>(meta: &T, body: &str) -> String {
    let yaml = serde_yaml::to_string(meta).unwrap_or_default();
    format!("---\n{yaml}---\n{body}")
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn need_str<'a>(v: &'a Value, key: &str) -> Result<&'a str, KbError> {
    str_of(v, key)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| invalid(format!("{key} is required")))
}

fn cwd_of(v: &Value) -> Result<PathBuf, KbError> {
    Ok(PathBuf::from(need_str(v, "cwd")?))
}

fn limit_of(v: &Value) -> usize {
    v.get("limit")
        .and_then(Value::as_u64)
        .map(|n| n.clamp(1, 500) as usize)
        .unwrap_or(50)
}

fn offset_of(v: &Value) -> usize {
    str_of(v, "cursor").and_then(|c| c.parse().ok()).unwrap_or(0)
}

fn fullwidth_to_ascii(c: char) -> char {
    match c {
        '\u{ff01}'..='\u{ff5e}' => char::from_u32(c as u32 - 0xfee0).unwrap_or(c),
        '\u{3000}' => ' ',
        _ => c,
    }
}

pub fn normalize_tag(tag: &str) -> String {
    tag.trim()
        .trim_start_matches('#')
        .chars()
        .map(fullwidth_to_ascii)
        .flat_map(char::to_lowercase)
        .collect()
}

// ---------------------------------------------------------------------------
// locations and the project id
// ---------------------------------------------------------------------------

pub struct Dirs {
    pub root: PathBuf,
    pub home: PathBuf,
}

impl Dirs {
    pub fn new(cwd: &Path) -> Dirs {
        let home = crate::inocode_home();
        Dirs {
            root: crate::project_root_for(cwd, &home),
            home,
        }
    }
    fn inocode(&self) -> PathBuf {
        self.root.join(".inocode")
    }
    fn kb(&self) -> PathBuf {
        self.inocode().join("kb")
    }
    fn project_json(&self) -> PathBuf {
        self.inocode().join("project.json")
    }
    fn overview(&self) -> PathBuf {
        self.kb().join("overview.md")
    }
    fn pages(&self) -> PathBuf {
        self.kb().join("pages")
    }
    fn prohibitions(&self) -> PathBuf {
        self.kb().join("prohibitions")
    }
    fn relaxations(&self) -> PathBuf {
        self.kb().join("relaxations.jsonl")
    }
    fn checklists(&self) -> PathBuf {
        self.kb().join("checklists")
    }
    fn migrations(&self) -> PathBuf {
        self.kb().join("legacy-migrations.json")
    }
}

fn pseudo_uuid() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let a = rev_of(&[&nanos.to_string(), &std::process::id().to_string(), &n.to_string()]);
    let b = rev_of(&[&a, &nanos.to_string(), "b"]);
    let c = rev_of(&[&b, &a, "c"]);
    let hex = format!("{}{}{}", &a[1..], &b[1..], &c[1..]);
    format!(
        "{}-{}-{}-{}-{}{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..24],
        &hex[0..8]
    )
}

fn git_remote(root: &Path) -> Option<String> {
    let cfg = read_text(&root.join(".git").join("config"))?;
    let mut in_origin = false;
    for line in cfg.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_origin = l.starts_with("[remote \"origin\"");
        } else if in_origin && let Some(url) = l.strip_prefix("url") {
            let url = url.trim_start_matches([' ', '=']).trim();
            let url = url
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_start_matches("git@")
                .replace(':', "/");
            return Some(url.trim_end_matches(".git").to_string());
        }
    }
    None
}

/// Makes sure the project has an id (written once). Returns (id, name, remote, ready).
pub fn ensure_project(dirs: &Dirs) -> (Option<String>, String, Option<String>, bool) {
    let name_from_dir = dirs
        .root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let path = dirs.project_json();
    if let Some(text) = std::fs::read_to_string(&path).ok()
        && let Ok(v) = serde_json::from_str::<Value>(&text)
        && let Some(id) = str_of(&v, "projectId")
    {
        let name = str_of(&v, "name").map(str::to_string).unwrap_or(name_from_dir);
        return (
            Some(id.to_string()),
            name,
            str_of(&v, "remote").map(str::to_string),
            true,
        );
    }
    if path.exists() {
        // unreadable: do not overwrite
        return (None, name_from_dir, None, false);
    }
    let id = pseudo_uuid();
    let remote = git_remote(&dirs.root);
    let v = json!({ "projectId": id, "name": name_from_dir, "remote": remote });
    let ok = write_text(&path, &(serde_json::to_string_pretty(&v).unwrap_or_default() + "\n")).is_ok();
    (ok.then_some(id), name_from_dir, remote, ok)
}

fn project_get(p: &Value) -> R {
    let dirs = Dirs::new(&cwd_of(p)?);
    let (_, name, remote, ready) = ensure_project(&dirs);
    Ok(json!({ "name": name, "remote": remote, "ready": ready }))
}

// ---------------------------------------------------------------------------
// text structure: tags, links, quote blocks
// ---------------------------------------------------------------------------

pub fn tags_in(body: &str) -> Vec<String> {
    let mut out = BTreeSet::new();
    for line in body.lines() {
        if line.trim_start().starts_with("> quote:") {
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '#' && (i == 0 || chars[i - 1].is_whitespace()) {
                let mut j = i + 1;
                while j < chars.len() && !chars[j].is_whitespace() && !matches!(chars[j], '#' | '[' | ']') {
                    j += 1;
                }
                if j > i + 1 {
                    out.insert(normalize_tag(&chars[i + 1..j].iter().collect::<String>()));
                }
                i = j;
            } else {
                i += 1;
            }
        }
    }
    out.into_iter().collect()
}

/// `[Title]` links; `[text](url)` is a Markdown link, not a page link.
pub fn links_in(body: &str) -> Vec<String> {
    let mut out = BTreeSet::new();
    let chars: Vec<char> = body.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '[' {
            let mut j = i + 1;
            while j < chars.len() && chars[j] != ']' && chars[j] != '[' && chars[j] != '\n' {
                j += 1;
            }
            if j < chars.len() && chars[j] == ']' && j > i + 1 {
                if chars.get(j + 1) != Some(&'(') {
                    let t: String = chars[i + 1..j].iter().collect();
                    out.insert(t.trim().to_string());
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out.into_iter().filter(|t| !t.is_empty()).collect()
}

/// `> quote:<source ref>: <verbatim text>` lines.
pub fn quotes_in(body: &str) -> Vec<(String, String)> {
    body.lines()
        .filter_map(|l| {
            let rest = l.trim_start().strip_prefix("> quote:")?;
            let (src, text) = rest.split_once(':')?;
            Some((src.trim().to_string(), text.trim_start_matches(' ').to_string()))
        })
        .collect()
}

fn source_text(source_ref: &str) -> Option<(String, Option<String>, String)> {
    let store = codex_inocode_evidence::default_store();
    store
        .read_source(source_ref)
        .map(|(s, text)| (s.title, s.origin, text))
}

// ---------------------------------------------------------------------------
// overview
// ---------------------------------------------------------------------------

fn mtime_rfc(p: &Path) -> Option<String> {
    let m = std::fs::metadata(p).ok()?.modified().ok()?;
    let dt: chrono::DateTime<chrono::Utc> = m.into();
    Some(dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

fn overview_read(p: &Value) -> R {
    let dirs = Dirs::new(&cwd_of(p)?);
    let path = dirs.overview();
    match read_text(&path) {
        Some(t) => Ok(json!({ "text": t, "rev": rev_of(&[&t]), "updatedAt": mtime_rfc(&path) })),
        None => Ok(json!({ "text": "", "rev": "r0", "updatedAt": null })),
    }
}

fn overview_write(p: &Value) -> R {
    let dirs = Dirs::new(&cwd_of(p)?);
    let text = str_of(p, "text").ok_or_else(|| invalid("text is required"))?;
    let path = dirs.overview();
    let current = read_text(&path).map(|t| rev_of(&[&t])).unwrap_or_else(|| "r0".into());
    let base = need_str(p, "baseRev")?;
    if base != current {
        return Err(conflict(&current));
    }
    ensure_project(&dirs);
    write_text(&path, text)?;
    Ok(json!({ "rev": rev_of(&[text]) }))
}

// ---------------------------------------------------------------------------
// pages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RevEntry {
    rev: String,
    by: String,
    at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PageMeta {
    title: String,
    by: String,
    confirmed: bool,
    updated_at: String,
    #[serde(default)]
    revs: Vec<RevEntry>,
}

struct Page {
    meta: PageMeta,
    body: String,
}

impl Page {
    fn rev(&self) -> String {
        rev_of(&[&self.meta.title, &self.body])
    }
}

fn load_page(path: &Path) -> Option<Page> {
    let text = read_text(path)?;
    let (yaml, body) = split_front(&text)?;
    let meta: PageMeta = serde_yaml::from_str(&yaml).ok()?;
    Some(Page { meta, body })
}

fn all_pages(dirs: &Dirs) -> Vec<Page> {
    let Ok(rd) = std::fs::read_dir(dirs.pages()) else {
        return Vec::new();
    };
    let mut v: Vec<Page> = rd
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("md"))
        .filter_map(|e| load_page(&e.path()))
        .collect();
    v.sort_by(|a, b| a.meta.title.cmp(&b.meta.title));
    v
}

fn page_path(dirs: &Dirs, title: &str) -> PathBuf {
    dirs.pages().join(format!("{}.md", slug(title)))
}

fn words(q: Option<&str>) -> Vec<String> {
    q.unwrap_or("")
        .split_whitespace()
        .map(|w| w.chars().map(fullwidth_to_ascii).flat_map(char::to_lowercase).collect())
        .collect()
}

fn snippet(body: &str, words: &[String]) -> String {
    let lower = body.to_lowercase();
    let start = words
        .iter()
        .filter_map(|w| lower.find(w.as_str()))
        .min()
        .unwrap_or(0);
    let start = (0..=start).rev().find(|i| body.is_char_boundary(*i)).unwrap_or(0);
    body[start..]
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(80)
        .collect()
}

fn page_list(p: &Value) -> R {
    let dirs = Dirs::new(&cwd_of(p)?);
    let ws = words(str_of(p, "query"));
    let tag = str_of(p, "tag").map(normalize_tag).filter(|t| !t.is_empty());
    let mut hits: Vec<(u32, Page)> = Vec::new();
    for page in all_pages(&dirs) {
        let tags = tags_in(&page.body);
        if let Some(t) = &tag
            && !tags.contains(t)
        {
            continue;
        }
        let title = page.meta.title.to_lowercase();
        let hay = format!("{}\n{}\n{}", title, page.body.to_lowercase(), tags.join(" "));
        let mut score = 1;
        let mut all = true;
        for w in &ws {
            let mut s = 0;
            if title.contains(w.as_str()) {
                s += 3;
            }
            if hay.contains(w.as_str()) {
                s += 1;
            }
            if s == 0 {
                all = false;
                break;
            }
            score += s;
        }
        if all {
            hits.push((score, page));
        }
    }
    if ws.is_empty() {
        hits.sort_by(|a, b| b.1.meta.updated_at.cmp(&a.1.meta.updated_at));
    } else {
        hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.meta.updated_at.cmp(&a.1.meta.updated_at)));
    }
    let limit = limit_of(p);
    let off = offset_of(p);
    let total = hits.len();
    let pages: Vec<Value> = hits
        .into_iter()
        .skip(off)
        .take(limit)
        .map(|(_, pg)| {
            json!({
                "title": pg.meta.title,
                "tags": tags_in(&pg.body),
                "by": pg.meta.by,
                "confirmed": pg.meta.confirmed,
                "updatedAt": pg.meta.updated_at,
                "snippet": snippet(&pg.body, &ws),
            })
        })
        .collect();
    let next = (off + pages.len() < total).then(|| (off + pages.len()).to_string());
    Ok(json!({ "pages": pages, "nextCursor": next }))
}

fn page_read(p: &Value) -> R {
    let dirs = Dirs::new(&cwd_of(p)?);
    let title = need_str(p, "title")?;
    let page = load_page(&page_path(&dirs, title)).ok_or_else(|| not_found(format!("page {title}")))?;
    let pages = all_pages(&dirs);
    let titles: BTreeSet<&str> = pages.iter().map(|x| x.meta.title.as_str()).collect();
    let links: Vec<Value> = links_in(&page.body)
        .into_iter()
        .map(|t| json!({ "exists": titles.contains(t.as_str()), "title": t }))
        .collect();
    let backlinks: Vec<&str> = pages
        .iter()
        .filter(|o| o.meta.title != page.meta.title && links_in(&o.body).iter().any(|l| l == &page.meta.title))
        .map(|o| o.meta.title.as_str())
        .collect();
    let quotes: Vec<Value> = quotes_in(&page.body)
        .into_iter()
        .map(|(src, text)| {
            let found = source_text(&src);
            json!({
                "sourceRef": src,
                "sourceTitle": found.as_ref().map(|f| f.0.clone()),
                "sourceUrl": found.as_ref().and_then(|f| f.1.clone()),
                "text": text,
                "matches": found.as_ref().is_some_and(|f| f.2.contains(&text)),
            })
        })
        .collect();
    let mut revs = page.meta.revs.clone();
    revs.reverse();
    Ok(json!({
        "title": page.meta.title,
        "body": page.body,
        "tags": tags_in(&page.body),
        "links": links,
        "backlinks": backlinks,
        "quotes": quotes,
        "revs": revs,
        "rev": page.rev(),
        "confirmed": page.meta.confirmed,
    }))
}

fn page_write(p: &Value) -> R {
    let dirs = Dirs::new(&cwd_of(p)?);
    let title = need_str(p, "title")?.trim().to_string();
    let body = str_of(p, "body").ok_or_else(|| invalid("body is required"))?.to_string();
    let by = match str_of(p, "by") {
        Some("model") => "model",
        _ => "user",
    };
    let confirm = p.get("confirm").and_then(Value::as_bool).unwrap_or(false);
    let path = page_path(&dirs, &title);
    let existing = load_page(&path);
    match (&existing, str_of(p, "baseRev")) {
        (Some(e), None) => return Err(conflict(&e.rev())),
        (Some(e), Some(base)) if base != e.rev() => return Err(conflict(&e.rev())),
        (None, Some(_)) => return Err(not_found(format!("page {title}"))),
        _ => {}
    }
    // Quote blocks must be verbatim in their source.
    let mut quote_errors = Vec::new();
    for (src, text) in quotes_in(&body) {
        match source_text(&src) {
            None => quote_errors.push(json!({ "sourceRef": src, "text": text, "reason": "the source does not exist" })),
            Some((_, _, full)) if !full.contains(&text) => quote_errors.push(
                json!({ "sourceRef": src, "text": text, "reason": "the text does not appear word for word in the source" }),
            ),
            Some(_) => {}
        }
    }
    if !quote_errors.is_empty() {
        return Ok(json!({ "rev": null, "tags": tags_in(&body), "links": [], "quoteErrors": quote_errors }));
    }
    ensure_project(&dirs);
    let at = now();
    let mut meta = existing.as_ref().map(|e| e.meta.clone()).unwrap_or_default();
    // keep the old version
    if let Some(old) = &existing
        && let Some(old_text) = read_text(&path)
    {
        let _ = write_text(
            &dirs.kb().join("revs").join(slug(&title)).join(format!("{}.md", old.rev())),
            &old_text,
        );
    }
    meta.title = title.clone();
    meta.by = by.to_string();
    meta.confirmed = by == "user" && confirm;
    meta.updated_at = at.clone();
    let page = Page { meta: meta.clone(), body: body.clone() };
    let rev = page.rev();
    meta.revs.push(RevEntry { rev: rev.clone(), by: by.to_string(), at });
    write_text(&path, &with_front(&meta, &body))?;
    let titles: BTreeSet<String> = all_pages(&dirs).into_iter().map(|x| x.meta.title).collect();
    let links: Vec<Value> = links_in(&body)
        .into_iter()
        .map(|t| json!({ "exists": titles.contains(&t), "title": t }))
        .collect();
    Ok(json!({ "rev": rev, "tags": tags_in(&body), "links": links, "quoteErrors": [] }))
}

fn page_delete(p: &Value) -> R {
    let dirs = Dirs::new(&cwd_of(p)?);
    let title = need_str(p, "title")?;
    let path = page_path(&dirs, title);
    if !path.exists() {
        return Err(not_found(format!("page {title}")));
    }
    let trash = dirs
        .kb()
        .join("trash")
        .join(format!("{}-{}.md", slug(title), chrono::Utc::now().format("%Y%m%dT%H%M%S")));
    if let Some(parent) = trash.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    std::fs::rename(&path, &trash).map_err(io)?;
    Ok(json!({ "movedTo": "trash" }))
}

fn tag_list(p: &Value) -> R {
    let dirs = Dirs::new(&cwd_of(p)?);
    let prefix = str_of(p, "prefix").map(normalize_tag).unwrap_or_default();
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    for page in all_pages(&dirs) {
        for t in tags_in(&page.body) {
            *counts.entry(t).or_default() += 1;
        }
    }
    let tags: Vec<Value> = counts
        .into_iter()
        .filter(|(t, _)| t.starts_with(&prefix))
        .map(|(t, c)| json!({ "tag": t, "count": c }))
        .collect();
    Ok(json!({ "tags": tags }))
}

// ---------------------------------------------------------------------------
// ledger view
// ---------------------------------------------------------------------------

fn ledger_entry(dirs: &Dirs, thread: &str, seq: u64) -> Option<codex_inocode_ledger::LedgerEntry> {
    codex_inocode_ledger::entries_at(&dirs.home, thread)
        .into_iter()
        .find(|e| e.seq == seq)
}

fn ledger_latest_seq(home: &Path, thread: &str) -> u64 {
    codex_inocode_ledger::entries_at(home, thread)
        .iter()
        .map(|e| e.seq)
        .max()
        .unwrap_or(0)
}

fn ledger_list(p: &Value) -> R {
    let home = crate::inocode_home();
    let thread = need_str(p, "threadId")?;
    let ws = words(str_of(p, "query"));
    let mut entries: Vec<_> = codex_inocode_ledger::entries_at(&home, thread)
        .into_iter()
        .filter(|e| {
            let t = e.text.to_lowercase();
            ws.iter().all(|w| t.contains(w.as_str()))
        })
        .collect();
    entries.reverse();
    let limit = limit_of(p);
    let off = offset_of(p);
    let total = entries.len();
    let out: Vec<Value> = entries
        .into_iter()
        .skip(off)
        .take(limit)
        .map(|e| json!({ "threadId": thread, "seq": e.seq, "tsMs": e.ts_ms, "text": e.text, "truncated": false }))
        .collect();
    let next = (off + out.len() < total).then(|| (off + out.len()).to_string());
    Ok(json!({ "entries": out, "nextCursor": next }))
}

fn ledger_get(p: &Value) -> R {
    let dirs = Dirs { root: PathBuf::new(), home: crate::inocode_home() };
    let thread = need_str(p, "threadId")?;
    let seq = p.get("seq").and_then(Value::as_u64).ok_or_else(|| invalid("seq is required"))?;
    let e = ledger_entry(&dirs, thread, seq).ok_or_else(|| not_found(format!("ledger {thread} #{seq}")))?;
    let latest = ledger_latest_seq(&dirs.home, thread);
    Ok(json!({
        "entry": { "threadId": thread, "seq": e.seq, "tsMs": e.ts_ms, "text": e.text },
        "laterEntries": latest.saturating_sub(seq),
    }))
}

/// The user's own words behind a reference: it must exist in the ledger and contain the quote.
fn check_ledger_quote(dirs: &Dirs, reference: &Value, quote: &str) -> Result<(), KbError> {
    let thread = need_str(reference, "threadId").map_err(|_| invalid("ledgerRef.threadId is required"))?;
    let seq = reference
        .get("seq")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("ledgerRef.seq is required"))?;
    if quote.trim().is_empty() {
        return Err(invalid("quote is required with a ledgerRef"));
    }
    let entry = ledger_entry(dirs, thread, seq).ok_or_else(|| {
        invalid("the referenced user message is not in the ledger (only what the user typed is recorded)")
    })?;
    if !entry.text.contains(quote) {
        return Err(invalid("the quote does not appear word for word in the referenced user message"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// prohibitions and relaxations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Origin {
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ledger_ref: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quote: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Prohibition {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub reason: String,
    /// `block` or `warn`.
    pub strength: String,
    #[serde(default, rename = "match", skip_serializing_if = "Option::is_none")]
    pub match_spec: Option<MatchSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// `active` or `retracted`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub irreversible: Option<bool>,
    origin: Origin,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

fn prohibition_path(dirs: &Dirs, id: &str) -> PathBuf {
    dirs.prohibitions().join(format!("{}.md", slug(id)))
}

fn load_prohibition(path: &Path) -> Option<Prohibition> {
    let text = read_text(path)?;
    let (yaml, _) = split_front(&text)?;
    serde_yaml::from_str(&yaml).ok()
}

fn all_prohibitions(dirs: &Dirs) -> Vec<Prohibition> {
    let Ok(rd) = std::fs::read_dir(dirs.prohibitions()) else {
        return Vec::new();
    };
    let mut v: Vec<Prohibition> = rd
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("md"))
        .filter_map(|e| load_prohibition(&e.path()))
        .collect();
    v.sort_by(|a, b| a.id.cmp(&b.id));
    v
}

fn next_number(prefix: &str, ids: impl Iterator<Item = String>) -> String {
    let max = ids
        .filter_map(|i| i.strip_prefix(&format!("{prefix}-"))?.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    format!("{prefix}-{:04}", max + 1)
}

fn save_prohibition(dirs: &Dirs, p: &Prohibition) -> Result<(), KbError> {
    write_text(&prohibition_path(dirs, &p.id), &with_front(p, ""))
}

fn parse_match(v: Option<&Value>) -> Result<Option<MatchSpec>, KbError> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(m) => serde_json::from_value::<MatchSpec>(m.clone())
            .map(Some)
            .map_err(|e| invalid(format!("match: {e}"))),
    }
}

fn expired(expires: &Option<String>) -> bool {
    expires
        .as_deref()
        .and_then(|e| chrono::DateTime::parse_from_rfc3339(e).ok())
        .is_some_and(|t| t < chrono::Utc::now())
}

fn prohibition_write(p: &Value) -> R {
    let dirs = Dirs::new(&cwd_of(p)?);
    let title = need_str(p, "title")?.trim().to_string();
    let strength = match str_of(p, "strength") {
        Some(s @ ("block" | "warn")) => s.to_string(),
        _ => return Err(invalid("strength must be block or warn")),
    };
    let origin_v = p.get("origin").ok_or_else(|| invalid("origin is required"))?;
    let kind = need_str(origin_v, "kind")?;
    let origin = match kind {
        "ledger" => {
            let reference = origin_v.get("ledgerRef").ok_or_else(|| invalid("ledgerRef is required for origin ledger"))?;
            let quote = str_of(origin_v, "quote").unwrap_or("");
            check_ledger_quote(&dirs, reference, quote)?;
            Origin { kind: "ledger".into(), ledger_ref: Some(reference.clone()), quote: Some(quote.to_string()) }
        }
        "ui" => Origin { kind: "ui".into(), ..Default::default() },
        other => return Err(invalid(format!("origin.kind {other} cannot be written (use ledger or ui)"))),
    };
    let spec = parse_match(p.get("match"))?;
    ensure_project(&dirs);
    let at = now();
    let existing_id = str_of(p, "id").filter(|s| !s.is_empty());
    let (id, created_at) = match existing_id {
        Some(id) => {
            let old = load_prohibition(&prohibition_path(&dirs, id)).ok_or_else(|| not_found(format!("prohibition {id}")))?;
            // keep the previous version
            if let Some(text) = read_text(&prohibition_path(&dirs, id)) {
                let _ = write_text(
                    &dirs.prohibitions().join(".history").join(format!("{}-{}.md", slug(id), rev_of(&[&text]))),
                    &text,
                );
            }
            (old.id, old.created_at)
        }
        None => (
            next_number("proh", all_prohibitions(&dirs).into_iter().map(|x| x.id)),
            at.clone(),
        ),
    };
    let prohibition = Prohibition {
        id: id.clone(),
        title,
        reason: str_of(p, "reason").unwrap_or("").to_string(),
        strength,
        match_spec: spec,
        role: str_of(p, "role").filter(|s| !s.is_empty()).map(str::to_string),
        status: "active".into(),
        expires: str_of(p, "expires").filter(|s| !s.is_empty()).map(str::to_string),
        irreversible: p.get("irreversible").and_then(Value::as_bool),
        origin,
        created_at,
        updated_at: at,
    };
    save_prohibition(&dirs, &prohibition)?;
    Ok(json!({ "id": id }))
}

fn prohibition_retract(p: &Value) -> R {
    let dirs = Dirs::new(&cwd_of(p)?);
    let id = need_str(p, "id")?;
    let mut x = load_prohibition(&prohibition_path(&dirs, id)).ok_or_else(|| not_found(format!("prohibition {id}")))?;
    x.status = "retracted".into();
    x.updated_at = now();
    save_prohibition(&dirs, &x)?;
    Ok(json!({}))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelaxationEvent {
    /// `add` or `revoke`.
    #[serde(rename = "type")]
    kind: String,
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prohibition_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ledger_ref: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effect: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allow: Option<MatchSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    until: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    by: Option<String>,
    at: String,
}

#[derive(Debug, Clone)]
pub struct Relaxation {
    pub id: String,
    pub prohibition_id: String,
    pub ledger_ref: Option<Value>,
    pub quote: Option<String>,
    pub effect: String,
    pub allow: Option<MatchSpec>,
    pub scope: String,
    pub until: Option<String>,
    pub by: String,
    pub at: String,
    pub revoked_at: Option<String>,
}

impl Relaxation {
    fn ref_thread(&self) -> Option<&str> {
        self.ledger_ref.as_ref().and_then(|r| r.get("threadId")).and_then(Value::as_str)
    }
    fn ref_seq(&self) -> Option<u64> {
        self.ledger_ref.as_ref().and_then(|r| r.get("seq")).and_then(Value::as_u64)
    }

    /// Is it in force now? `thread` is the thread the checked action runs in, when known.
    pub fn active(&self, home: &Path, thread: Option<&str>) -> bool {
        if self.revoked_at.is_some() {
            return false;
        }
        match self.scope.as_str() {
            "always" => true,
            "until" => self
                .until
                .as_deref()
                .and_then(|u| chrono::DateTime::parse_from_rfc3339(u).ok())
                .is_some_and(|t| t > chrono::Utc::now()),
            "session" => match (self.ref_thread(), thread) {
                (Some(r), Some(t)) => r == t,
                (Some(_), None) => false,
                (None, _) => true,
            },
            // "request": while the user message it quotes is the newest one of that thread
            _ => match (self.ref_thread(), self.ref_seq(), thread) {
                (Some(r), Some(seq), Some(t)) => r == t && ledger_latest_seq(home, t) == seq,
                _ => false,
            },
        }
    }

    fn to_json(&self, home: &Path, thread: Option<&str>) -> Value {
        json!({
            "id": self.id,
            "ledgerRef": self.ledger_ref,
            "quote": self.quote,
            "effect": self.effect,
            "allow": self.allow,
            "scope": self.scope,
            "until": self.until,
            "by": self.by,
            "at": self.at,
            "revokedAt": self.revoked_at,
            "active": self.active(home, thread),
        })
    }
}

fn read_relaxations(dirs: &Dirs) -> Vec<Relaxation> {
    let Some(text) = read_text(&dirs.relaxations()) else {
        return Vec::new();
    };
    let mut out: Vec<Relaxation> = Vec::new();
    for line in text.lines() {
        let Ok(ev) = serde_json::from_str::<RelaxationEvent>(line) else {
            continue;
        };
        match ev.kind.as_str() {
            "add" => out.push(Relaxation {
                id: ev.id,
                prohibition_id: ev.prohibition_id.unwrap_or_default(),
                ledger_ref: ev.ledger_ref,
                quote: ev.quote,
                effect: ev.effect.unwrap_or_default(),
                allow: ev.allow,
                scope: ev.scope.unwrap_or_else(|| "request".into()),
                until: ev.until,
                by: ev.by.unwrap_or_default(),
                at: ev.at,
                revoked_at: None,
            }),
            "revoke" => {
                if let Some(r) = out.iter_mut().find(|r| r.id == ev.id) {
                    r.revoked_at = Some(ev.at);
                }
            }
            _ => {}
        }
    }
    out
}

fn append_relaxation_event(dirs: &Dirs, ev: &RelaxationEvent) -> Result<(), KbError> {
    use std::io::Write;
    let path = dirs.relaxations();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(io)?;
    writeln!(f, "{}", serde_json::to_string(ev).unwrap_or_default()).map_err(io)
}

fn relaxation_add(p: &Value) -> R {
    let dirs = Dirs::new(&cwd_of(p)?);
    let pid = need_str(p, "prohibitionId")?;
    let proh = load_prohibition(&prohibition_path(&dirs, pid)).ok_or_else(|| not_found(format!("prohibition {pid}")))?;
    if proh.status != "active" {
        return Err(invalid("the prohibition is retracted"));
    }
    let by = match str_of(p, "by") {
        Some(b @ ("user-text" | "user-ui")) => b,
        _ => return Err(invalid("by must be user-text or user-ui")),
    };
    let effect = match str_of(p, "effect") {
        Some(e @ ("allowOnly" | "warnOnly" | "disable")) => e,
        _ => return Err(invalid("effect must be allowOnly, warnOnly or disable")),
    };
    let scope = str_of(p, "scope").unwrap_or("request");
    if !matches!(scope, "request" | "session" | "until" | "always") {
        return Err(invalid("scope must be request, session, until or always"));
    }
    let reference = p.get("ledgerRef").filter(|v| !v.is_null());
    let quote = str_of(p, "quote");
    if by == "user-text" {
        let r = reference.ok_or_else(|| invalid("ledgerRef is required when by is user-text"))?;
        check_ledger_quote(&dirs, r, quote.unwrap_or(""))?;
    } else if let (Some(r), Some(q)) = (reference, quote) {
        check_ledger_quote(&dirs, r, q)?;
    }
    if scope == "request" && reference.is_none() {
        return Err(invalid("scope request needs the ledgerRef of the user's message; use until or always from the screen"));
    }
    let until = str_of(p, "until").filter(|s| !s.is_empty());
    if scope == "until" {
        let u = until.ok_or_else(|| invalid("until is required for scope until"))?;
        chrono::DateTime::parse_from_rfc3339(u).map_err(|_| invalid("until must be an RFC 3339 time"))?;
    }
    let allow = parse_match(p.get("allow"))?;
    if effect == "allowOnly" && !allow.as_ref().is_some_and(MatchSpec::has_criteria) {
        return Err(invalid("allowOnly needs an allow condition (commands, paths, tools, ...)"));
    }
    let existing = read_relaxations(&dirs);
    let id = next_number("rel", existing.iter().map(|r| r.id.clone()));
    append_relaxation_event(
        &dirs,
        &RelaxationEvent {
            kind: "add".into(),
            id: id.clone(),
            prohibition_id: Some(pid.to_string()),
            ledger_ref: reference.cloned(),
            quote: quote.map(str::to_string),
            effect: Some(effect.to_string()),
            allow,
            scope: Some(scope.to_string()),
            until: until.map(str::to_string),
            by: Some(by.to_string()),
            at: now(),
        },
    )?;
    Ok(json!({ "id": id }))
}

fn relaxation_revoke(p: &Value) -> R {
    let dirs = Dirs::new(&cwd_of(p)?);
    let rid = need_str(p, "relaxationId")?;
    let pid = need_str(p, "prohibitionId")?;
    let found = read_relaxations(&dirs)
        .into_iter()
        .find(|r| r.id == rid && r.prohibition_id == pid)
        .ok_or_else(|| not_found(format!("relaxation {rid}")))?;
    if found.revoked_at.is_none() {
        append_relaxation_event(
            &dirs,
            &RelaxationEvent {
                kind: "revoke".into(),
                id: rid.to_string(),
                prohibition_id: Some(pid.to_string()),
                ledger_ref: None,
                quote: None,
                effect: None,
                allow: None,
                scope: None,
                until: None,
                by: None,
                at: now(),
            },
        )?;
    }
    Ok(json!({}))
}

fn prohibition_to_json(dirs: &Dirs, x: &Prohibition, rels: &[Relaxation]) -> Value {
    let mine: Vec<Value> = rels
        .iter()
        .filter(|r| r.prohibition_id == x.id)
        .map(|r| r.to_json(&dirs.home, None))
        .collect();
    json!({
        "id": x.id,
        "title": x.title,
        "reason": x.reason,
        "strength": x.strength,
        "match": x.match_spec,
        "role": x.role,
        "status": if x.status == "active" && expired(&x.expires) { "retracted" } else { x.status.as_str() },
        "expires": x.expires,
        "irreversible": x.irreversible,
        "origin": { "kind": x.origin.kind, "ledgerRef": x.origin.ledger_ref, "quote": x.origin.quote },
        "relaxations": mine,
    })
}

fn prohibition_list(p: &Value) -> R {
    let dirs = Dirs::new(&cwd_of(p)?);
    let status = str_of(p, "status");
    let rels = read_relaxations(&dirs);
    let list: Vec<Value> = all_prohibitions(&dirs)
        .iter()
        .map(|x| prohibition_to_json(&dirs, x, &rels))
        .filter(|v| status.is_none_or(|s| v["status"] == s))
        .collect();
    Ok(json!({ "prohibitions": list }))
}

/// Active (not retracted, not expired) prohibitions of the project, for the guard and the prompt.
pub fn active_prohibitions(root: &Path) -> Vec<Prohibition> {
    let dirs = Dirs { root: root.to_path_buf(), home: crate::inocode_home() };
    all_prohibitions(&dirs)
        .into_iter()
        .filter(|x| x.status == "active" && !expired(&x.expires))
        .collect()
}

/// The relaxations in force for `prohibition` now (for `thread`, when known).
pub fn active_relaxations(root: &Path, prohibition: &str, thread: Option<&str>) -> Vec<Relaxation> {
    let dirs = Dirs { root: root.to_path_buf(), home: crate::inocode_home() };
    read_relaxations(&dirs)
        .into_iter()
        .filter(|r| r.prohibition_id == prohibition && r.active(&dirs.home, thread))
        .collect()
}

pub(crate) fn prohibition_rule(x: &Prohibition) -> crate::record::Record {
    let mut r = crate::record::Record::new(Kind::Rule, &x.title);
    r.id = x.id.clone();
    r.scope = Scope::Project;
    r.status = Status::Verified;
    r.severity = Some(if x.strength == "block" { Severity::Block } else { Severity::Warn });
    r.reason = Some(x.reason.clone());
    r.match_spec = x.match_spec.clone();
    r.irreversible = x.irreversible;
    r
}

/// Is the relaxation's `allow` condition met by this action (for `allowOnly`)?
pub(crate) fn allow_matches(r: &Relaxation, action: &crate::guard::GuardAction) -> bool {
    r.allow.as_ref().is_some_and(|spec| crate::guard::spec_matches(spec, action))
}

// ---------------------------------------------------------------------------
// checklists
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ChecklistItem {
    id: String,
    text: String,
    #[serde(default)]
    when: String,
    #[serde(default)]
    check: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
    #[serde(default)]
    done: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    done_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    done_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ChecklistFile {
    role: String,
    #[serde(default)]
    items: Vec<ChecklistItem>,
}

fn checklist_path(dirs: &Dirs, role: &str) -> PathBuf {
    dirs.checklists().join(format!("{}.md", slug(role)))
}

fn load_checklists(dirs: &Dirs) -> Vec<ChecklistFile> {
    let Ok(rd) = std::fs::read_dir(dirs.checklists()) else {
        return Vec::new();
    };
    let mut v: Vec<ChecklistFile> = rd
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("md"))
        .filter_map(|e| {
            let t = read_text(&e.path())?;
            let (yaml, _) = split_front(&t)?;
            serde_yaml::from_str(&yaml).ok()
        })
        .collect();
    v.sort_by(|a, b| a.role.cmp(&b.role));
    v
}

fn save_checklist(dirs: &Dirs, c: &ChecklistFile) -> Result<(), KbError> {
    write_text(&checklist_path(dirs, &c.role), &with_front(c, ""))
}

fn item_json(role: &str, i: &ChecklistItem) -> Value {
    json!({
        "id": i.id, "role": role, "text": i.text, "when": i.when, "check": i.check,
        "origin": i.origin, "done": i.done, "doneBy": i.done_by, "doneAt": i.done_at,
    })
}

fn checklist_list(p: &Value) -> R {
    let dirs = Dirs::new(&cwd_of(p)?);
    let role = str_of(p, "role").filter(|r| !r.is_empty());
    let all = load_checklists(&dirs);
    let roles: Vec<&str> = all.iter().map(|c| c.role.as_str()).collect();
    let items: Vec<Value> = all
        .iter()
        .filter(|c| role.is_none_or(|r| r == c.role))
        .flat_map(|c| c.items.iter().map(|i| item_json(&c.role, i)))
        .collect();
    Ok(json!({ "roles": roles, "items": items }))
}

fn checklist_write(p: &Value) -> R {
    let dirs = Dirs::new(&cwd_of(p)?);
    let role = need_str(p, "role")?.trim().to_string();
    let items_in = p.get("items").and_then(Value::as_array).ok_or_else(|| invalid("items is required"))?;
    let all = load_checklists(&dirs);
    let old = all.iter().find(|c| c.role == role).cloned().unwrap_or_default();
    let mut ids: Vec<String> = all.iter().flat_map(|c| c.items.iter().map(|i| i.id.clone())).collect();
    let mut items = Vec::new();
    for it in items_in {
        let text = need_str(it, "text")?.trim().to_string();
        let when = match str_of(it, "when") {
            Some(w @ ("beforeStart" | "afterChange" | "beforeReport")) => w.to_string(),
            None | Some("") => "beforeReport".to_string(),
            Some(_) => return Err(invalid("when must be beforeStart, afterChange or beforeReport")),
        };
        let check = match str_of(it, "check") {
            Some(c @ ("machine" | "user" | "self")) => c.to_string(),
            None | Some("") => "self".to_string(),
            Some(_) => return Err(invalid("check must be machine, user or self")),
        };
        let prior = str_of(it, "id").and_then(|id| old.items.iter().find(|o| o.id == id));
        let id = match str_of(it, "id").filter(|s| !s.is_empty()) {
            Some(id) => id.to_string(),
            None => {
                let n = next_number("chk", ids.iter().cloned());
                ids.push(n.clone());
                n
            }
        };
        items.push(ChecklistItem {
            id,
            text,
            when,
            check,
            origin: str_of(it, "origin").filter(|s| !s.is_empty()).map(str::to_string),
            done: prior.is_some_and(|o| o.done),
            done_by: prior.and_then(|o| o.done_by.clone()),
            done_at: prior.and_then(|o| o.done_at.clone()),
        });
    }
    ensure_project(&dirs);
    let ids_out: Vec<Value> = items.iter().map(|i| json!({ "id": i.id })).collect();
    save_checklist(&dirs, &ChecklistFile { role, items })?;
    Ok(json!({ "items": ids_out }))
}

fn checklist_set_done(p: &Value) -> R {
    let dirs = Dirs::new(&cwd_of(p)?);
    let role = need_str(p, "role")?;
    let item_id = need_str(p, "itemId")?;
    let done = p.get("done").and_then(Value::as_bool).ok_or_else(|| invalid("done is required"))?;
    let mut c = load_checklists(&dirs)
        .into_iter()
        .find(|c| c.role == role)
        .ok_or_else(|| not_found(format!("checklist {role}")))?;
    let item = c
        .items
        .iter_mut()
        .find(|i| i.id == item_id)
        .ok_or_else(|| not_found(format!("item {item_id}")))?;
    if item.check == "machine" {
        return Err(invalid("a machine item is marked by the host, not by hand"));
    }
    item.done = done;
    item.done_by = done.then(|| "user".to_string());
    item.done_at = done.then(now);
    save_checklist(&dirs, &c)?;
    Ok(json!({}))
}

// ---------------------------------------------------------------------------
// legacy (read only) and the migration marks
// ---------------------------------------------------------------------------

fn read_migrations(dirs: &Dirs) -> BTreeMap<String, Value> {
    read_text(&dirs.migrations())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Ids of legacy rules the user has decided on (they no longer guard by themselves).
pub fn migrated_rule_ids(root: &Path) -> BTreeSet<String> {
    let dirs = Dirs { root: root.to_path_buf(), home: crate::inocode_home() };
    read_migrations(&dirs).into_keys().collect()
}

fn legacy_store(dirs: &Dirs) -> crate::Store {
    crate::Store::new(Some(dirs.root.clone()), dirs.home.clone())
}

fn suggested(r: &crate::record::Record) -> &'static str {
    let t = &r.title;
    if r.status == Status::Verified {
        "prohibition"
    } else if r.status == Status::Retracted && (t.contains("検証") || t.contains("目視")) {
        "checklist"
    } else {
        "unused"
    }
}

fn legacy_rules(p: &Value) -> R {
    let dirs = Dirs::new(&cwd_of(p)?);
    let marks = read_migrations(&dirs);
    let rules: Vec<Value> = legacy_store(&dirs)
        .all(Some(Scope::Project))
        .into_iter()
        .filter(|r| r.kind == Kind::Rule)
        .map(|r| {
            let migrated = marks.get(&r.id).cloned();
            let guarding = migrated.is_none() && r.status.is_live() && r.match_spec.is_some();
            json!({
                "id": r.id,
                "title": r.title,
                "reason": r.reason,
                "severity": r.severity.map(|s| if s == Severity::Block { "block" } else { "warn" }),
                "status": serde_json::to_value(r.status).unwrap_or(Value::Null),
                "source": r.source,
                "match": r.match_spec,
                "suggested": suggested(&r),
                "migrated": migrated,
                "stillGuarding": guarding,
            })
        })
        .collect();
    Ok(json!({ "rules": rules }))
}

fn legacy_migrate(p: &Value) -> R {
    let dirs = Dirs::new(&cwd_of(p)?);
    let rule_id = need_str(p, "ruleId")?;
    let to = str_of(p, "to").unwrap_or("");
    let rule = legacy_store(&dirs)
        .read(Scope::Project, rule_id)
        .filter(|r| r.kind == Kind::Rule)
        .ok_or_else(|| not_found(format!("rule {rule_id}")))?;
    let mut marks = read_migrations(&dirs);
    if marks.contains_key(rule_id) {
        return Err(invalid("this rule was already migrated"));
    }
    let (new_id, origin) = match to {
        "prohibition" => {
            let (origin_kind, origin_ref, origin_quote) = match (p.get("ledgerRef").filter(|v| !v.is_null()), str_of(p, "quote")) {
                (Some(r), Some(q)) => {
                    check_ledger_quote(&dirs, r, q)?;
                    ("ledger", Some(r.clone()), Some(q.to_string()))
                }
                _ => ("legacy", None, None),
            };
            ensure_project(&dirs);
            let at = now();
            let id = next_number("proh", all_prohibitions(&dirs).into_iter().map(|x| x.id));
            save_prohibition(
                &dirs,
                &Prohibition {
                    id: id.clone(),
                    title: rule.title.clone(),
                    reason: rule.reason.clone().unwrap_or_default(),
                    strength: if rule.severity == Some(Severity::Block) { "block".into() } else { "warn".into() },
                    match_spec: rule.match_spec.clone(),
                    role: None,
                    status: "active".into(),
                    expires: None,
                    irreversible: rule.irreversible,
                    origin: Origin { kind: origin_kind.into(), ledger_ref: origin_ref, quote: origin_quote },
                    created_at: at.clone(),
                    updated_at: at,
                },
            )?;
            (Some(id), origin_kind)
        }
        "checklist" => {
            let role = str_of(p, "role").filter(|s| !s.is_empty()).unwrap_or("全員");
            let all = load_checklists(&dirs);
            let mut c = all.iter().find(|c| c.role == role).cloned().unwrap_or(ChecklistFile { role: role.into(), items: vec![] });
            let id = next_number("chk", all.iter().flat_map(|c| c.items.iter().map(|i| i.id.clone())));
            c.items.push(ChecklistItem {
                id: id.clone(),
                text: rule.title.clone(),
                when: "beforeReport".into(),
                check: "self".into(),
                origin: Some(format!("legacy:{rule_id}")),
                ..Default::default()
            });
            ensure_project(&dirs);
            save_checklist(&dirs, &c)?;
            (Some(id), "legacy")
        }
        "skip" => (None, "legacy"),
        _ => return Err(invalid("to must be prohibition, checklist or skip")),
    };
    marks.insert(rule_id.to_string(), json!({ "to": to, "id": new_id, "at": now() }));
    write_text(&dirs.migrations(), &serde_json::to_string_pretty(&marks).unwrap_or_default())?;
    Ok(json!({ "id": new_id, "origin": origin }))
}

pub fn is_candidate(r: &crate::record::Record) -> bool {
    r.kind == Kind::FailureCase && r.created_by.as_deref() == Some("advisor")
}

fn legacy_failure_cases(p: &Value) -> R {
    let dirs = Dirs::new(&cwd_of(p)?);
    let limit = limit_of(p);
    let off = offset_of(p);
    let all: Vec<_> = legacy_store(&dirs)
        .all(Some(Scope::Project))
        .into_iter()
        .filter(|r| r.kind == Kind::FailureCase)
        .collect();
    let total = all.len();
    let cases: Vec<Value> = all
        .into_iter()
        .skip(off)
        .take(limit)
        .map(|r| {
            json!({
                "id": r.id, "title": r.title, "symptom": r.symptom,
                "candidate": is_candidate(&r),
            })
        })
        .collect();
    let next = (off + cases.len() < total).then(|| (off + cases.len()).to_string());
    Ok(json!({ "cases": cases, "nextCursor": next }))
}

// ---------------------------------------------------------------------------
// the computed index
// ---------------------------------------------------------------------------

fn index_get(p: &Value) -> R {
    let dirs = Dirs::new(&cwd_of(p)?);
    let pages = all_pages(&dirs);
    let overview = read_text(&dirs.overview()).unwrap_or_default();
    let titles: BTreeSet<&str> = pages.iter().map(|x| x.meta.title.as_str()).collect();
    let mut tag_counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut unresolved: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for page in &pages {
        for t in tags_in(&page.body) {
            *tag_counts.entry(t).or_default() += 1;
        }
        for l in links_in(&page.body) {
            if !titles.contains(l.as_str()) {
                unresolved.entry(l).or_default().insert(page.meta.title.clone());
            }
        }
    }
    for t in tags_in(&overview) {
        *tag_counts.entry(t).or_default() += 1;
    }
    for l in links_in(&overview) {
        if !titles.contains(l.as_str()) {
            unresolved.entry(l).or_default().insert("概要".to_string());
        }
    }
    let rels = read_relaxations(&dirs);
    let proh: Vec<Value> = active_prohibitions(&dirs.root)
        .iter()
        .map(|x| {
            let relaxed = rels.iter().any(|r| r.prohibition_id == x.id && r.active(&dirs.home, None));
            json!({ "id": x.id, "title": x.title, "strength": x.strength, "relaxed": relaxed })
        })
        .collect();
    let open: Vec<Value> = load_checklists(&dirs)
        .iter()
        .map(|c| {
            json!({ "role": c.role, "open": c.items.iter().filter(|i| !i.done).count(), "total": c.items.len() })
        })
        .collect();
    let store = legacy_store(&dirs);
    let legacy_all = store.all(Some(Scope::Project));
    let claims = codex_inocode_evidence::default_store().list_claims(None).len();
    let mut tags: Vec<(String, u32)> = tag_counts.into_iter().collect();
    tags.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(json!({
        "tags": tags.into_iter().map(|(t, c)| json!({ "tag": t, "count": c })).collect::<Vec<_>>(),
        "unresolvedLinks": unresolved.into_iter().map(|(t, from)| json!({ "title": t, "fromPages": from })).collect::<Vec<_>>(),
        "activeProhibitions": proh,
        "openChecklist": open,
        "pageCount": pages.len(),
        "commonRefCount": 0,
        "legacy": {
            "rules": legacy_all.iter().filter(|r| r.kind == Kind::Rule).count(),
            "failureCases": legacy_all.iter().filter(|r| r.kind == Kind::FailureCase).count(),
            "evidenceClaims": claims,
        },
    }))
}

// ---------------------------------------------------------------------------
// entry point
// ---------------------------------------------------------------------------

/// `method` is the part after `inocode/kb/` (for example `page/read`).
pub fn call(method: &str, params: &Value) -> R {
    match method {
        "project/get" => project_get(params),
        "overview/read" => overview_read(params),
        "overview/write" => overview_write(params),
        "index/get" => index_get(params),
        "page/list" => page_list(params),
        "page/read" => page_read(params),
        "page/write" => page_write(params),
        "page/delete" => page_delete(params),
        "tag/list" => tag_list(params),
        "prohibition/list" => prohibition_list(params),
        "prohibition/write" => prohibition_write(params),
        "prohibition/retract" => prohibition_retract(params),
        "relaxation/add" => relaxation_add(params),
        "relaxation/revoke" => relaxation_revoke(params),
        "ledger/list" => ledger_list(params),
        "ledger/get" => ledger_get(params),
        "checklist/list" => checklist_list(params),
        "checklist/write" => checklist_write(params),
        "checklist/setDone" => checklist_set_done(params),
        "legacy/rules" => legacy_rules(params),
        "legacy/migrate" => legacy_migrate(params),
        "legacy/failureCases" => legacy_failure_cases(params),
        other => Err(KbError { code: -32601, message: format!("method not found: inocode/kb/{other}"), data: None }),
    }
}

#[cfg(test)]
#[path = "kb_tests.rs"]
mod kb_tests;
