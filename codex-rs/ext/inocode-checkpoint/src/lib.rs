//! Restore points for file changes inside the working folder.
//!
//! What a restore point holds is only the files that are about to change, never a copy of the
//! whole tree:
//!
//! * `apply_patch` knows the paths it touches, so exactly those files are saved before the change.
//! * A shell command can touch anything, so before it runs the files that are already changed or
//!   untracked (`git status --porcelain=v1 -z`, which uses fsmonitor / the untracked cache when
//!   the repository has them) are saved. Files the command changes afterwards are clean in `HEAD`;
//!   their earlier content is in git, so at restore time they are taken from the `HEAD` that was
//!   recorded with the restore point.
//!
//! Nothing here touches the user's branches, index or history: `git status` runs with
//! `GIT_OPTIONAL_LOCKS=0`, and the store lives under `INOCODE_HOME/checkpoints` (content addressed:
//! the same content is stored once).
//!
//! Left out from the start: files of 10 MiB or more, untracked directories holding 200 or more
//! files, anything `.gitignore` covers, the tool's own state (`.inocode`, `.build`, `target`,
//! `node_modules`, `.git`), symlinks. Outside a git repository only the files `apply_patch` touches
//! are saved. A restore point that would take longer than the time budget is not made at all, and
//! the caller is told in one line.
//!
//! Not restorable, by nature: anything outside the working folder, processes, the network, account
//! operations.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

/// Directory names that are never saved (the tool's own state and dependency trees).
const EXCLUDED_COMPONENTS: &[&str] = &[".git", ".inocode", ".build", "target", "node_modules"];

