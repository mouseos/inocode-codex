use super::*;
use crate::guard::GuardAction;

/// One INOCODE_HOME for all tests of this module (the environment is process-wide).
fn shared_home() -> PathBuf {
    crate::test_home()
}

/// A project folder that has its own `.inocode/kb`, so discovery stops here even when the temp
/// directory lives inside a repository.
fn project() -> (tempfile::TempDir, String) {
    shared_home();
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join(".inocode").join("kb")).unwrap();
    let cwd = d.path().to_string_lossy().into_owned();
    (d, cwd)
}

fn ok(method: &str, params: Value) -> Value {
    call(method, &params).unwrap_or_else(|e| panic!("{method}: {e:?}"))
}

fn err(method: &str, params: Value) -> KbError {
    call(method, &params).expect_err(method)
}

fn say(thread: &str, text: &str) -> u64 {
    codex_inocode_ledger::record_at(&shared_home(), thread, text).expect("recorded")
}

#[test]
fn project_id_is_written_once_and_named_after_the_folder() {
    let (d, cwd) = project();
    let a = ok("project/get", json!({ "cwd": cwd }));
    assert_eq!(a["ready"], true);
    let id1 = std::fs::read_to_string(d.path().join(".inocode").join("project.json")).unwrap();
    let b = ok("project/get", json!({ "cwd": cwd }));
    assert_eq!(a["name"], b["name"]);
    let id2 = std::fs::read_to_string(d.path().join(".inocode").join("project.json")).unwrap();
    assert_eq!(id1, id2, "the id is created once");
    assert!(id1.contains("projectId"));
}

#[test]
fn overview_uses_revisions_to_refuse_a_stale_write() {
    let (_d, cwd) = project();
    let first = ok("overview/read", json!({ "cwd": cwd }));
    assert_eq!((first["text"].as_str(), first["rev"].as_str()), (Some(""), Some("r0")));
    let w = ok("overview/write", json!({ "cwd": cwd, "text": "# P\n#方針", "baseRev": "r0" }));
    let rev = w["rev"].as_str().unwrap().to_string();
    let stale = err("overview/write", json!({ "cwd": cwd, "text": "x", "baseRev": "r0" }));
    assert_eq!(stale.code, CONFLICT);
    assert_eq!(stale.data.unwrap()["currentRev"], rev.as_str());
    let again = ok("overview/read", json!({ "cwd": cwd }));
    assert_eq!(again["text"], "# P\n#方針");
}

#[test]
fn pages_tags_links_backlinks_quotes_and_versions() {
    let (_d, cwd) = project();
    // an original source, in the evidence store of this home
    let store = codex_inocode_evidence::default_store();
    let src = store
        .import_bytes("SQLite docs", "text", "In WAL mode readers do not block writers.".as_bytes(), None, Some("https://sqlite.org/wal.html"))
        .unwrap();
    let body = format!(
        "#調査 #Ｗａｌ\n[WAL の設定] と [Markdown](http://x) を見る。C# は見ない。\n> quote:{}: readers do not block writers\n",
        src.id
    );
    let w = ok("page/write", json!({ "cwd": cwd, "title": "SQLite のロック", "body": body, "by": "user", "confirm": true }));
    assert_eq!(w["quoteErrors"], json!([]));
    assert_eq!(w["tags"], json!(["wal", "調査"]), "tags are normalized (full width, case)");
    assert_eq!(w["links"], json!([{ "exists": false, "title": "WAL の設定" }]));
    let r = ok("page/read", json!({ "cwd": cwd, "title": "SQLite のロック" }));
    assert_eq!(r["confirmed"], true);
    assert_eq!(r["quotes"][0]["matches"], true);
    // a page for the missing link, which then links back
    ok("page/write", json!({ "cwd": cwd, "title": "WAL の設定", "body": "戻る: [SQLite のロック]", "by": "model" }));
    let r = ok("page/read", json!({ "cwd": cwd, "title": "SQLite のロック" }));
    assert_eq!(r["links"][0]["exists"], true);
    assert_eq!(r["backlinks"], json!(["WAL の設定"]));
    let model_page = ok("page/read", json!({ "cwd": cwd, "title": "WAL の設定" }));
    assert_eq!(model_page["confirmed"], false, "what the model writes stays unconfirmed");

    // a quote that is not in the source is refused: nothing is saved
    let bad = ok(
        "page/write",
        json!({ "cwd": cwd, "title": "嘘", "body": format!("> quote:{}: writers block readers", src.id), "by": "model" }),
    );
    assert_eq!(bad["quoteErrors"].as_array().unwrap().len(), 1);
    assert_eq!(err("page/read", json!({ "cwd": cwd, "title": "嘘" })).code, NOT_FOUND);

    // versions: a stale base is a conflict; an update keeps the history
    let rev = r["rev"].as_str().unwrap();
    let stale = err("page/write", json!({ "cwd": cwd, "title": "SQLite のロック", "body": "x", "baseRev": "nope", "by": "user" }));
    assert_eq!(stale.code, CONFLICT);
    ok("page/write", json!({ "cwd": cwd, "title": "SQLite のロック", "body": "#調査\n更新", "baseRev": rev, "by": "user" }));
    let r2 = ok("page/read", json!({ "cwd": cwd, "title": "SQLite のロック" }));
    assert_eq!(r2["revs"].as_array().unwrap().len(), 2);
    assert_eq!(r2["confirmed"], false, "an edit without confirm is not confirmed");
    // creating an existing title is a conflict
    assert_eq!(err("page/write", json!({ "cwd": cwd, "title": "SQLite のロック", "body": "y", "by": "user" })).code, CONFLICT);

    // search: full text first, tag exact
    let found = ok("page/list", json!({ "cwd": cwd, "query": "更新" }));
    assert_eq!(found["pages"][0]["title"], "SQLite のロック");
    let tagged = ok("page/list", json!({ "cwd": cwd, "tag": "ＷＡＬ" }));
    assert_eq!(tagged["pages"].as_array().unwrap().len(), 0, "the edited page lost the tag");
    assert_eq!(ok("tag/list", json!({ "cwd": cwd, "prefix": "調" }))["tags"], json!([{ "tag": "調査", "count": 1 }]));
    // delete moves to trash
    assert_eq!(ok("page/delete", json!({ "cwd": cwd, "title": "WAL の設定" }))["movedTo"], "trash");
    assert_eq!(err("page/read", json!({ "cwd": cwd, "title": "WAL の設定" })).code, NOT_FOUND);
    // the computed index sees the unresolved link again
    let idx = ok("index/get", json!({ "cwd": cwd }));
    assert_eq!(idx["pageCount"], 1);
}

