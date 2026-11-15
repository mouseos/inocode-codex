//! Importing Claude Code and Codex history into InoCode (ref/docs/22-import-plan.md, 70).
//!
//! - [`detect`]: the sessions found under `~/.claude/projects` and `~/.codex/sessions` (and
//!   `archived_sessions`), and the memory files (Claude's `memory/*.md` and `CLAUDE.md`, Codex's
//!   `AGENTS.md` and `memories/`), with what was imported already.
//! - [`convert`]: one session file as the rollout items of an InoCode thread (Legacy history), the
//!   user's words (for the ledger) and the first message.
//! - The import record (`INOCODE_HOME/imports/imports.json`): which file became which thread, and
//!   per thread what the user decided about extracting items from it.
//!
//! The sources are only read. Nothing is extracted on import: the user asks for it per session.

mod claude;
mod codex;
mod items;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use std::collections::BTreeMap;
use std::io::BufRead;
use std::io::Read;
use std::io::Seek;
use std::path::Path;
use std::path::PathBuf;

pub use codex_rollout::RolloutItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Claude,
    Codex,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Claude => "Claude Code",
            Source::Codex => "Codex",
        }
    }
}

/// A session file that can be imported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Found {
    pub source: Source,
    pub path: String,
    pub size_bytes: u64,
    /// Last write, seconds since the epoch.
    pub modified: i64,
    pub cwd: Option<String>,
    pub title: Option<String>,
    /// The thread it became, when it was imported (and the file has not changed since).
    pub thread_id: Option<String>,
    /// Imported before, and the file has grown or changed since.
    pub changed: bool,
}

/// A memory or instruction file items can be extracted from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Memory {
    pub source: Source,
    pub path: String,
    pub size_bytes: u64,
    pub modified: i64,
    /// The project it belongs to (None: every project).
    pub cwd: Option<String>,
}

/// One session as an InoCode thread.
#[derive(Debug, Clone)]
pub struct Converted {
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub first_user: Option<String>,
    pub items: Vec<RolloutItem>,
    /// The user's messages, verbatim and in order (for the ledger of the user's words).
    pub user_texts: Vec<String>,
    /// Lines that were not JSON (skipped).
    pub broken_lines: usize,
    /// About how many tokens the history is in a model's context (4 bytes a token).
    pub context_tokens: i64,
}

pub fn convert(path: &Path, source: Source) -> std::io::Result<Converted> {
    match source {
        Source::Claude => claude::convert(path),
        Source::Codex => codex::convert(path),
    }
}

/// RFC 3339 time to seconds since the epoch (`2026-11-14T13:45:21.552Z`, `+09:00` offsets).
pub(crate) fn parse_ts(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    // days from civil (Howard Hinnant)
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let mut secs = days * 86_400 + h * 3_600 + mi * 60 + se;
    let rest = &s[19..];
    let tz = rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    if let Some(sign) = tz.chars().next().filter(|c| *c == '+' || *c == '-') {
        let off = tz.get(1..3)?.parse::<i64>().ok()? * 3_600 + tz.get(4..6).and_then(|m| m.parse::<i64>().ok()).unwrap_or(0) * 60;
        secs -= if sign == '+' { off } else { -off };
    }
    Some(secs)
}

// ---- the import record -------------------------------------------------------------------------

/// What the user decided about extracting items from an imported thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Extraction {
    /// Not asked yet: asked when the thread is first opened.
    #[default]
    None,
    /// "Later": asked again the next time.
    Later,
    /// Started (the extraction turn was sent).
    Started,
    /// "Do not ask in this project".
    Declined,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedFile {
    pub source: Option<Source>,
    pub sha256: String,
    pub size_bytes: u64,
    pub thread_id: String,
    pub imported_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedThread {
    pub source: Option<Source>,
    pub path: String,
    /// The folder the thread was made in.
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub extraction: Extraction,
    /// About how many tokens the history is in a model's context.
    #[serde(default)]
    pub context_tokens: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    #[serde(default)]
    pub files: BTreeMap<String, ImportedFile>,
    #[serde(default)]
    pub threads: BTreeMap<String, ImportedThread>,
    /// Projects (cwd) where the user said not to ask.
    #[serde(default)]
    pub quiet_projects: Vec<String>,
}

pub fn record_path(home: &Path) -> PathBuf {
    home.join("imports").join("imports.json")
}

pub fn read_record(home: &Path) -> Record {
    std::fs::read_to_string(record_path(home))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn write_record(home: &Path, record: &Record) -> std::io::Result<()> {
    let path = record_path(home);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(record).unwrap_or_default())?;
    std::fs::rename(tmp, path)
}

pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// The extraction state of a thread, and whether to ask about it now.
pub fn should_ask(record: &Record, thread_id: &str, cwd: Option<&str>) -> bool {
    let Some(t) = record.threads.get(thread_id) else { return false };
    if !matches!(t.extraction, Extraction::None | Extraction::Later) {
        return false;
    }
    let cwd = cwd.unwrap_or(t.cwd.as_str());
    !record.quiet_projects.iter().any(|q| same_path(q, cwd))
}

fn same_path(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.replace('\\', "/").trim_end_matches('/').to_lowercase();
    norm(a) == norm(b)
}

// ---- finding what to import ----------------------------------------------------------------------

/// The roots to look in: `claude` is `~/.claude`, `codex` is `~/.codex` (CODEX_HOME).
#[derive(Debug, Clone)]
pub struct Roots {
    pub claude: Option<PathBuf>,
    pub codex: Option<PathBuf>,
}

impl Roots {
    pub fn from_env() -> Self {
        let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
        let codex = std::env::var_os("CODEX_HOME").map(PathBuf::from).or_else(|| home.as_ref().map(|h| h.join(".codex")));
        Self { claude: home.map(|h| h.join(".claude")), codex }
    }
}

fn modified(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn jsonl_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if depth > 0 {
                jsonl_files(&p, depth - 1, out);
            }
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
}

/// The first `cwd` in the first lines of a session file.
fn head_cwd(path: &Path, source: Source) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(f).lines().take(400) {
        let line = line.ok()?;
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        let cwd = match source {
            Source::Claude => v.get("cwd"),
            Source::Codex => v.pointer("/payload/cwd"),
        };
        if let Some(c) = cwd.and_then(|c| c.as_str()) {
            return Some(c.to_string());
        }
    }
    None
}

