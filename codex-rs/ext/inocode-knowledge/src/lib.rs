//! InoCode 知識データベース。
//!
//! 1 レコード 1 ファイル(Markdown + YAML front-matter)が正本。
//! プロジェクト: `<cwd>/.inocode/knowledge/<kind>/<id>.md`
//! 共通: `<INOCODE_HOME>/knowledge/<kind>/<id>.md`
//! 検索はファイルから都度組み立てる(索引は常にファイルから再生成できる状態で、
//! 日本語の部分一致を扱うため、FTS5 ではなく部分一致 + 重み付けで実装している)。

mod events;
mod guard;
pub mod kb;
mod record;
mod store;

pub use guard::GuardOrigin;
pub use guard::{GuardAction, action_from_tool_input, builtin_check, GuardHit, GuardVerdict, check_action, guard_message};
pub use events::{
    GuardEvent, GuardOutcome, GuardRuleStats, GuardTarget, guard_stats, KnowledgeRecorded, guard_events_path, list_guard_events,
    record_guard_hits, subscribe_guard, subscribe_recorded, KbChanged, subscribe_kb_changed,
    ToolUsed, subscribe_tool_used, notify_tool_used,
};
pub use record::{Confidence, FailureDetail, Kind, MatchSpec, Record, Scope, Severity, Status};
pub use store::{ListQuery, Store, SummaryRow};

use std::path::{Path, PathBuf};

/// `INOCODE_HOME`(未設定なら `~/.inocode`)。
pub fn inocode_home() -> PathBuf {
    codex_inocode_flags::inocode_home()
}

/// 現在の cwd と共通領域に対する Store。
pub fn store_for(cwd: &Path) -> Store {
    let home = inocode_home();
    Store::new(Some(project_root_for(cwd, &home)), home).with_session_cwd(cwd.to_path_buf())
}

/// The folder whose `.inocode/kb` holds the knowledge and evidence of a session in `cwd` (the
/// pages, the evidence, the failures shown in the tabs): the project the session is in, not a
/// folder above it. In a git repository: the nearest folder up to the top of the repository that
/// already has `.inocode/kb` or `project.json`, else the top of the repository. Outside git: `cwd`
/// itself (a folder above, such as Documents, is another project). The guard's rules still reach
/// down from a folder above (`project_root_for`).
pub fn kb_root_for(cwd: &Path, home: &Path) -> PathBuf {
    let top = cwd.ancestors().find(|d| d.join(".git").exists());
    kb_root_from(cwd, home, top)
}

/// `kb_root_for` with the top of the repository given (`None`: not in git).
pub(crate) fn kb_root_from(cwd: &Path, home: &Path, top: Option<&Path>) -> PathBuf {
    let Some(top) = top else {
        return cwd.to_path_buf();
    };
    for dir in cwd.ancestors() {
        let inocode = dir.join(".inocode");
        if inocode != *home && (inocode.join("kb").is_dir() || inocode.join("project.json").is_file()) {
            return dir.to_path_buf();
        }
        if dir == top {
            break;
        }
    }
    top.to_path_buf()
}

/// `kb_root_for` with this machine's home.
pub fn kb_root(cwd: &Path) -> PathBuf {
    kb_root_for(cwd, &inocode_home())
}

/// プロジェクトの知識の置き場の親: `cwd` かその上位で、最初に `.inocode/knowledge` を持つフォルダ。
/// 無ければ `cwd` そのもの。共通領域(`<home>/knowledge`)は数えない。
/// 親フォルダに置いた規則が、その下のリポジトリで開いたセッションにも効く。
pub fn project_root_for(cwd: &Path, home: &Path) -> PathBuf {
    let global = home.join("knowledge");
    for dir in cwd.ancestors() {
        let inocode = dir.join(".inocode");
        if inocode == *home {
            continue;
        }
        let candidate = inocode.join("knowledge");
        if (candidate != global && candidate.is_dir())
            || inocode.join("kb").is_dir()
            || inocode.join("project.json").is_file()
        {
            return dir.to_path_buf();
        }
    }
    cwd.to_path_buf()
}

/// 重大度 `block` の有効な rule を、1 行 1 件(理由付き)のリストにして返す。
/// 圧縮のあと・モデル切り替えのあとの再注入に使う。該当が無ければ空文字列。
/// `knowledge.enabled` / `knowledge.pinned_rules` が false なら空文字列。
pub fn pinned_rules_text(cwd: &Path) -> String {
    let f = codex_inocode_flags::load();
    if !(f.knowledge_enabled && f.knowledge_pinned_rules) {
        return String::new();
    }
    store_for(cwd).pinned_rules_text()
}