#[test]
fn a_prohibition_needs_the_users_own_words_from_the_ledger() {
    let (_d, cwd) = project();
    let seq = say("th-proh", "プロセスは名前で止めないで。あと、ログは消さないで。");
    let mk = |reference: Value, quote: &str| {
        json!({
            "cwd": cwd, "title": "プロセスを名前で止めない", "reason": "他のアプリを落とす", "strength": "block",
            "match": { "commands": ["re:taskkill\\s+/IM"] },
            "origin": { "kind": "ledger", "ledgerRef": reference, "quote": quote },
        })
    };
    let good = ok("prohibition/write", mk(json!({ "threadId": "th-proh", "seq": seq }), "名前で止めないで"));
    assert_eq!(good["id"], "proh-0001");
    // a quote that is not in the message, a message that is not in the ledger, no reference
    let wrong = err("prohibition/write", mk(json!({ "threadId": "th-proh", "seq": seq }), "止めてよい"));
    assert_eq!(wrong.code, INVALID);
    assert!(wrong.data.unwrap()["reason"].as_str().unwrap().contains("word for word"));
    assert_eq!(err("prohibition/write", mk(json!({ "threadId": "th-proh", "seq": 999 }), "x")).code, INVALID);
    assert_eq!(err("prohibition/write", mk(json!({ "threadId": "nobody", "seq": 1 }), "x")).code, INVALID);
    // legacy cannot be written, ui can
    let legacy = err("prohibition/write", json!({ "cwd": cwd, "title": "t", "strength": "warn", "origin": { "kind": "legacy" } }));
    assert_eq!(legacy.code, INVALID);
    let ui = ok("prohibition/write", json!({ "cwd": cwd, "title": "画面から", "strength": "warn", "origin": { "kind": "ui" } }));
    assert_eq!(ui["id"], "proh-0002");
    let list = ok("prohibition/list", json!({ "cwd": cwd, "status": "active" }));
    assert_eq!(list["prohibitions"].as_array().unwrap().len(), 2);
    assert_eq!(list["prohibitions"][0]["origin"]["kind"], "ledger");
    // retract keeps it, off the active list
    ok("prohibition/retract", json!({ "cwd": cwd, "id": "proh-0002" }));
    assert_eq!(ok("prohibition/list", json!({ "cwd": cwd, "status": "active" }))["prohibitions"].as_array().unwrap().len(), 1);
    assert_eq!(ok("prohibition/list", json!({ "cwd": cwd }))["prohibitions"].as_array().unwrap().len(), 2);
}

fn guard_store(cwd: &str) -> crate::Store {
    crate::Store::new(Some(PathBuf::from(cwd)), shared_home())
}

fn verdict_for(cwd: &str, command: &str, thread: &str) -> (usize, usize) {
    let action = GuardAction {
        command: Some(command.into()),
        thread_id: Some(thread.into()),
        ..Default::default()
    };
    let v = crate::guard_check_with(&codex_inocode_flags::InocodeFlags::default(), &guard_store(cwd), &action);
    let blocked = v.blocked.iter().filter(|h| h.id.starts_with("proh-")).count();
    let warned = v.warnings.iter().filter(|h| h.id.starts_with("proh-")).count();
    (blocked, warned)
}

