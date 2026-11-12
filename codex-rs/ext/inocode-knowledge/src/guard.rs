use crate::record::{Scope, Severity};
use crate::store::Store;
use globset::Glob;
use regex::Regex;

/// ツール実行前に調べる操作。
#[derive(Debug, Clone, Default)]
pub struct GuardAction {
    /// 実行するコマンド文字列。
    pub command: Option<String>,
    /// 読み書きするファイルのパス。
    pub paths: Vec<String>,
    /// ツール名(例 `Bash`、`apply_patch`、`mcp__server__tool`)。
    pub tool: Option<String>,
    /// MCP ツールなら、サーバー名とツール名。
    pub mcp: Option<(String, String)>,
    /// 使用中のモデル名(スラッグ)。呼び出し側が設定する。
    pub model: Option<String>,
    /// 書き込む内容(ファイルの書き込み・編集・パッチの本文)。
    pub content: Option<String>,
    /// 操作が走るスレッド。緩和の「この依頼の間」「このセッション」の判定に使う。
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardHit {
    pub id: String,
    pub title: String,
    pub severity: Severity,
    pub reason: String,
    pub scope: Scope,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GuardVerdict {
    /// `block` に合ったもの(1 件でもあれば拒否)。
    pub blocked: Vec<GuardHit>,
    /// `warn` に合ったもの(モデルへ警告として返す)。
    pub warnings: Vec<GuardHit>,
}

/// 操作に合う有効な rule を探す。
pub fn check_action(store: &Store, action: &GuardAction) -> GuardVerdict {
    let mut verdict = GuardVerdict::default();
    for rule in store.live_rules() {
        let Some(spec) = &rule.match_spec else {
            continue;
        };
        if !store.applies(&rule, &action.paths) || !spec_matches(spec, action) {
            continue;
        }
        let h = GuardHit {
            id: rule.id.clone(),
            title: rule.title.clone(),
            severity: rule.severity.unwrap_or(Severity::Warn),
            reason: rule
                .reason
                .clone()
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| rule.title.clone()),
            scope: rule.scope,
        };
        match h.severity {
            Severity::Block => verdict.blocked.push(h),
            Severity::Warn => verdict.warnings.push(h),
        }
    }
    verdict
}

/// 条件の組(commands/paths、tools、mcp、models、content)は、書かれているものが全部合うこと(AND)。
/// commands と paths は従来どおり、どちらかが合えばよい(OR)。
pub(crate) fn spec_matches(spec: &crate::record::MatchSpec, action: &GuardAction) -> bool {
    if !spec.has_criteria() {
        return false;
    }
    if action
        .paths
        .iter()
        .any(|p| spec.exclude_paths.iter().any(|g| path_matches(g, p)))
    {
        return false;
    }
    if !spec.commands.is_empty() || !spec.paths.is_empty() {
        let mut hit = false;
        if let Some(cmd) = &action.command {
            hit |= spec.commands.iter().any(|p| command_matches(p, cmd));
        }
        for path in &action.paths {
            hit |= spec.paths.iter().any(|g| path_matches(g, path));
        }
        if !hit {
            return false;
        }
    }
    if !spec.tools.is_empty()
        && !action
            .tool
            .as_deref()
            .is_some_and(|t| spec.tools.iter().any(|g| name_glob(g, t)))
    {
        return false;
    }
    if !spec.mcp.is_empty()
        && !action.mcp.as_ref().is_some_and(|(server, tool)| {
            spec.mcp.iter().any(|g| mcp_matches(g, server, tool))
        })
    {
        return false;
    }
    if !spec.models.is_empty()
        && !action
            .model
            .as_deref()
            .is_some_and(|m| spec.models.iter().any(|g| name_glob(g, m)))
    {
        return false;
    }
    // `content` looks at what is written and at the command line that is run.
    if !spec.content.is_empty()
        && !action
            .content
            .as_deref()
            .into_iter()
            .chain(action.command.as_deref())
            .any(|c| {
                spec.content.iter().any(|p| {
                    Regex::new(p.strip_prefix("re:").unwrap_or(p)).is_ok_and(|r| r.is_match(c))
                })
            })
    {
        return false;
    }
    true
}

/// `appliesTo`(絞り込みのパス)のどれかが、候補(cwd や操作のパス)のどれかに合うか。
/// パターンそのもの、および配下(`<pattern>/**`)に合えばよい。
pub(crate) fn applies_to_matches(patterns: &[String], candidates: &[String]) -> bool {
    patterns.iter().any(|p| {
        let base = p.trim().trim_end_matches(['/', '\\']);
        if base.is_empty() {
            return false;
        }
        let under = format!("{base}/**");
        candidates
            .iter()
            .any(|c| path_matches(base, c) || path_matches(&under, c))
    })
}

/// 大文字小文字を区別しない glob(ツール名・モデル名)。
fn name_glob(glob: &str, name: &str) -> bool {
    globset::GlobBuilder::new(glob)
        .case_insensitive(true)
        .build()
        .is_ok_and(|g| g.compile_matcher().is_match(name))
}

/// `server`(そのサーバーの全ツール)または `server/tool`(glob 可)。
fn mcp_matches(pattern: &str, server: &str, tool: &str) -> bool {
    let full = if pattern.contains('/') {
        pattern.to_string()
    } else {
        format!("{pattern}/*")
    };
    name_glob(&full, &format!("{server}/{tool}"))
}

const WRITE_TOOLS: [&str; 11] = [
    "apply_patch",
    "write",
    "edit",
    "multiedit",
    "write_file",
    "edit_file",
    "create_file",
    "str_replace",
    "str_replace_editor",
    "str_replace_based_edit_tool",
    "notebookedit",
];

fn builtin_hit(reason: &str) -> GuardHit {
    GuardHit {
        id: "builtin-protect-self".into(),
        title: "ガード自体の保護".into(),
        severity: Severity::Block,
        reason: format!(
            "{reason}(`[inocode.guard] protect_self = false` で切り替えられる。ユーザーの明示の許可が要る)"
        ),
        scope: Scope::Global,
    }
}

fn re(pattern: &str, text: &str) -> bool {
    Regex::new(pattern).is_ok_and(|r| r.is_match(text))
}

/// ガード自体の保護(組み込み。規則ではなく機構)。規則・フックの設定の書き換えと、検証の無効化を止める。
/// `inocode_home` は共通領域の場所(その `knowledge` も守る)。
pub fn builtin_check(action: &GuardAction, inocode_home: &std::path::Path) -> Vec<GuardHit> {
    const WRITE_VERB: &str = r"(?i)(\brm\b|\bdel\b|\berase\b|\brmdir\b|\brd\b|remove-item|\bmv\b|\bmove\b|move-item|rename|set-content|add-content|out-file|clear-content|\btee\b|\bsed\s+-i|\bcp\b|\bcopy\b|copy-item|truncate|\bgit\s+(checkout|restore|clean|rm)\b|(?:^|[^0-9&])>>?\s*\S)";
    let mut hits = Vec::new();
    let norm = |s: &str| s.replace('\\', "/").to_lowercase();
    let home_knowledge = norm(&inocode_home.join("knowledge").to_string_lossy());
    let is_knowledge = |s: &str| {
        let n = norm(s);
        n.contains(".inocode/knowledge") || n.contains(&home_knowledge)
    };
    let tool = action.tool.as_deref().unwrap_or("").to_lowercase();
    let writes = action.content.is_some() || WRITE_TOOLS.contains(&tool.as_str());
    if writes {
        for path in &action.paths {
            let n = norm(path);
            if is_knowledge(path) {
                hits.push(builtin_hit(
                    "知識 DB(規則)のファイルを直接書き換えない。規則の変更は、知識の書き込みの手順で行う",
                ));
            }
            let name = n.rsplit('/').next().unwrap_or("");
            if matches!(name, "hooks.json" | "hooks.toml") || n.contains("/.codex/hooks/") {
                hits.push(builtin_hit("フックの設定ファイルを書き換えない"));
            }
            if name == "config.toml"
                && action.content.as_deref().is_some_and(|c| {
                    re(r"(?i)\[\s*(inocode\.)?hooks|\bhooks\s*=|protect_self", c)
                })
            {
                hits.push(builtin_hit("設定の [hooks] とガードの保護の設定を書き換えない"));
            }
        }
    }
    if let Some(cmd) = &action.command {
        let lower = norm(cmd);
        if is_knowledge(cmd) && re(WRITE_VERB, cmd) {
            hits.push(builtin_hit("知識 DB(規則)のファイルを、コマンドで書き換え・削除しない"));
        }
        if (lower.contains("hooks.json") || lower.contains("hooks.toml")) && re(WRITE_VERB, cmd) {
            hits.push(builtin_hit("フックの設定ファイルを、コマンドで書き換え・削除しない"));
        }
        if lower.contains("config.toml")
            && (lower.contains("hooks") || lower.contains("protect_self"))
            && re(WRITE_VERB, cmd)
        {
            hits.push(builtin_hit(
                "設定の [hooks] とガードの保護の設定を、コマンドで書き換えない",
            ));
        }
        let verification = [
            (r"(?i)--no-verify\b", "検証(フック)を飛ばす --no-verify は使わない"),
            (
                r"(?i)\bgit\s+commit\b[^;&|\n]*\s-[A-Za-z]*n[A-Za-z]*(\s|$)",
                "検証(フック)を飛ばす git commit -n は使わない",
            ),
            (r"(?i)core\.hooksPath", "core.hooksPath を変更しない(フックの無効化になる)"),
            (r"(?i)\bHUSKY\s*=\s*0\b", "HUSKY=0 でフックを無効化しない"),
            (r"(?:^|[\s;&|(])SKIP\s*=", "SKIP= でフックを飛ばさない"),
            (
                r"(?i)\bgit\s+config\b[^;&|\n]*hooks",
                "git config でフックの設定を変更しない",
            ),
        ];
        for (pattern, reason) in verification {
            if re(pattern, cmd) {
                hits.push(builtin_hit(reason));
            }
        }
    }
    hits.dedup_by(|a, b| a.reason == b.reason);
    hits
}

/// モデルへ返す文面。何も合わなければ None。
pub fn guard_message(verdict: &GuardVerdict) -> Option<String> {
    if verdict.blocked.is_empty() && verdict.warnings.is_empty() {
        return None;
    }
    let mut out = String::new();
    if !verdict.blocked.is_empty() {
        out.push_str("この操作は禁止事項に合うため実行しません:\n");
        for h in &verdict.blocked {
            out.push_str(&format!("- [{}] {}: {}\n", h.id, h.title, h.reason));
        }
    }
    if !verdict.warnings.is_empty() {
        out.push_str("警告(禁止事項の注意):\n");
        for h in &verdict.warnings {
            out.push_str(&format!("- [{}] {}: {}\n", h.id, h.title, h.reason));
        }
    }
    Some(out)
}

/// `re:` で始まれば正規表現。それ以外は、コマンド全体または `&&` `;` `|` で区切った
/// 各部分の前方一致。
fn command_matches(pattern: &str, command: &str) -> bool {
    if let Some(re) = pattern.strip_prefix("re:") {
        return Regex::new(re).is_ok_and(|r| r.is_match(command));
    }
    let pat = pattern.trim();
    if pat.is_empty() {
        return false;
    }
    if command.trim_start().starts_with(pat) {
        return true;
    }
    command
        .split(['\n', ';', '|', '&'])
        .any(|seg| seg.trim_start().starts_with(pat))
}

fn path_matches(glob: &str, path: &str) -> bool {
    let norm = path.replace('\\', "/");
    let Ok(g) = Glob::new(&glob.replace('\\', "/")) else {
        return false;
    };
    let m = g.compile_matcher();
    if m.is_match(&norm) {
        return true;
    }
    // 絶対パスでも、相対の glob に末尾側で合えばよい。
    let parts: Vec<&str> = norm.split('/').collect();
    (1..parts.len()).any(|i| m.is_match(parts[i..].join("/")))
}

/// ファイルの書き込み・編集の本文になるフィールド(`content`、`new_string` など)を集める。
fn content_from_input(obj: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    const KEYS: [&str; 7] = [
        "content",
        "contents",
        "text",
        "file_text",
        "new_string",
        "new_str",
        "new_text",
    ];
    let mut parts: Vec<String> = KEYS
        .iter()
        .filter_map(|k| obj.get(*k).and_then(|v| v.as_str()))
        .map(str::to_string)
        .collect();
    if let Some(edits) = obj.get("edits").and_then(|v| v.as_array()) {
        for edit in edits.iter().filter_map(|e| e.as_object()) {
            parts.extend(
                KEYS.iter()
                    .filter_map(|k| edit.get(*k).and_then(|v| v.as_str()))
                    .map(str::to_string),
            );
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// ツール入力(JSON)から、照合する操作を取り出す。
/// `command`(文字列または配列)はコマンド、`path` / `file_path` / `paths` / `workdir` 系はパス。
/// apply_patch の本文からは `*** Add/Update/Delete File:` と `*** Move to:` のパスを取る。
pub fn action_from_tool_input(tool_name: &str, input: &serde_json::Value) -> GuardAction {
    let mut action = GuardAction {
        tool: Some(tool_name.to_string()),
        mcp: tool_name.strip_prefix("mcp__").and_then(|rest| {
            rest.split_once("__")
                .map(|(server, tool)| (server.to_string(), tool.to_string()))
        }),
        ..Default::default()
    };
    let Some(obj) = input.as_object() else {
        return action;
    };
    action.content = content_from_input(obj);
    let is_patch = tool_name == "apply_patch";
    if let Some(cmd) = obj.get("command").or_else(|| obj.get("cmd")) {
        let text = match cmd {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Array(a) => Some(
                a.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
        };
        if let Some(t) = text {
            if is_patch || t.contains("*** Begin Patch") {
                action.content = Some(t.clone());
                for line in t.lines() {
                    for p in ["*** Add File: ", "*** Update File: ", "*** Delete File: ", "*** Move to: "] {
                        if let Some(path) = line.strip_prefix(p) {
                            action.paths.push(path.trim().to_string());
                        }
                    }
                }
            } else {
                action.command = Some(t);
            }
        }
    }
    for key in ["path", "file_path", "filePath", "file", "target_file"] {
        if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
            action.paths.push(s.to_string());
        }
    }
    if let Some(a) = obj.get("paths").and_then(|v| v.as_array()) {
        action
            .paths
            .extend(a.iter().filter_map(|v| v.as_str()).map(str::to_string));
    }
    action
}