/// 再注入の対象の規則そのもの(フラグに関係なく返す。GUI の表示用)。
pub fn pinned_rules(cwd: &Path) -> Vec<Record> {
    store_for(cwd).pinned_rules()
}

/// 引き継ぎパック: 禁止事項、現在のタスクに関連する issue、直近の decision、
/// 関連する failure_case、未解決の点。
/// `knowledge.enabled` が false なら空文字列。
pub fn build_handoff_pack(cwd: &Path, task: &str) -> String {
    if !codex_inocode_flags::load().knowledge_enabled {
        return String::new();
    }
    store_for(cwd).build_handoff_pack(task)
}

/// 失敗例を記録する。同じ署名の未撤回の失敗例があればその ID を返す(重複作成しない)。
/// `knowledge.enabled` が false なら何もせず None。
pub fn record_failure_case(
    cwd: &Path,
    signature: &str,
    symptom: &str,
    root_cause: Option<&str>,
    do_not: Option<&str>,
    what_worked: Option<&str>,
    created_by: &str,
) -> std::io::Result<Option<String>> {
    if !codex_inocode_flags::load().knowledge_enabled {
        return Ok(None);
    }
    let store = store_for(cwd);
    let id = store.record_failure_case(
        signature,
        symptom,
        root_cause,
        do_not,
        what_worked,
        created_by,
    )?;
    events::notify_recorded(&store, &id);
    Ok(Some(id))
}

/// 失敗した呼び出しの詳細(プロジェクト、目的、実行したこと、結果、条件、原因)つきで失敗例を
/// 記録する。同じ署名の未撤回の失敗例があればその ID を返す(詳細は上書きしない)。
pub fn record_failure_case_with_conditions(
    cwd: &Path,
    signature: &str,
    symptom: &str,
    detail: &FailureDetail,
    created_by: &str,
) -> std::io::Result<Option<String>> {
    if !codex_inocode_flags::load().knowledge_enabled {
        return Ok(None);
    }
    let store = store_for(cwd);
    let id = store.record_failure_case_with_conditions(signature, symptom, detail, created_by)?;
    events::notify_recorded(&store, &id);
    if let Some(root) = store.project_root() {
        events::notify_kb_changed(root, "failure");
    }
    Ok(Some(id))
}

/// 失敗例の呼び出しがのちに成功した: その失敗例に `stateDependent: true` と、成功時の条件
/// (`worksWhen`)を付ける。付けたら true。
pub fn mark_failure_state_dependent(
    cwd: &Path,
    id: &str,
    works_when: &str,
) -> std::io::Result<bool> {
    if !codex_inocode_flags::load().knowledge_enabled {
        return Ok(false);
    }
    store_for(cwd).mark_state_dependent(id, works_when)
}

/// 呼び出し `<tool> <args>` が成功した: signature がその呼び出しそのもの、または
/// `その呼び出し + " exit="` で始まる未撤回で未設定の失敗例に `stateDependent` を付ける。
pub fn mark_call_failures_state_dependent(
    cwd: &Path,
    call: &str,
    works_when: &str,
) -> std::io::Result<Vec<String>> {
    if !codex_inocode_flags::load().knowledge_enabled {
        return Ok(Vec::new());
    }
    store_for(cwd).mark_state_dependent_for_call(call, works_when)
}

/// PreToolUse ガード用: cwd のプロジェクトと共通領域の rule に照合する。
/// `knowledge.enabled` / `knowledge.guard` が false なら何も合わない(空の判定)。
pub fn guard_check(cwd: &Path, action: &GuardAction) -> GuardVerdict {
    guard_check_with(&codex_inocode_flags::load(), &store_for(cwd), action)
}

/// 新しい禁止事項(`.inocode/kb/prohibitions`)の照合。緩和(原文参照つき)が効いているものは、
/// 無効なら外し、警告にするなら警告へ、「この条件の行為だけ許す」なら条件に合う行為だけ外す。
/// 禁止の本文は変えない。判定はホストの決定的な処理で、モデルの文面は見ない。
fn add_prohibition_hits(root: &Path, action: &GuardAction, verdict: &mut GuardVerdict) {
    for x in kb::active_prohibitions(root) {
        let rule = kb::prohibition_rule(&x);
        let Some(spec) = &rule.match_spec else { continue };
        if !guard::spec_matches(spec, action) {
            continue;
        }
        let mut severity = rule.severity.unwrap_or(Severity::Warn);
        let mut allowed = false;
        for r in kb::active_relaxations(root, &x.id, action.thread_id.as_deref()) {
            match r.effect.as_str() {
                "disable" => allowed = true,
                "warnOnly" => severity = Severity::Warn,
                "allowOnly" if kb::allow_matches(&r, action) => allowed = true,
                _ => {}
            }
        }
        if allowed {
            continue;
        }
        let hit = GuardHit {
            origin: Some(kb::hit_origin(&x)),
            available_scopes: guard::all_scopes(),
            id: x.id.clone(),
            title: x.title.clone(),
            severity,
            reason: if x.reason.is_empty() { x.title.clone() } else { x.reason.clone() },
            scope: Scope::Project,
        };
        match severity {
            Severity::Block => verdict.blocked.push(hit),
            Severity::Warn => verdict.warnings.push(hit),
        }
    }
}