#[test]
fn relaxations_come_from_the_users_words_and_the_guard_follows_them() {
    let (_d, cwd) = project();
    let th = "th-relax";
    let s1 = say(th, "taskkill は使わないで");
    ok(
        "prohibition/write",
        json!({
            "cwd": cwd, "title": "taskkill を使わない", "reason": "他のアプリを落とす", "strength": "block",
            "match": { "commands": ["re:^taskkill\\b"] },
            "origin": { "kind": "ledger", "ledgerRef": { "threadId": th, "seq": s1 }, "quote": "taskkill は使わないで" },
        }),
    );
    assert_eq!(verdict_for(&cwd, "taskkill /PID 42", th), (1, 0));

    // the user says: this one only, for this request
    let s2 = say(th, "今回だけ taskkill /PID 42 はやっていい");
    let reference = json!({ "threadId": th, "seq": s2 });
    let rel = |effect: &str, scope: &str, extra: Value| {
        let mut p = json!({
            "cwd": cwd, "prohibitionId": "proh-0001", "ledgerRef": reference, "quote": "taskkill /PID 42 はやっていい",
            "effect": effect, "scope": scope, "by": "user-text",
        });
        p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        p
    };
    // a made-up quote is not accepted
    let mut fake = rel("disable", "request", json!({}));
    fake["quote"] = json!("何でもやっていい");
    assert_eq!(err("relaxation/add", fake).code, INVALID);
    // allowOnly needs a condition
    assert_eq!(err("relaxation/add", rel("allowOnly", "request", json!({}))).code, INVALID);
    let added = ok("relaxation/add", rel("allowOnly", "request", json!({ "allow": { "commands": ["taskkill /PID 42"] } })));
    assert_eq!(added["id"], "rel-0001");
    // only the matching action is let through; the prohibition itself is unchanged
    assert_eq!(verdict_for(&cwd, "taskkill /PID 42", th), (0, 0));
    assert_eq!(verdict_for(&cwd, "taskkill /IM app.exe", th), (1, 0));
    // other threads are not covered by "this request"
    assert_eq!(verdict_for(&cwd, "taskkill /PID 42", "other-thread"), (1, 0));
    // the user writes again: "this request" is over
    say(th, "次の依頼です");
    assert_eq!(verdict_for(&cwd, "taskkill /PID 42", th), (1, 0));

    // warnOnly and disable, lasting until revoked
    let s3 = say(th, "taskkill は警告だけでいい。ずっと");
    let always = |effect: &str| {
        json!({ "cwd": cwd, "prohibitionId": "proh-0001", "ledgerRef": { "threadId": th, "seq": s3 }, "quote": "警告だけでいい",
                "effect": effect, "scope": "always", "by": "user-text" })
    };
    let w = ok("relaxation/add", always("warnOnly"));
    assert_eq!(verdict_for(&cwd, "taskkill /PID 42", th), (0, 1));
    let d = ok("relaxation/add", always("disable"));
    assert_eq!(verdict_for(&cwd, "taskkill /PID 42", th), (0, 0));
    ok("relaxation/revoke", json!({ "cwd": cwd, "prohibitionId": "proh-0001", "relaxationId": d["id"] }));
    assert_eq!(verdict_for(&cwd, "taskkill /PID 42", th), (0, 1));
    ok("relaxation/revoke", json!({ "cwd": cwd, "prohibitionId": "proh-0001", "relaxationId": w["id"] }));
    assert_eq!(verdict_for(&cwd, "taskkill /PID 42", th), (1, 0), "revoking restores the prohibition");

    // the history keeps every relaxation, revoked or not
    let list = ok("prohibition/list", json!({ "cwd": cwd }));
    let rels = list["prohibitions"][0]["relaxations"].as_array().unwrap();
    assert_eq!(rels.len(), 3);
    assert!(rels.iter().all(|r| r["quote"].is_string()));
    // the screen can relax without words: "request" then needs a reference, "always" does not
    let ui = err("relaxation/add", json!({ "cwd": cwd, "prohibitionId": "proh-0001", "effect": "disable", "scope": "request", "by": "user-ui" }));
    assert_eq!(ui.code, INVALID);
    ok("relaxation/add", json!({ "cwd": cwd, "prohibitionId": "proh-0001", "effect": "disable", "scope": "always", "by": "user-ui" }));
    assert_eq!(verdict_for(&cwd, "taskkill /PID 42", th), (0, 0));
}

#[test]
fn irreversible_false_prohibitions_are_not_in_the_fixed_list_but_still_guard() {
    let (_d, cwd) = project();
    let mk = |title: &str, irreversible: Value| {
        json!({ "cwd": cwd, "title": title, "reason": "理由", "strength": "block",
                "match": { "commands": [format!("re:^{title}")] }, "irreversible": irreversible, "origin": { "kind": "ui" } })
    };
    ok("prohibition/write", mk("dropdb", json!(true)));
    ok("prohibition/write", mk("makepreview", json!(false)));
    let text = guard_store(&cwd).pinned_rules_text();
    assert!(text.contains("dropdb") && !text.contains("makepreview"), "{text}");
    assert_eq!(verdict_for(&cwd, "makepreview now", "t").0, 1);
}