/// A Claude title from the end of the file (titles are appended as the session goes).
fn tail_title(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(256 * 1024);
    f.seek(std::io::SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let (mut custom, mut ai) = (None, None);
    for line in text.lines() {
        if !line.contains("-title\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("custom-title") => custom = v.get("customTitle").and_then(|t| t.as_str()).map(str::to_string),
            Some("ai-title") => ai = v.get("aiTitle").and_then(|t| t.as_str()).map(str::to_string),
            _ => {}
        }
    }
    custom.or(ai)
}

/// The sessions that can be imported, newest first.
pub fn detect(roots: &Roots, home: &Path) -> Vec<Found> {
    let record = read_record(home);
    let mut files: Vec<(Source, PathBuf)> = Vec::new();
    if let Some(c) = &roots.claude {
        let mut v = Vec::new();
        jsonl_files(&c.join("projects"), 1, &mut v);
        files.extend(v.into_iter().map(|p| (Source::Claude, p)));
    }
    if let Some(c) = &roots.codex {
        let mut v = Vec::new();
        jsonl_files(&c.join("sessions"), 3, &mut v);
        jsonl_files(&c.join("archived_sessions"), 3, &mut v);
        files.extend(v.into_iter().map(|p| (Source::Codex, p)));
    }
    let mut out: Vec<Found> = files
        .into_iter()
        .filter_map(|(source, path)| {
            let meta = std::fs::metadata(&path).ok()?;
            let key = path.to_string_lossy().to_string();
            let done = record.files.get(&key);
            let changed = done.is_some_and(|d| d.size_bytes != meta.len());
            Some(Found {
                source,
                cwd: head_cwd(&path, source),
                title: if source == Source::Claude { tail_title(&path) } else { None },
                size_bytes: meta.len(),
                modified: modified(&meta),
                thread_id: done.filter(|_| !changed).map(|d| d.thread_id.clone()),
                changed,
                path: key,
            })
        })
        .collect();
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out
}

/// The memory and instruction files of Claude Code and Codex.
pub fn detect_memories(roots: &Roots) -> Vec<Memory> {
    let mut out = Vec::new();
    let mut push = |source: Source, path: PathBuf, cwd: Option<String>| {
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.is_file() && meta.len() > 0 {
                out.push(Memory {
                    source,
                    path: path.to_string_lossy().to_string(),
                    size_bytes: meta.len(),
                    modified: modified(&meta),
                    cwd,
                });
            }
        }
    };
    if let Some(c) = &roots.claude {
        push(Source::Claude, c.join("CLAUDE.md"), None);
        if let Ok(rd) = std::fs::read_dir(c.join("projects")) {
            for project in rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
                let mem = project.join("memory");
                let Ok(files) = std::fs::read_dir(&mem) else { continue };
                // The project's folder: from one of its sessions.
                let mut sessions = Vec::new();
                jsonl_files(&project, 0, &mut sessions);
                let cwd = sessions.iter().find_map(|s| head_cwd(s, Source::Claude));
                let mut list: Vec<PathBuf> = files
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "md"))
                    .collect();
                list.sort();
                for p in list {
                    push(Source::Claude, p, cwd.clone());
                }
                if let Some(cwd) = &cwd {
                    push(Source::Claude, Path::new(cwd).join("CLAUDE.md"), Some(cwd.clone()));
                    push(Source::Claude, Path::new(cwd).join(".claude").join("CLAUDE.md"), Some(cwd.clone()));
                }
            }
        }
    }
    if let Some(c) = &roots.codex {
        push(Source::Codex, c.join("AGENTS.md"), None);
        let mut stack = vec![c.join("memories")];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for p in rd.flatten().map(|e| e.path()) {
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "md") {
                    push(Source::Codex, p, None);
                }
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|m| seen.insert(m.path.to_lowercase()));
    out
}

#[cfg(test)]
mod tests;