/// フラグと Store を明示するガード(テスト用に分離)。
pub fn guard_check_with(
    flags: &codex_inocode_flags::InocodeFlags,
    store: &Store,
    action: &GuardAction,
) -> GuardVerdict {
    if !(flags.knowledge_enabled && flags.knowledge_guard) {
        return GuardVerdict::default();
    }
    let mut verdict = check_action(store, action);
    if let Some(root) = store.project_root() {
        add_prohibition_hits(root, action, &mut verdict);
    }
    let home = inocode_home();
    if codex_inocode_flags::guard_protect_self_from(&home) {
        verdict.blocked.extend(guard::builtin_check(action, &home));
    }
    verdict
}

/// One INOCODE_HOME for every test of the crate (the environment is process-wide, so two
/// modules with their own homes would trample each other's files).
#[cfg(test)]
pub(crate) fn test_home() -> PathBuf {
    static H: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let d = H.get_or_init(|| tempfile::tempdir().unwrap());
    // SAFETY: the value never changes.
    unsafe { std::env::set_var("INOCODE_HOME", d.path()) };
    d.path().to_path_buf()
}

#[cfg(test)]
mod event_tests {
    use super::*;

    /// 全テストで同じ INOCODE_HOME を使う(環境変数はプロセス共通)。
    fn shared_home() {
        crate::test_home();
    }