#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Files of this size or more are not saved.
    pub max_file_bytes: u64,
    /// An untracked directory holding this many files or more is not saved.
    pub max_untracked_dir_files: usize,
    /// A restore point that would take longer than this is not made.
    pub budget: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_file_bytes: 10 * 1024 * 1024,
            max_untracked_dir_files: 200,
            budget: Duration::from_secs(240),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    ApplyPatch,
    Shell,
    /// Made by a restore, from the state it replaced. Never a start point for another restore.
    Safety,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum EntryState {
    /// The file existed with this content (blob id = SHA-256).
    Saved { blob: String, size: u64 },
    /// The file did not exist.
    Absent,
    /// Not saved, with the reason.
    Skipped { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    /// Relative to the point's root, `/` separated.
    pub path: String,
    #[serde(flatten)]
    pub state: EntryState,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub files: u64,
    /// Bytes newly written to the store (content already stored costs nothing).
    pub bytes_written: u64,
    pub millis: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Point {
    pub id: String,
    pub thread_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub ts_ms: u64,
    pub kind: Kind,
    pub root: String,
    /// `HEAD` when the point was made (git repositories only).
    pub head: Option<String>,
    /// Shell points: every path that was changed or untracked at that moment (a directory ending
    /// in `/` stands for all of it), saved or not.
    #[serde(default)]
    pub dirty_before: Vec<String>,
    pub entries: Vec<Entry>,
    pub stats: Stats,
}

/// Where a point is made from.
#[derive(Debug, Clone)]
pub struct Origin {
    pub thread_id: String,
    pub turn_id: String,
    pub call_id: String,
    /// The working folder (inside or at the top of a git repository, or not in one at all).
    pub cwd: PathBuf,
}

/// What `capture_*` returns.
#[derive(Debug)]
pub enum Captured {
    Made(Point),
    /// The time budget ran out; no point was made. The text is the one line to show.
    Omitted(String),
    /// Nothing to save (for example a shell command in a folder that is not a git repository).
    Nothing,
}

#[derive(Debug, Clone)]
pub enum Target {
    /// Back to the state before this turn's first change.
    Turn(String),
    /// Back to the state before this restore point's change.
    Point(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreOutcome {
    pub restored: Vec<String>,
    pub removed: Vec<String>,
    pub skipped: Vec<String>,
    /// The state that was replaced, saved as a point so the restore can itself be undone.
    pub safety_point_id: Option<String>,
    pub from_points: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

static SEQ: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn new_id() -> String {
    format!(
        "{:013}-{:05}-{:04}",
        now_ms(),
        std::process::id() % 100_000,
        SEQ.fetch_add(1, Ordering::Relaxed) % 10_000
    )
}

fn safe(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

impl Store {
    /// A store under `<home>/checkpoints`.
    pub fn at_home(home: &Path) -> Self {
        Self {
            dir: home.join("checkpoints"),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn blob_path(&self, blob: &str) -> PathBuf {
        self.dir.join("blobs").join(&blob[..2]).join(blob)
    }

    fn point_path(&self, id: &str) -> PathBuf {
        self.dir.join("points").join(format!("{}.json", safe(id)))
    }

    /// Stores `data` once; returns (blob id, bytes newly written).
    fn put_blob(&self, data: &[u8]) -> Option<(String, u64)> {
        let blob = hex(&Sha256::digest(data));
        let path = self.blob_path(&blob);
        if path.exists() {
            return Some((blob, 0));
        }
        std::fs::create_dir_all(path.parent()?).ok()?;
        let tmp = path.with_extension(format!("tmp{}", SEQ.fetch_add(1, Ordering::Relaxed)));
        std::fs::write(&tmp, data).ok()?;
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            if !path.exists() {
                return None;
            }
        }
        Some((blob, data.len() as u64))
    }

    fn get_blob(&self, blob: &str) -> Option<Vec<u8>> {
        if blob.len() != 64 || !blob.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        std::fs::read(self.blob_path(blob)).ok()
    }

    fn save_point(&self, point: &Point) -> bool {
        let path = self.point_path(&point.id);
        let Some(parent) = path.parent() else {
            return false;
        };
        std::fs::create_dir_all(parent).is_ok()
            && serde_json::to_vec(point)
                .ok()
                .is_some_and(|bytes| std::fs::write(path, bytes).is_ok())
    }

    /// The points of a thread, oldest first.
    pub fn list(&self, thread_id: &str) -> Vec<Point> {
        let Ok(dir) = std::fs::read_dir(self.dir.join("points")) else {
            return Vec::new();
        };
        let mut points: Vec<Point> = dir
            .filter_map(Result::ok)
            .filter_map(|e| std::fs::read(e.path()).ok())
            .filter_map(|b| serde_json::from_slice::<Point>(&b).ok())
            .filter(|p| p.thread_id == thread_id)
            .collect();
        points.sort_by(|a, b| a.id.cmp(&b.id));
        points
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// git
// ---------------------------------------------------------------------------

/// Runs git in `root`, killing it when `deadline` passes. `None` on failure or timeout.
fn git(root: &Path, args: &[&str], deadline: Instant) -> Option<Vec<u8>> {
    git_status(root, args, deadline).and_then(|(ok, out)| ok.then_some(out))
}

/// (exited with success, stdout)
fn git_status(root: &Path, args: &[&str], deadline: Instant) -> Option<(bool, Vec<u8>)> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["-c", "core.quotepath=false"])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let remaining = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(remaining) {
        Ok(buf) => {
            let status = child.wait().ok()?;
            Some((status.success(), buf))
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => p,
    }
}

/// The root a restore point works in: the top of the git repository holding `cwd`, else `cwd`.
/// The bool says whether it is a git repository.
fn find_root(cwd: &Path) -> (PathBuf, bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    if let Some(out) = git(cwd, &["rev-parse", "--show-toplevel"], deadline) {
        let text = String::from_utf8_lossy(&out);
        let top = text.trim();
        if !top.is_empty()
            && let Ok(canon) = Path::new(top).canonicalize()
        {
            return (strip_verbatim(canon), true);
        }
    }
    (
        strip_verbatim(cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf())),
        false,
    )
}

/// `abs` relative to `root` (both resolved), `/` separated. `None` when it is outside.
fn rel_to_root(root: &Path, abs: &Path) -> Option<String> {
    let mut probe = abs.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let base = loop {
        if let Ok(c) = probe.canonicalize() {
            break strip_verbatim(c);
        }
        tail.push(probe.file_name()?.to_os_string());
        probe = probe.parent()?.to_path_buf();
    };
    let mut full = base;
    for part in tail.iter().rev() {
        full.push(part);
    }
    let rel = full.strip_prefix(root).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn is_excluded(rel: &str) -> bool {
    rel.split('/')
        .any(|part| EXCLUDED_COMPONENTS.contains(&part))
}

fn head_of(root: &Path, deadline: Instant) -> Option<String> {
    let out = git(root, &["rev-parse", "HEAD"], deadline)?;
    let head = String::from_utf8_lossy(&out).trim().to_string();
    (!head.is_empty()).then_some(head)
}

/// Every path `git status` reports as changed or untracked (a wholly untracked directory is one
/// entry ending in `/`). `None` on failure or timeout.
fn dirty_paths(root: &Path, deadline: Instant) -> Option<Vec<String>> {
    let out = git(
        root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=normal"],
        deadline,
    )?;
    let mut paths = Vec::new();
    let mut fields = out.split(|b| *b == 0).filter(|f| !f.is_empty());
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue;
        }
        let (x, y) = (field[0], field[1]);
        paths.push(String::from_utf8_lossy(&field[3..]).into_owned());
        // A rename or copy carries its source as the next field.
        if matches!(x, b'R' | b'C') || matches!(y, b'R' | b'C') {
            if let Some(from) = fields.next() {
                paths.push(String::from_utf8_lossy(from).into_owned());
            }
        }
    }
    Some(paths)
}

enum Expanded {
    Files(Vec<String>),
    TooMany,
}

/// The files of a wholly untracked directory (`.gitignore` respected), or `TooMany`.
fn expand_untracked_dir(
    root: &Path,
    dir: &str,
    cfg: &Config,
    deadline: Instant,
) -> Option<Expanded> {
    let out = git(
        root,
        &["ls-files", "-z", "--others", "--exclude-standard", "--", dir],
        deadline,
    )?;
    let files: Vec<String> = out
        .split(|b| *b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| String::from_utf8_lossy(f).into_owned())
        .collect();
    Some(if files.len() >= cfg.max_untracked_dir_files {
        Expanded::TooMany
    } else {
        Expanded::Files(files)
    })
}

/// Of `paths`, those `.gitignore` covers.
fn ignored_among(root: &Path, paths: &[String], deadline: Instant) -> BTreeSet<String> {
    use std::io::Write;
    let mut ignored = BTreeSet::new();
    if paths.is_empty() {
        return ignored;
    }
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["check-ignore", "-z", "--stdin"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let Ok(mut child) = command.spawn() else {
        return ignored;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let mut input = Vec::new();
        for p in paths {
            input.extend_from_slice(p.as_bytes());
            input.push(0);
        }
        let _ = stdin.write_all(&input);
    }
    let Some(mut stdout) = child.stdout.take() else {
        return ignored;
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(buf) => {
            let _ = child.wait();
            for f in buf.split(|b| *b == 0).filter(|f| !f.is_empty()) {
                ignored.insert(String::from_utf8_lossy(f).into_owned());
            }
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    ignored
}

// ---------------------------------------------------------------------------
// capture
// ---------------------------------------------------------------------------

struct Timeout;

/// Saves the current state of `rels` (relative paths) into `store`.
fn capture_files(
    store: &Store,
    root: &Path,
    rels: &[String],
    cfg: &Config,
    deadline: Instant,
) -> Result<(Vec<Entry>, u64), Timeout> {
    let mut entries = Vec::new();
    let mut written = 0u64;
    for rel in rels {
        if Instant::now() >= deadline {
            return Err(Timeout);
        }
        let abs = root.join(rel);
        let state = match std::fs::symlink_metadata(&abs) {
            Err(_) => EntryState::Absent,
            Ok(meta) if meta.file_type().is_symlink() => EntryState::Skipped {
                reason: "symlink".into(),
            },
            Ok(meta) if meta.is_dir() => continue,
            Ok(meta) if meta.len() >= cfg.max_file_bytes => EntryState::Skipped {
                reason: format!("{} bytes (limit {})", meta.len(), cfg.max_file_bytes),
            },
            Ok(_) => match std::fs::read(&abs) {
                Ok(data) => match store.put_blob(&data) {
                    Some((blob, new)) => {
                        written += new;
                        EntryState::Saved {
                            blob,
                            size: data.len() as u64,
                        }
                    }
                    None => EntryState::Skipped {
                        reason: "could not store".into(),
                    },
                },
                Err(err) => EntryState::Skipped {
                    reason: format!("unreadable: {err}"),
                },
            },
        };
        entries.push(Entry {
            path: rel.clone(),
            state,
        });
    }
    Ok((entries, written))
}

fn finish_point(
    store: &Store,
    origin: &Origin,
    kind: Kind,
    root: &Path,
    head: Option<String>,
    dirty_before: Vec<String>,
    entries: Vec<Entry>,
    written: u64,
    started: Instant,
) -> Captured {
    let files = entries
        .iter()
        .filter(|e| matches!(e.state, EntryState::Saved { .. }))
        .count() as u64;
    let point = Point {
        id: new_id(),
        thread_id: origin.thread_id.clone(),
        turn_id: origin.turn_id.clone(),
        call_id: origin.call_id.clone(),
        ts_ms: now_ms(),
        kind,
        root: root.to_string_lossy().into_owned(),
        head,
        dirty_before,
        entries,
        stats: Stats {
            files,
            bytes_written: written,
            millis: started.elapsed().as_millis() as u64,
        },
    };
    if store.save_point(&point) {
        Captured::Made(point)
    } else {
        Captured::Nothing
    }
}

fn omitted_line(cfg: &Config) -> String {
    format!(
        "Restore point skipped: it would take longer than {} seconds (large repository). The change goes ahead without one.",
        cfg.budget.as_secs()
    )
}

/// Before `apply_patch` changes `paths` (absolute): saves those files.
pub fn capture_paths(
    store: &Store,
    origin: &Origin,
    paths: &[PathBuf],
    cfg: &Config,
) -> Captured {
    let started = Instant::now();
    let deadline = started + cfg.budget;
    let (root, is_git) = find_root(&origin.cwd);
    let mut rels: Vec<String> = paths
        .iter()
        .filter_map(|p| rel_to_root(&root, p))
        .filter(|r| !is_excluded(r))
        .collect();
    rels.sort();
    rels.dedup();
    if is_git {
        let ignored = ignored_among(&root, &rels, deadline);
        rels.retain(|r| !ignored.contains(r));
    }
    if rels.is_empty() {
        return Captured::Nothing;
    }
    let head = if is_git { head_of(&root, deadline) } else { None };
    match capture_files(store, &root, &rels, cfg, deadline) {
        Ok((entries, written)) => finish_point(
            store,
            origin,
            Kind::ApplyPatch,
            &root,
            head,
            Vec::new(),
            entries,
            written,
            started,
        ),
        Err(Timeout) => Captured::Omitted(omitted_line(cfg)),
    }
}

/// Before a shell command runs: saves the files that are already changed or untracked.
/// `Nothing` outside a git repository.
pub fn capture_shell(store: &Store, origin: &Origin, cfg: &Config) -> Captured {
    let started = Instant::now();
    let deadline = started + cfg.budget;
    let (root, is_git) = find_root(&origin.cwd);
    if !is_git {
        return Captured::Nothing;
    }
    let Some(dirty) = dirty_paths(&root, deadline) else {
        return if Instant::now() >= deadline {
            Captured::Omitted(omitted_line(cfg))
        } else {
            Captured::Nothing
        };
    };
    let mut files = Vec::new();
    for path in &dirty {
        if is_excluded(path) {
            continue;
        }
        if path.ends_with('/') {
            match expand_untracked_dir(&root, path, cfg, deadline) {
                Some(Expanded::Files(list)) => files.extend(list),
                Some(Expanded::TooMany) => {}
                None => return Captured::Omitted(omitted_line(cfg)),
            }
        } else {
            files.push(path.clone());
        }
    }
    files.retain(|f| !is_excluded(f));
    files.sort();
    files.dedup();
    let head = head_of(&root, deadline);
    match capture_files(store, &root, &files, cfg, deadline) {
        Ok((entries, written)) => finish_point(
            store,
            origin,
            Kind::Shell,
            &root,
            head,
            dirty,
            entries,
            written,
            started,
        ),
        Err(Timeout) => Captured::Omitted(omitted_line(cfg)),
    }
}

// ---------------------------------------------------------------------------
// restore
// ---------------------------------------------------------------------------

enum Wanted {
    Blob(String),
    Absent,
}

/// The state of `path` in commit `head`, as bytes; `None` when the commit has no such file.
fn blob_in_head(root: &Path, head: &str, path: &str, deadline: Instant) -> Option<Vec<u8>> {
    git(root, &["cat-file", "blob", &format!("{head}:{path}")], deadline)
}

fn changed_between(root: &Path, from: &str, deadline: Instant) -> Vec<String> {
    git(
        root,
        &["diff", "--name-only", "-z", from, "HEAD"],
        deadline,
    )
    .map(|out| {
        out.split(|b| *b == 0)
            .filter(|f| !f.is_empty())
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .collect()
    })
    .unwrap_or_default()
}

/// Puts the files back as they were before `target`. A point of the current state is made first, so
/// the restore can be undone.
pub fn restore(
    store: &Store,
    thread_id: &str,
    target: &Target,
    cfg: &Config,
) -> Result<RestoreOutcome, String> {
    let points: Vec<Point> = store
        .list(thread_id)
        .into_iter()
        .filter(|p| p.kind != Kind::Safety)
        .collect();
    // A safety point (made by an earlier restore) brings back exactly what that restore replaced.
    let safety_point = match target {
        Target::Point(id) => store
            .list(thread_id)
            .into_iter()
            .find(|p| &p.id == id && p.kind == Kind::Safety),
        Target::Turn(_) => None,
    };
    let start = if safety_point.is_some() {
        0
    } else {
        match target {
            Target::Turn(turn) => points.iter().position(|p| &p.turn_id == turn),
            Target::Point(id) => points.iter().position(|p| &p.id == id),
        }
        .ok_or_else(|| "no restore point for that target".to_string())?
    };
    let root_str = safety_point
        .as_ref()
        .map_or_else(|| points[start].root.clone(), |p| p.root.clone());
    let range: Vec<&Point> = match &safety_point {
        Some(p) => vec![p],
        None => points[start..]
            .iter()
            .filter(|p| p.root == root_str)
            .collect(),
    };
    let root = PathBuf::from(&root_str);
    let deadline = Instant::now() + cfg.budget;

    // The earliest saved state of each file wins.
    // Once a shell point exists, nothing saved after it can be the earliest state of a file it did
    // not already hold: such a file may have been changed by a command in between, and what it
    // was at that first shell point is in git (its `HEAD`). Those paths go to the git step below.
    let first_shell_index = range.iter().position(|p| p.kind == Kind::Shell);
    let mut wanted: BTreeMap<String, Wanted> = BTreeMap::new();
    let mut skipped: BTreeSet<String> = BTreeSet::new();
    let mut late_paths: BTreeSet<String> = BTreeSet::new();
    for (index, point) in range.iter().enumerate() {
        let late = first_shell_index.is_some_and(|first| index > first);
        for entry in &point.entries {
            if late && !wanted.contains_key(&entry.path) {
                match &entry.state {
                    EntryState::Skipped { .. } => {
                        skipped.insert(entry.path.clone());
                    }
                    _ => {
                        late_paths.insert(entry.path.clone());
                    }
                }
                continue;
            }
            match &entry.state {
                EntryState::Saved { blob, .. } => {
                    wanted
                        .entry(entry.path.clone())
                        .or_insert_with(|| Wanted::Blob(blob.clone()));
                }
                EntryState::Absent => {
                    wanted.entry(entry.path.clone()).or_insert(Wanted::Absent);
                }
                EntryState::Skipped { .. } => {
                    skipped.insert(entry.path.clone());
                }
            }
        }
    }

    // Files a shell command changed after the earliest shell point were clean in that point's
    // `HEAD` (or did not exist): take them from git.
    let mut from_git: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::new();
    if let Some(first_shell) = range.iter().find(|p| p.kind == Kind::Shell)
        && let Some(head) = &first_shell.head
    {
        let before: BTreeSet<&String> = first_shell.dirty_before.iter().collect();
        let dirs: Vec<&String> = first_shell
            .dirty_before
            .iter()
            .filter(|p| p.ends_with('/'))
            .collect();
        let mut candidates: BTreeSet<String> = BTreeSet::new();
        if let Some(now_dirty) = dirty_paths(&root, deadline) {
            for path in now_dirty {
                if path.ends_with('/') {
                    if let Some(Expanded::Files(list)) =
                        expand_untracked_dir(&root, &path, cfg, deadline)
                    {
                        candidates.extend(list);
                    }
                } else {
                    candidates.insert(path);
                }
            }
        }
        candidates.extend(changed_between(&root, head, deadline));
        candidates.extend(late_paths.iter().cloned());
        for path in candidates {
            if is_excluded(&path)
                || wanted.contains_key(&path)
                || skipped.contains(&path)
                || before.contains(&path)
                || dirs.iter().any(|d| path.starts_with(d.as_str()))
            {
                continue;
            }
            let content = blob_in_head(&root, head, &path, deadline);
            from_git.insert(path, content);
        }
    }

    // Undo-of-undo: save what is there now for every path the restore will touch.
    let touched: Vec<String> = wanted
        .keys()
        .cloned()
        .chain(from_git.keys().cloned())
        .collect();
    let started = Instant::now();
    let origin = Origin {
        thread_id: thread_id.to_string(),
        turn_id: format!("restore-of:{}", range[0].turn_id),
        call_id: String::new(),
        cwd: root.clone(),
    };
    let safety_cfg = Config {
        budget: Duration::from_secs(86_400),
        ..*cfg
    };
    let safety_deadline = started + safety_cfg.budget;
    let safety_point_id = match capture_files(store, &root, &touched, &safety_cfg, safety_deadline) {
        Ok((entries, written)) if !entries.is_empty() => {
            let head = head_of(&root, safety_deadline);
            match finish_point(
                store,
                &origin,
                Kind::Safety,
                &root,
                head,
                Vec::new(),
                entries,
                written,
                started,
            ) {
                Captured::Made(p) => Some(p.id),
                _ => None,
            }
        }
        _ => None,
    };

    let mut outcome = RestoreOutcome {
        safety_point_id,
        from_points: range.iter().map(|p| p.id.clone()).collect(),
        skipped: skipped.into_iter().collect(),
        ..Default::default()
    };
    let write = |path: &str, data: &[u8]| -> bool {
        let abs = root.join(path);
        if let Some(parent) = abs.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(abs, data).is_ok()
    };
    let remove = |path: &str| -> bool {
        let abs = root.join(path);
        match std::fs::symlink_metadata(&abs) {
            Ok(m) if m.is_file() => std::fs::remove_file(abs).is_ok(),
            _ => false,
        }
    };
    for (path, want) in &wanted {
        match want {
            Wanted::Blob(blob) => match store.get_blob(blob) {
                Some(data) if write(path, &data) => outcome.restored.push(path.clone()),
                _ => outcome.skipped.push(path.clone()),
            },
            Wanted::Absent => {
                if remove(path) {
                    outcome.removed.push(path.clone());
                }
            }
        }
    }
    for (path, content) in &from_git {
        match content {
            Some(data) => {
                // Skip when it already matches.
                let same = std::fs::read(root.join(path)).is_ok_and(|now| &now == data);
                if !same {
                    if write(path, data) {
                        outcome.restored.push(path.clone());
                    } else {
                        outcome.skipped.push(path.clone());
                    }
                }
            }
            None => {
                if remove(path) {
                    outcome.removed.push(path.clone());
                }
            }
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn run(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        run(d.path(), &["init", "-q"]);
        run(d.path(), &["config", "user.email", "t@example.invalid"]);
        run(d.path(), &["config", "user.name", "t"]);
        run(d.path(), &["config", "core.autocrlf", "false"]);
        fs::write(d.path().join("a.txt"), "a0\n").unwrap();
        fs::write(d.path().join("b.txt"), "b0\n").unwrap();
        fs::write(d.path().join(".gitignore"), "ignored.log\n").unwrap();
        run(d.path(), &["add", "."]);
        run(d.path(), &["commit", "-q", "-m", "init"]);
        d
    }

    fn origin(cwd: &Path, turn: &str) -> Origin {
        Origin {
            thread_id: "t1".into(),
            turn_id: turn.into(),
            call_id: format!("call-{turn}"),
            cwd: cwd.to_path_buf(),
        }
    }

    fn made(c: Captured) -> Point {
        match c {
            Captured::Made(p) => p,
            other => panic!("expected a point, got {other:?}"),
        }
    }

    #[test]
    fn apply_patch_files_are_saved_and_restored() {
        let work = repo();
        let store_dir = tempfile::tempdir().unwrap();
        let store = Store::at_home(store_dir.path());
        let cfg = Config::default();
        let a = work.path().join("a.txt");
        let new = work.path().join("sub").join("new.txt");
        let p = made(capture_paths(&store, &origin(work.path(), "turn1"), &[a.clone(), new.clone()], &cfg));
        assert_eq!(p.kind, Kind::ApplyPatch);
        assert_eq!(p.stats.files, 1);
        fs::write(&a, "changed\n").unwrap();
        fs::create_dir_all(new.parent().unwrap()).unwrap();
        fs::write(&new, "created\n").unwrap();
        let out = restore(&store, "t1", &Target::Turn("turn1".into()), &cfg).unwrap();
        assert_eq!(fs::read_to_string(&a).unwrap(), "a0\n");
        assert!(!new.exists());
        assert_eq!(out.restored, vec!["a.txt".to_string()]);
        assert_eq!(out.removed, vec!["sub/new.txt".to_string()]);
        // The restore itself can be undone, with the safety point.
        let safety = out.safety_point_id.unwrap();
        restore(&store, "t1", &Target::Point(safety), &cfg).unwrap();
        assert_eq!(fs::read_to_string(&a).unwrap(), "changed
");
        assert_eq!(fs::read_to_string(&new).unwrap(), "created
");
    }

    #[test]
    fn the_restore_can_be_undone_through_its_safety_point() {
        let work = repo();
        let store_dir = tempfile::tempdir().unwrap();
        let store = Store::at_home(store_dir.path());
        let cfg = Config::default();
        let a = work.path().join("a.txt");
        made(capture_paths(&store, &origin(work.path(), "turn1"), &[a.clone()], &cfg));
        fs::write(&a, "agent version\n").unwrap();
        let out = restore(&store, "t1", &Target::Turn("turn1".into()), &cfg).unwrap();
        assert_eq!(fs::read_to_string(&a).unwrap(), "a0\n");
        let safety = store
            .list("t1")
            .into_iter()
            .find(|p| Some(&p.id) == out.safety_point_id.as_ref())
            .unwrap();
        let EntryState::Saved { blob, .. } = &safety.entries[0].state else {
            panic!()
        };
        assert_eq!(store.get_blob(blob).unwrap(), b"agent version\n");
    }

    #[test]
    fn shell_saves_dirty_files_and_restores_files_it_changed_from_head() {
        let work = repo();
        let store_dir = tempfile::tempdir().unwrap();
        let store = Store::at_home(store_dir.path());
        let cfg = Config::default();
        // Already dirty before the command: a.txt edited, scratch.txt untracked.
        fs::write(work.path().join("a.txt"), "my edit\n").unwrap();
        fs::write(work.path().join("scratch.txt"), "mine\n").unwrap();
        let p = made(capture_shell(&store, &origin(work.path(), "turn1"), &cfg));
        assert_eq!(p.kind, Kind::Shell);
        assert_eq!(p.stats.files, 2);
        // The command: edits both, edits a clean file, adds a file.
        fs::write(work.path().join("a.txt"), "shell edit\n").unwrap();
        fs::write(work.path().join("scratch.txt"), "shell scratch\n").unwrap();
        fs::write(work.path().join("b.txt"), "shell b\n").unwrap();
        fs::write(work.path().join("made.txt"), "made\n").unwrap();
        restore(&store, "t1", &Target::Turn("turn1".into()), &cfg).unwrap();
        assert_eq!(fs::read_to_string(work.path().join("a.txt")).unwrap(), "my edit\n");
        assert_eq!(fs::read_to_string(work.path().join("scratch.txt")).unwrap(), "mine\n");
        assert_eq!(fs::read_to_string(work.path().join("b.txt")).unwrap(), "b0\n");
        assert!(!work.path().join("made.txt").exists());
    }

    #[test]
    fn a_file_a_later_shell_point_saw_already_changed_still_comes_back_from_git() {
        let work = repo();
        let store_dir = tempfile::tempdir().unwrap();
        let store = Store::at_home(store_dir.path());
        let cfg = Config::default();
        made(capture_shell(&store, &origin(work.path(), "turn1"), &cfg));
        // Command 1 changes b.txt and creates d.txt; the next shell point sees both already changed.
        fs::write(work.path().join("b.txt"), "after command 1
").unwrap();
        fs::write(work.path().join("d.txt"), "d
").unwrap();
        made(capture_shell(&store, &origin(work.path(), "turn1"), &cfg));
        fs::write(work.path().join("b.txt"), "after command 2
").unwrap();
        restore(&store, "t1", &Target::Turn("turn1".into()), &cfg).unwrap();
        assert_eq!(fs::read_to_string(work.path().join("b.txt")).unwrap(), "b0
");
        assert!(!work.path().join("d.txt").exists());
    }

    #[test]
    fn a_commit_made_after_the_point_does_not_hide_the_change() {
        let work = repo();
        let store_dir = tempfile::tempdir().unwrap();
        let store = Store::at_home(store_dir.path());
        let cfg = Config::default();
        made(capture_shell(&store, &origin(work.path(), "turn1"), &cfg));
        fs::write(work.path().join("b.txt"), "committed by the agent\n").unwrap();
        run(work.path(), &["commit", "-q", "-am", "agent commit"]);
        restore(&store, "t1", &Target::Turn("turn1".into()), &cfg).unwrap();
        assert_eq!(fs::read_to_string(work.path().join("b.txt")).unwrap(), "b0\n");
    }

    #[test]
    fn exclusions_are_applied_from_the_start() {
        let work = repo();
        let store_dir = tempfile::tempdir().unwrap();
        let store = Store::at_home(store_dir.path());
        let cfg = Config {
            max_file_bytes: 100,
            max_untracked_dir_files: 5,
            ..Config::default()
        };
        fs::write(work.path().join("big.bin"), vec![1u8; 500]).unwrap();
        fs::write(work.path().join("ignored.log"), "x").unwrap();
        fs::create_dir_all(work.path().join("node_modules/dep")).unwrap();
        fs::write(work.path().join("node_modules/dep/i.js"), "x").unwrap();
        fs::create_dir_all(work.path().join("many")).unwrap();
        for i in 0..6 {
            fs::write(work.path().join(format!("many/f{i}.txt")), "x").unwrap();
        }
        fs::create_dir_all(work.path().join("few")).unwrap();
        for i in 0..2 {
            fs::write(work.path().join(format!("few/f{i}.txt")), "x").unwrap();
        }
        let p = made(capture_shell(&store, &origin(work.path(), "turn1"), &cfg));
        let saved: Vec<&str> = p
            .entries
            .iter()
            .filter(|e| matches!(e.state, EntryState::Saved { .. }))
            .map(|e| e.path.as_str())
            .collect();
        assert_eq!(saved, vec!["few/f0.txt", "few/f1.txt"]);
        assert!(p.entries.iter().any(|e| e.path == "big.bin"
            && matches!(e.state, EntryState::Skipped { .. })));
        // The big file and the big directory are left alone by a restore.
        fs::write(work.path().join("many/f0.txt"), "changed").unwrap();
        restore(&store, "t1", &Target::Turn("turn1".into()), &cfg).unwrap();
        assert_eq!(fs::read_to_string(work.path().join("many/f0.txt")).unwrap(), "changed");
        assert_eq!(fs::read(work.path().join("big.bin")).unwrap().len(), 500);
    }

    /// A folder that is not inside any git repository (the temp dir may be inside one).
    fn plain_dir() -> Option<tempfile::TempDir> {
        let candidates = [
            std::env::temp_dir(),
            PathBuf::from(std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?),
        ];
        candidates.into_iter().find_map(|base| {
            let dir = tempfile::tempdir_in(base).ok()?;
            (!find_root(dir.path()).1).then_some(dir)
        })
    }

    #[test]
    fn outside_git_only_patched_files_are_saved() {
        let Some(work) = plain_dir() else {
            return;
        };
        let store_dir = tempfile::tempdir().unwrap();
        let store = Store::at_home(store_dir.path());
        let cfg = Config::default();
        assert!(matches!(
            capture_shell(&store, &origin(work.path(), "turn1"), &cfg),
            Captured::Nothing
        ));
        let f = work.path().join("note.txt");
        fs::write(&f, "v1").unwrap();
        made(capture_paths(&store, &origin(work.path(), "turn1"), &[f.clone()], &cfg));
        fs::write(&f, "v2").unwrap();
        restore(&store, "t1", &Target::Turn("turn1".into()), &cfg).unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), "v1");
    }

    #[test]
    fn the_same_content_is_stored_once() {
        let work = repo();
        let store_dir = tempfile::tempdir().unwrap();
        let store = Store::at_home(store_dir.path());
        let cfg = Config::default();
        fs::write(work.path().join("a.txt"), "same\n").unwrap();
        fs::write(work.path().join("b.txt"), "same\n").unwrap();
        let first = made(capture_shell(&store, &origin(work.path(), "turn1"), &cfg));
        let second = made(capture_shell(&store, &origin(work.path(), "turn2"), &cfg));
        assert_eq!(first.stats.bytes_written, 5);
        assert_eq!(second.stats.bytes_written, 0);
    }

    #[test]
    fn a_point_over_the_time_budget_is_not_made() {
        let work = repo();
        let store_dir = tempfile::tempdir().unwrap();
        let store = Store::at_home(store_dir.path());
        let cfg = Config {
            budget: Duration::ZERO,
            ..Config::default()
        };
        fs::write(work.path().join("a.txt"), "x").unwrap();
        let out = capture_shell(&store, &origin(work.path(), "turn1"), &cfg);
        assert!(matches!(out, Captured::Omitted(line) if line.contains("skipped")));
        assert!(store.list("t1").is_empty());
    }

    #[test]
    fn the_users_branch_and_index_are_left_alone() {
        let work = repo();
        let store_dir = tempfile::tempdir().unwrap();
        let store = Store::at_home(store_dir.path());
        let cfg = Config::default();
        let before_index = fs::read(work.path().join(".git/index")).unwrap();
        fs::write(work.path().join("a.txt"), "edit\n").unwrap();
        made(capture_shell(&store, &origin(work.path(), "turn1"), &cfg));
        assert_eq!(fs::read(work.path().join(".git/index")).unwrap(), before_index);
        assert!(!work.path().join(".inocode").exists());
    }

    /// Measurement on a real repository: `INOCODE_CKPT_MEASURE_DIR=<dir> cargo test -p
    /// codex-inocode-checkpoint measure -- --ignored --nocapture`. Prints seconds and bytes of one
    /// shell restore point, twice (the second one shows what content sharing saves).
    #[test]
    #[ignore]
    fn measure_on_a_real_repository() {
        let Some(dir) = std::env::var_os("INOCODE_CKPT_MEASURE_DIR") else {
            return;
        };
        let store_dir = tempfile::tempdir().unwrap();
        let store = Store::at_home(store_dir.path());
        let cfg = Config::default();
        let t = Instant::now();
        let status = dirty_paths(&find_root(Path::new(&dir)).0, Instant::now() + cfg.budget);
        println!("git status alone: {:?}, {} paths", t.elapsed(), status.map_or(0, |s| s.len()));
        for round in 1..=2 {
            let t = Instant::now();
            let out = capture_shell(&store, &origin(Path::new(&dir), &format!("m{round}")), &cfg);
            let wall = t.elapsed();
            match out {
                Captured::Made(p) => println!(
                    "round {round}: {wall:?}; files saved {}, entries {}, dirty paths {}, bytes newly written {}",
                    p.stats.files,
                    p.entries.len(),
                    p.dirty_before.len(),
                    p.stats.bytes_written
                ),
                other => println!("round {round}: {wall:?}; {other:?}"),
            }
        }
        fn dir_size(p: &Path) -> u64 {
            std::fs::read_dir(p)
                .map(|rd| {
                    rd.filter_map(Result::ok)
                        .map(|e| match e.metadata() {
                            Ok(m) if m.is_dir() => dir_size(&e.path()),
                            Ok(m) => m.len(),
                            Err(_) => 0,
                        })
                        .sum()
                })
                .unwrap_or(0)
        }
        println!("store size on disk: {} bytes", dir_size(store.dir()));
    }
}