#[test]
fn checklists_per_role_without_a_gate() {
    let (_d, cwd) = project();
    let w = ok(
        "checklist/write",
        json!({ "cwd": cwd, "role": "実装", "items": [
            { "text": "変更の後にテストを走らせる", "when": "afterChange", "check": "machine" },
            { "text": "報告に残りを書く", "when": "beforeReport", "check": "self" } ] }),
    );
    assert_eq!(w["items"][0]["id"], "chk-0001");
    ok("checklist/write", json!({ "cwd": cwd, "role": "目視確認", "items": [{ "text": "画面を見る", "check": "user" }] }));
    let all = ok("checklist/list", json!({ "cwd": cwd }));
    assert_eq!(all["roles"], json!(["実装", "目視確認"]));
    assert_eq!(all["items"].as_array().unwrap().len(), 3);
    assert_eq!(all["items"][2]["id"], "chk-0003", "ids are unique across roles");
    let mine = ok("checklist/list", json!({ "cwd": cwd, "role": "実装" }));
    assert_eq!(mine["items"].as_array().unwrap().len(), 2);
    // by hand: user and self items; a machine item is the host's
    ok("checklist/setDone", json!({ "cwd": cwd, "role": "実装", "itemId": "chk-0002", "done": true }));
    assert_eq!(err("checklist/setDone", json!({ "cwd": cwd, "role": "実装", "itemId": "chk-0001", "done": true })).code, INVALID);
    let after = ok("checklist/list", json!({ "cwd": cwd, "role": "実装" }));
    assert_eq!(after["items"][1]["done"], true);
    // rewriting the list keeps the marks of the items that stay
    ok("checklist/write", json!({ "cwd": cwd, "role": "実装", "items": [
        { "id": "chk-0001", "text": "変更の後にテストを走らせる", "when": "afterChange", "check": "machine" },
        { "id": "chk-0002", "text": "報告に残りを書く", "when": "beforeReport", "check": "self" } ] }));
    assert_eq!(ok("checklist/list", json!({ "cwd": cwd, "role": "実装" }))["items"][1]["done"], true);
    let idx = ok("index/get", json!({ "cwd": cwd }));
    assert_eq!(idx["openChecklist"][0]["open"], 1);
    assert_eq!(err("checklist/write", json!({ "cwd": cwd, "role": "x", "items": [{ "text": "a", "when": "someday" }] })).code, INVALID);
}

#[test]
fn legacy_rules_migrate_one_at_a_time_and_stay_on_disk() {
    let (_d, cwd) = project();
    let store = guard_store(&cwd);
    let mut r = crate::record::Record::new(Kind::Rule, "プロセスを名前で止めない");
    r.severity = Some(Severity::Block);
    r.status = Status::Verified;
    r.reason = Some("他のアプリを落とす".into());
    r.match_spec = Some(MatchSpec { commands: vec!["re:^killall".into()], ..Default::default() });
    let id = store.write(r).unwrap();
    let mut old = crate::record::Record::new(Kind::Rule, "完了の前に変更を検証する");
    old.status = Status::Retracted;
    let old_id = store.write(old).unwrap();

    let rules = ok("legacy/rules", json!({ "cwd": cwd }));
    let by = |id: &str| rules["rules"].as_array().unwrap().iter().find(|r| r["id"] == id).unwrap().clone();
    assert_eq!(by(&id)["suggested"], "prohibition");
    assert_eq!(by(&id)["stillGuarding"], true);
    assert_eq!(by(&old_id)["suggested"], "checklist");
    assert_eq!(by(&old_id)["stillGuarding"], false);
    // until it is migrated, the old rule guards
    assert_eq!(store.live_rules().len(), 1);

    let m = ok("legacy/migrate", json!({ "cwd": cwd, "ruleId": id, "to": "prohibition" }));
    assert_eq!(m["origin"], "legacy");
    let p = ok("prohibition/list", json!({ "cwd": cwd }));
    assert_eq!(p["prohibitions"][0]["title"], "プロセスを名前で止めない");
    assert_eq!(p["prohibitions"][0]["origin"]["kind"], "legacy");
    assert_eq!(p["prohibitions"][0]["strength"], "block");
    // the old file is untouched; it just stops guarding by itself (the new one guards)
    assert!(store.read(Scope::Project, &id).is_some());
    assert_eq!(store.live_rules().len(), 0);
    assert_eq!(verdict_for(&cwd, "killall foo", "t").0, 1);
    assert_eq!(err("legacy/migrate", json!({ "cwd": cwd, "ruleId": id, "to": "skip" })).code, INVALID);
    ok("legacy/migrate", json!({ "cwd": cwd, "ruleId": old_id, "to": "checklist", "role": "全員" }));
    assert_eq!(ok("checklist/list", json!({ "cwd": cwd }))["items"][0]["origin"], format!("legacy:{old_id}"));
    assert_eq!(ok("legacy/rules", json!({ "cwd": cwd }))["rules"][0]["migrated"]["to"], "prohibition");
}

#[test]
fn the_ledger_view_and_failure_candidates() {
    let (_d, cwd) = project();
    let seq = say("th-view", "最初の依頼");
    say("th-view", "二つ目の依頼 ロック");
    let list = ok("ledger/list", json!({ "threadId": "th-view" }));
    assert_eq!(list["entries"][0]["text"], "二つ目の依頼 ロック", "newest first");
    assert_eq!(ok("ledger/list", json!({ "threadId": "th-view", "query": "ロック" }))["entries"].as_array().unwrap().len(), 1);
    let got = ok("ledger/get", json!({ "threadId": "th-view", "seq": seq }));
    assert_eq!(got["entry"]["text"], "最初の依頼");
    assert_eq!(got["laterEntries"], 1);
    assert_eq!(err("ledger/get", json!({ "threadId": "th-view", "seq": 99 })).code, NOT_FOUND);
    // the advisor's automatic failure cases are candidates
    let store = guard_store(&cwd);
    store.record_failure_case("cargo test exit=101", "boom", None, None, None, "advisor").unwrap();
    store.record_failure_case("manual one", "boom", None, None, None, "user").unwrap();
    let cases = ok("legacy/failureCases", json!({ "cwd": cwd }));
    let cands: Vec<bool> = cases["cases"].as_array().unwrap().iter().map(|c| c["candidate"].as_bool().unwrap()).collect();
    assert_eq!(cands, vec![true, false]);
    // and they are not injected through the handoff pack
    assert!(!store.build_handoff_pack("cargo test").contains("cargo test exit=101"));
}