    #[test]
    fn guard_hits_are_logged_and_broadcast() {
        shared_home();
        let mut rx = subscribe_guard();
        let verdict = GuardVerdict {
            blocked: vec![GuardHit {
                origin: None,
                available_scopes: Vec::new(),
                id: "rule-0001".into(),
                title: "force push".into(),
                severity: Severity::Block,
                reason: "履歴が消える".into(),
                scope: Scope::Project,
            }],
            warnings: vec![],
        };
        let t = GuardTarget {
            thread_id: "th1".into(),
            tool: "shell".into(),
            target: "git push --force".into(),
            cwd: "/w".into(),
            ..Default::default()
        };
        let evs = record_guard_hits(&verdict, &t);
        assert_eq!(evs.len(), 1);
        assert_eq!(rx.try_recv().unwrap().rule_id, "rule-0001");
        let (list, next) = list_guard_events(Some("/w"), Some("th1"), 10, 0);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].result, GuardOutcome::Blocked);
        assert!(next.is_none());
        assert!(list_guard_events(Some("/other"), None, 10, 0).0.is_empty());
    }

    #[test]
    fn failure_case_is_recorded_once_notified_and_in_handoff() {
        shared_home();
        let cwd = tempfile::tempdir().unwrap();
        let mut rx = subscribe_recorded();
        let id = record_failure_case(cwd.path(), "same_failure:cargo test", "E0432", None, None, None, "advisor")
            .unwrap()
            .expect("recorded");
        let again = record_failure_case(cwd.path(), "same_failure:cargo test", "E0432", None, None, None, "advisor")
            .unwrap();
        assert_eq!(again.as_deref(), Some(id.as_str()));
        let note = rx.try_recv().unwrap();
        assert_eq!(note.id, id);
        assert_eq!(note.kind, Kind::FailureCase);
        let pack = build_handoff_pack(cwd.path(), "cargo test");
        assert!(pack.contains("引き継ぎパック"));
        // The advisor's automatic record is a candidate: kept, but never injected.
        assert!(!pack.contains("same_failure:cargo test") && !pack.contains(&id));
    }

    #[test]
    fn a_failure_case_keeps_its_detail_and_learns_it_depends_on_the_state() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let store = Store::new(Some(cwd.path().to_path_buf()), home.path().to_path_buf());
        let detail = FailureDetail {
            project: Some("/repo (main)".into()),
            purpose: Some("flash the device".into()),
            attempted: Some("exec_command: adb shell
cwd: /repo".into()),
            outcome: Some("exit code: 1
output tail: error: no devices".into()),
            conditions: Some("latest calls: fastboot flash ; adb shell".into()),
            cause: None,
        };
        let id = store
            .record_failure_case_with_conditions(
                "adb shell exit=1 | error: no devices",
                "error: no devices/emulators found",
                &detail,
                "advisor",
            )
            .unwrap();
        let rec = store.read(Scope::Project, &id).unwrap();
        assert_eq!(rec.project.as_deref(), Some("/repo (main)"));
        assert_eq!(rec.purpose.as_deref(), Some("flash the device"));
        assert_eq!(rec.attempted.as_deref(), Some("exec_command: adb shell
cwd: /repo"));
        assert_eq!(rec.outcome.as_deref(), Some("exit code: 1
output tail: error: no devices"));
        assert_eq!(rec.conditions.as_deref(), Some("latest calls: fastboot flash ; adb shell"));
        assert_eq!(rec.state_dependent, None);
        // A repeat does not overwrite the detail.
        let other = FailureDetail { conditions: Some("other".into()), ..Default::default() };
        let again = store
            .record_failure_case_with_conditions(
                "adb shell exit=1 | error: no devices",
                "x",
                &other,
                "advisor",
            )
            .unwrap();
        assert_eq!(again, id);
        assert_eq!(
            store.read(Scope::Project, &id).unwrap().conditions.as_deref(),
            Some("latest calls: fastboot flash ; adb shell")
        );
        // The same call worked later, in another state.
        assert!(store.mark_state_dependent(&id, "latest calls: adb reboot ; adb shell").unwrap());
        let rec = store.read(Scope::Project, &id).unwrap();
        assert_eq!(rec.state_dependent, Some(true));
        assert_eq!(rec.works_when.as_deref(), Some("latest calls: adb reboot ; adb shell"));
        // Marking twice, or a missing case, does nothing.
        assert!(!store.mark_state_dependent(&id, "again").unwrap());
        assert!(!store.mark_state_dependent("failure_case-9999", "x").unwrap());
    }

    #[test]
    fn a_later_success_marks_the_cases_of_that_call_only() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let store = Store::new(Some(cwd.path().to_path_buf()), home.path().to_path_buf());
        let rec = |sig: &str| {
            store
                .record_failure_case_with_conditions(sig, "s", &FailureDetail::default(), "advisor")
                .unwrap()
        };
        let hit_exit = rec("exec_command {\"cmd\":\"adb shell\"} exit=1 | error: no devices");
        let hit_plain = rec("exec_command {\"cmd\":\"adb shell\"}");
        let other_args = rec("exec_command {\"cmd\":\"adb shell ls\"} exit=1 | x");
        let other_tool = rec("shell {\"cmd\":\"adb shell\"} exit=1 | x");
        let marked = store
            .mark_state_dependent_for_call("exec_command {\"cmd\":\"adb shell\"}", "after reboot")
            .unwrap();
        assert_eq!(marked.len(), 2);
        for id in [&hit_exit, &hit_plain] {
            assert_eq!(store.read(Scope::Project, id).unwrap().state_dependent, Some(true));
        }
        for id in [&other_args, &other_tool] {
            assert_eq!(store.read(Scope::Project, id).unwrap().state_dependent, None);
        }
    }

    #[test]
    fn write_fills_internal_values_counts_and_delete() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let store = Store::new(Some(cwd.path().to_path_buf()), home.path().to_path_buf());
        let mut f = Record::new(Kind::FailureCase, "ロック失敗");
        f.symptom = Some("database is locked\n詳細".into());
        let fid = store.write(f).unwrap();
        let f = store.read(Scope::Project, &fid).unwrap();
        assert_eq!(f.signature.as_deref(), Some("database is locked"));
        assert_eq!(f.created_by.as_deref(), Some("user"));
        let mut r = Record::new(Kind::Rule, "rm 禁止");
        r.match_spec = Some(MatchSpec {
            commands: vec!["rm -rf".into()],
            ..Default::default()
        });
        let rid = store.write(r).unwrap();
        assert_eq!(
            store.read(Scope::Project, &rid).unwrap().enforced_by.as_deref(),
            Some("hook")
        );
        let rid2 = store.write(Record::new(Kind::Rule, "注意")).unwrap();
        assert_eq!(
            store.read(Scope::Project, &rid2).unwrap().enforced_by.as_deref(),
            Some("prompt")
        );
        let counts = store.counts(Some(Scope::Project));
        assert_eq!(counts["rule"], 2);
        assert_eq!(counts["failure_case"], 1);
        assert_eq!(counts["unverified"], 3);
        store.delete(Scope::Project, &rid2).unwrap();
        assert!(store.read(Scope::Project, &rid2).is_none());
        assert!(store.delete(Scope::Project, &rid2).is_err());
    }
}