#[test]
fn a_ui_relaxation_takes_the_newest_user_message_as_its_range() {
    let (_d, cwd) = project();
    let th = "th-ui-relax";
    say(th, "全部消す前に確認して");
    ok(
        "prohibition/write",
        json!({ "cwd": cwd, "title": "rm を使わない", "reason": "取り戻せない", "strength": "block",
                "match": { "commands": ["re:^rm\\b"] }, "origin": { "kind": "ui" } }),
    );
    assert_eq!(verdict_for(&cwd, "rm -rf build", th), (1, 0));
    // from the card: no words, only the thread; this request = until the next user message
    let rel = ok("relaxation/add", json!({
        "cwd": cwd, "prohibitionId": "proh-0001", "effect": "allowOnly",
        "allow": { "commands": ["rm -rf build"] }, "scope": "request", "by": "user-ui", "threadId": th }));
    assert_eq!(rel["id"], "rel-0001");
    assert_eq!(verdict_for(&cwd, "rm -rf build", th), (0, 0));
    assert_eq!(verdict_for(&cwd, "rm -rf src", th), (1, 0));
    say(th, "次の依頼");
    assert_eq!(verdict_for(&cwd, "rm -rf build", th), (1, 0), "the next user message ends it");
    // without a thread, request/session from the screen has no range
    let no_thread = err("relaxation/add", json!({
        "cwd": cwd, "prohibitionId": "proh-0001", "effect": "disable", "scope": "request", "by": "user-ui" }));
    assert_eq!(no_thread.code, INVALID);
    // an old rule cannot be relaxed before it is migrated
    let old = err("relaxation/add", json!({
        "cwd": cwd, "prohibitionId": "rule-0005", "effect": "disable", "scope": "always", "by": "user-ui" }));
    assert!(old.message.contains("migrated"));
    // the stopped card gets the origin and the ranges the user can choose
    let store = guard_store(&cwd);
    let action = GuardAction { command: Some("rm -rf src".into()), thread_id: Some(th.into()), ..Default::default() };
    let v = crate::guard_check_with(&codex_inocode_flags::InocodeFlags::default(), &store, &action);
    let hit = &v.blocked[0];
    assert_eq!(hit.origin.as_ref().unwrap().kind, "ui");
    assert_eq!(hit.available_scopes, vec!["request", "session", "always"]);
}

#[test]
fn candidates_left_from_before_are_adopted_when_the_list_is_read() {
    let (d, cwd) = project();
    let root = d.path();
    let a = add_candidate(root, "checklistItem", "変更の後にテストを走らせる",
        json!({ "role": "実装", "text": "変更の後にテストを走らせる", "when": "afterChange", "check": "machine" }), "model").unwrap();
    let b = add_candidate(root, "overviewAppend", "概要へ追記", json!({ "text": "方針: 小さく直す" }), "model").unwrap();
    assert_eq!((a.as_str(), b.as_str()), ("cand-0001", "cand-0002"));
    assert!(add_candidate(root, "prohibition", "x", json!({}), "model").is_err(), "a model cannot propose a prohibition");
    // nothing waits for the user: reading the list adopts them
    assert_eq!(ok("candidate/list", json!({ "cwd": cwd }))["candidates"].as_array().unwrap().len(), 0);
    assert_eq!(ok("checklist/list", json!({ "cwd": cwd }))["items"][0]["origin"], "candidate:cand-0001");
    assert!(ok("overview/read", json!({ "cwd": cwd }))["text"].as_str().unwrap().contains("小さく直す"));
    assert_eq!(err("candidate/approve", json!({ "cwd": cwd, "id": a })).code, NOT_FOUND);
    // one approved directly still lands where it belongs
    let c = add_candidate(root, "overviewAppend", "追記", json!({ "text": "方針: 確認してから直す" }), "model").unwrap();
    assert_eq!(ok("candidate/approve", json!({ "cwd": cwd, "id": c }))["status"], "registered");
    assert!(ok("overview/read", json!({ "cwd": cwd }))["text"].as_str().unwrap().contains("確認してから直す"));
}

#[test]
fn the_model_proposes_and_only_the_user_registers() {
    let (d, cwd) = project();
    let root = d.path();
    let failure = |title: &str, extra: Value| {
        let mut a = json!({ "kind": "failure", "title": title, "body": "adb shell が失敗した",
            "conditions": "fastboot の最中だった", "attempted": "adb shell", "outcome": "exit 1: device offline",
            "avoid": "fastboot の間は adb shell を使わない", "source": "https://example.com/adb", "checked_at": "2026-11-13" });
        a.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        a
    };
    // a failure without conditions, attempted or outcome is refused with a reason
    let no_cond = propose(root, &failure("x", json!({ "conditions": null })), "model:t");
    assert_eq!(no_cond["status"], "denied");
    assert!(no_cond["reason"].as_str().unwrap().contains("conditions"));
    assert_eq!(propose(root, &json!({ "kind": "failure", "title": "t", "body": "b", "conditions": "x" }), "m")["status"], "denied");
    assert_eq!(propose(root, &json!({ "kind": "idea", "title": "t", "body": "b" }), "m")["status"], "denied");
    // registered at once (no approval); the user rejects what is wrong
    let ok = propose(root, &failure("adb shell は fastboot 中に失敗する", json!({})), "model:t");
    assert_eq!((ok["status"].as_str(), ok["requiresApproval"].as_bool(), ok["kind"].as_str()), (Some("registered"), Some(false), Some("failureCase")));
    assert!(ok["id"].as_str().unwrap().starts_with("failure_case-"), "the result says which record it made");
    assert_eq!(ok["target"]["sub"], "failures");
    let store = guard_store(&cwd);
    // "unknown" conditions are accepted and said so
    let unknown = propose(root, &failure("別の失敗", json!({ "conditions": "unknown", "attempted": "cargo build", "outcome": "OOM" })), "m");
    assert_eq!(unknown["status"], "registered");
    // the screen reads registered failures: list, search, read; "unknown" conditions say so
    let listed = ok_call("failure/list", json!({ "cwd": cwd, "query": "fastboot" }));
    assert_eq!(listed["failures"].as_array().unwrap().len(), 1);
    let first = &listed["failures"][0];
    assert_eq!(first["conditions"], "fastboot の最中だった");
    assert_eq!((first["attempted"].as_str(), first["outcome"].as_str()), (Some("adb shell"), Some("exit 1: device offline")));
    assert_eq!((first["source"].as_str(), first["checkedAt"].as_str()), (Some("https://example.com/adb"), Some("2026-11-13")));
    assert_eq!(first["avoid"], "fastboot の間は adb shell を使わない");
    assert_eq!(ok_call("failure/list", json!({ "cwd": cwd, "query": "nothing-like-it" }))["failures"].as_array().unwrap().len(), 0);
    assert_eq!(ok_call("failure/read", json!({ "cwd": cwd, "id": ok["id"] }))["failure"]["title"], "adb shell は fastboot 中に失敗する");
    assert_eq!(err("failure/read", json!({ "cwd": cwd, "id": "failure_case-9999" })).code, NOT_FOUND);
    let rec = store.all(Some(Scope::Project)).into_iter().find(|r| r.title.starts_with("adb shell")).unwrap();
    assert_eq!(rec.status, Status::Verified);
    assert!(!is_candidate(&rec));
    let unk_listed = ok_call("failure/list", json!({ "cwd": cwd, "query": "別の失敗" }));
    assert_eq!(unk_listed["failures"][0]["conditions"], "unknown");
    // a similar record that exists is updated (the old one is superseded)
    let upd = propose(root, &failure("adb shell は fastboot 中に失敗する", json!({ "outcome": "exit 1 (again)" })), "m");
    assert_eq!((upd["op"].as_str(), upd["status"].as_str()), (Some("update"), Some("registered")));
    assert_eq!(upd["updates"], rec.id.as_str());
    let all: Vec<_> = store.all(Some(Scope::Project)).into_iter().filter(|r| r.title.starts_with("adb shell")).collect();
    assert!(all.iter().any(|r| r.status == Status::Superseded) && all.iter().any(|r| r.status == Status::Verified && r.outcome.as_deref() == Some("exit 1 (again)")));
    // the user rejects one (reason optional): it is retracted and not proposed again
    let now_id = upd["id"].as_str().unwrap().to_string();
    let rej = ok_call("reject", json!({ "cwd": cwd, "kind": "failureCase", "id": now_id, "reason": "fastboot は関係なかった" }));
    assert_eq!(rej["status"], "retracted");
    assert_eq!(ok_call("failure/list", json!({ "cwd": cwd, "query": "fastboot" }))["failures"].as_array().unwrap().len(), 0);
    let again = propose(root, &failure("adb shell は fastboot 中に失敗する", json!({})), "m");
    assert_eq!(again["status"], "denied");
    assert!(again["reason"].as_str().unwrap().contains("fastboot は関係なかった"));

    // a new page and a fact are registered at once, marked as drafts (Q-12)
    let page = propose(root, &json!({ "kind": "page", "title": "ビルドの手順", "body": "#手順\nlock を取って cargo を 1 本ずつ走らせる" }), "m");
    assert_eq!(page["kind"], "page");
    assert_eq!(page["status"], "registered");
    assert_eq!(page["draft"], true);
    assert_eq!(page["requiresApproval"], false);
    let fact = propose(root, &json!({ "kind": "fact", "title": "ビルドは Windows だけ", "body": "他の OS では確認していない", "source": "AGENTS.md" }), "m");
    assert_eq!(fact["status"], "registered");
    let pages = ok_call("page/list", json!({ "cwd": cwd }));
    assert_eq!(pages["pages"].as_array().unwrap().len(), 2);
    let r = ok_call("page/read", json!({ "cwd": cwd, "title": "ビルドは Windows だけ" }));
    assert_eq!(r["confirmed"], false, "a draft until the user confirms it");
    assert!(r["tags"].as_array().unwrap().iter().any(|t| t == "fact"));
    assert!(r["body"].as_str().unwrap().contains("Source: AGENTS.md"));
    // a change to an existing page replaces it (the history keeps the old one); rejecting it
    // brings the old version back
    let changed = propose(root, &json!({ "kind": "page", "title": "ビルドの手順", "body": "cargo を並列で何本も走らせる" }), "m");
    assert_eq!((changed["op"].as_str(), changed["status"].as_str()), (Some("update"), Some("registered")));
    assert_eq!(ok_call("page/read", json!({ "cwd": cwd, "title": "ビルドの手順" }))["revs"].as_array().unwrap().len(), 2);
    assert_eq!(ok_call("reject", json!({ "cwd": cwd, "kind": "page", "id": "ビルドの手順" }))["status"], "restored");
    assert!(ok_call("page/read", json!({ "cwd": cwd, "title": "ビルドの手順" }))["body"].as_str().unwrap().contains("1 本ずつ"));
    // a page the model made goes to the trash, without a reason
    assert_eq!(ok_call("reject", json!({ "cwd": cwd, "kind": "page", "id": "ビルドは Windows だけ" }))["status"], "removed");
    assert_eq!(ok_call("page/list", json!({ "cwd": cwd }))["pages"].as_array().unwrap().len(), 1);
}

fn ok_call(method: &str, params: Value) -> Value {
    ok(method, params)
}

#[test]
fn page_graph_gives_the_mind_map_in_one_call_without_bodies() {
    let (_d, cwd) = project();
    let store = codex_inocode_evidence::default_store();
    let src = store.import_text("法令", "law", "他人の財物を窃取した者は処罰される。", None).unwrap();
    ok("page/write", json!({ "cwd": cwd, "title": "A", "body": format!("[B] と [無い]
> quote:{}: 財物を窃取した者", src.id), "by": "user", "confirm": true }));
    ok("page/write", json!({ "cwd": cwd, "title": "B", "body": "本文は返らない", "by": "model" }));
    let g = ok("page/graph", json!({ "cwd": cwd }));
    let pages = g["pages"].as_array().unwrap();
    assert_eq!(pages.len(), 2);
    let a = &pages[0];
    assert_eq!(a["title"], "A");
    assert_eq!(a["confirmed"], true);
    assert!(a.get("body").is_none() && a["rev"].is_string());
    assert_eq!(a["links"], json!([{ "title": "B", "exists": true }, { "title": "無い", "exists": false }]));
    assert_eq!(a["quotes"][0]["matches"], true);
    assert_eq!(a["quotes"][0]["sourceTitle"], "法令");
    assert_eq!(pages[1]["confirmed"], false);
    let one = ok("page/graph", json!({ "cwd": cwd, "limit": 1 }));
    assert_eq!(one["pages"].as_array().unwrap().len(), 1);
}

#[test]
fn verify_reports_novel_terms_and_missing_quotes_without_blocking() {
    let (_d, cwd) = project();
    let store = codex_inocode_evidence::default_store();
    let src = store.import_text("訴状", "text", "被告人は他人の財物を奪い、強盗罪に当たる行為をした。", None).unwrap();
    let r = ok("verify", json!({ "cwd": cwd, "text": "本件は窃盗罪に当たる。「存在しない文」[出典]", "sourceRefs": [src.id] }));
    let f = r["findings"].as_array().unwrap();
    assert!(f.iter().any(|x| x["kind"] == "novel_term" && x["text"] == "窃盗罪"), "{f:?}");
    assert!(f.iter().any(|x| x["kind"] == "quote_missing" && x["text"] == "存在しない文"), "{f:?}");
    let novel = f.iter().find(|x| x["text"] == "窃盗罪").unwrap();
    assert_eq!(novel["position"]["end"].as_u64().unwrap() - novel["position"]["start"].as_u64().unwrap(), 3);
    assert_eq!(err("verify", json!({ "cwd": cwd, "text": "x", "sourceRefs": ["src-9999"] })).code, INVALID);
}

#[test]
fn record_evidence_writes_the_project_page_and_quote_that_the_screen_reads() {
    let (d, cwd) = project();
    let root = d.path();
    let page_text = "Widget X costs 1,200 yen (tax included) as of 2026-11-11.";
    let rec = |claim: &str, quote: &str| {
        json!({ "claim": claim, "quote": quote, "source_url": "https://example.com/price",
            "source_title": "Price page", "source_text": page_text, "page": "Widget の価格" })
    };
    let first = record_evidence(root, &rec("Widget X は 1,200 円", "Widget X costs 1,200 yen"), "model:t");
    assert_eq!(first["status"], "registered", "{first}");
    assert_eq!(first["draft"], true);
    assert_eq!(first["requiresApproval"], false);
    assert_eq!(first["op"], "create");
    assert_eq!(first["target"], json!({ "tab": "evidence", "sub": "pages", "ref": "Widget の価格" }));
    assert!(first["retrievedAt"].is_string());
    // the source belongs to this project, not to the old global database
    assert!(root.join(".inocode").join("kb").join("evidence").join("sources").exists());
    let src = first["sourceRef"].as_str().unwrap().to_string();
    // the same page again is the same source, and the fact goes onto the same page
    let second = record_evidence(root, &rec("税込み", "tax included"), "model:t");
    assert_eq!(second["op"], "update");
    assert_eq!(second["sourceRef"], src.as_str());
    // the page, its quotes and the mind map read it
    let r = ok("page/read", json!({ "cwd": cwd, "title": "Widget の価格" }));
    assert_eq!(r["confirmed"], false);
    let quotes = r["quotes"].as_array().unwrap();
    assert_eq!(quotes.len(), 2);
    assert!(quotes.iter().all(|q| q["matches"] == true && q["sourceUrl"] == "https://example.com/price"));
    let g = ok("page/graph", json!({ "cwd": cwd }));
    assert_eq!(g["pages"][0]["quotes"][0]["sourceTitle"], "Price page");
    // refusals carry the reason
    assert_eq!(record_evidence(root, &rec("x", "Widget X costs 9 yen"), "m")["status"], "denied");
    assert_eq!(record_evidence(root, &json!({ "claim": "x", "quote": "a\nb" }), "m")["status"], "denied");
    // without a page name, the page is named after the source
    let other = record_evidence(root, &json!({ "claim": "c", "quote": "q", "source_title": "Memo" }), "m");
    assert_eq!(other["page"], "Memo");

    // search finds the page with its quotes and sources
    let found = search_evidence(root, Some("widget 円"), 20);
    assert_eq!(found["count"], 1);
    assert_eq!(found["results"][0]["page"], "Widget の価格");
    assert_eq!(found["results"][0]["draft"], true);
    assert_eq!(found["results"][0]["quotes"][0]["sourceUrl"], "https://example.com/price");
    assert!(found["results"][0]["quotes"][0]["retrievedAt"].is_string());
    assert_eq!(search_evidence(root, Some("gadget"), 20)["count"], 0);

    // verify is the same check as inocode/kb/verify, over this project's sources by default
    let a = verify_document(root, "Widget X costs 1,200 yen.", &[src.clone()]).unwrap();
    let b = ok("verify", json!({ "cwd": cwd, "text": "Widget X costs 1,200 yen.", "sourceRefs": [src] }));
    assert_eq!(a, b);
    let all = verify_document(root, "「存在しない文」", &[]).unwrap();
    assert!(all["findings"].as_array().unwrap().iter().any(|f| f["kind"] == "quote_missing"));
    assert_eq!(verify_document(root, "x", &["src-9999".to_string()]).unwrap_err().code, INVALID);

    // the user rejects one quote: only its two lines go, the other quote and the page stay, and
    // the same claim is not recorded again (the page can still take other claims)
    let rej = |r: &Value| json!({ "cwd": cwd, "kind": "evidence", "id": r["id"], "claim": r["claim"], "sourceRef": r["sourceRef"], "reason": "古い価格" });
    assert_eq!(ok("reject", rej(&second))["status"], "quoteRemoved");
    let left = ok("page/read", json!({ "cwd": cwd, "title": "Widget の価格" }));
    assert_eq!(left["quotes"].as_array().unwrap().len(), 1);
    assert_eq!(left["confirmed"], false, "a rejection does not confirm the page");
    let again = record_evidence(root, &rec("税込み", "tax included"), "m");
    assert!(again["reason"].as_str().unwrap().contains("古い価格"), "{again}");
    // the last quote of a page the model made: the page goes
    assert_eq!(ok("reject", rej(&first))["status"], "removed");
    assert_eq!(err("page/read", json!({ "cwd": cwd, "title": "Widget の価格" })).code, NOT_FOUND);
}

#[test]
fn a_folder_above_the_project_is_another_project() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home").join(".inocode");
    // Documents was opened once (it has its own knowledge); temp3 below it is not part of it.
    let docs = d.path().join("Documents");
    let temp3 = docs.join("temp3");
    std::fs::create_dir_all(docs.join(".inocode").join("kb").join("inbox")).unwrap();
    std::fs::create_dir_all(&temp3).unwrap();
    // (not in git; the test folder itself may be inside a repository, so that is given)
    assert_eq!(crate::kb_root_from(&temp3, &home, None), temp3);
    // In a repository: its top, never the workspace folder above it (whose rules still guard).
    let ws = d.path().join("ws");
    let repo = ws.join("repo");
    std::fs::create_dir_all(ws.join(".inocode")).unwrap();
    std::fs::write(ws.join(".inocode").join("project.json"), "{}").unwrap();
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    assert_eq!(crate::kb_root_from(&repo.join("src"), &home, Some(&repo)), repo);
    assert_eq!(crate::project_root_for(&repo.join("src"), &home), ws, "the guard's rules reach down as before");
    // A sub-project inside the repository that has its own knowledge is used.
    std::fs::create_dir_all(repo.join("src").join(".inocode").join("kb")).unwrap();
    assert_eq!(crate::kb_root_from(&repo.join("src").join("a"), &home, Some(&repo)), repo.join("src"));
    assert_eq!(crate::kb_root_for(&repo.join("src").join("a"), &home), repo.join("src"), "the repository is found");
}
