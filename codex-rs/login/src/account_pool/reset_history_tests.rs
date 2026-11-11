#![allow(clippy::expect_used)]

use super::HistoryEntry;
use super::HistoryKind;
use super::HistorySource;
use super::append;
use super::observe;
use super::read;
use crate::account_pool::AccountUsage;
use crate::account_pool::ResetCredit;
use crate::account_pool::ResetGrant;
use crate::account_pool::ResetStatus;
use crate::account_pool::UsageWindow;
use pretty_assertions::assert_eq;

const NOW: i64 = 1_794_000_000;

fn grant(id: &str, left: i64, ends_at: Option<i64>) -> ResetGrant {
    ResetGrant {
        id: id.to_string(),
        label: format!("label {id}"),
        resets_left: left,
        resets_total: 1,
        ends_at,
        paused: false,
        usable_now: left > 0,
        clears: vec!["five_hour".to_string()],
    }
}

fn usage(grants: Vec<ResetGrant>, updated_at: i64) -> AccountUsage {
    AccountUsage {
        updated_at: Some(updated_at),
        reset_credit: Some(ResetCredit {
            eligible: true,
            at_limit: false,
            resets_left: grants.iter().map(|g| g.resets_left).sum(),
            resets_total: i64::try_from(grants.len()).expect("len"),
            usable_now: true,
            ends_at: None,
            cooldown_until: None,
            grants,
        }),
        reset_status: Some(ResetStatus::ok()),
        ..AccountUsage::default()
    }
}

fn windowed(five: Option<(f64, i64)>, week: Option<(f64, i64)>, at: i64) -> AccountUsage {
    let window = |value: Option<(f64, i64)>, minutes: i64| {
        value.map(|(used_percent, resets_at)| UsageWindow {
            used_percent,
            window_minutes: Some(minutes),
            resets_at: Some(resets_at),
        })
    };
    AccountUsage {
        five_hour: window(five, 300),
        weekly: window(week, 10_080),
        updated_at: Some(at),
        ..AccountUsage::default()
    }
}

fn kinds(path: &std::path::Path) -> Vec<(HistoryKind, Option<String>)> {
    read(path)
        .into_iter()
        .map(|e| (e.kind, e.grant_id))
        .collect()
}

#[test]
fn a_grant_is_recorded_once_when_first_seen_however_often_it_is_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("reset-history.jsonl");
    let first = usage(vec![grant("a", 1, None)], NOW);
    observe(&path, None, &first, NOW);
    observe(
        &path,
        Some(&first),
        &usage(vec![grant("a", 1, None)], NOW + 300),
        NOW + 300,
    );
    observe(
        &path,
        Some(&first),
        &usage(vec![grant("a", 1, None), grant("b", 1, None)], NOW + 600),
        NOW + 600,
    );
    assert_eq!(
        kinds(&path),
        vec![
            (HistoryKind::Granted, Some("a".to_string())),
            (HistoryKind::Granted, Some("b".to_string())),
        ]
    );
    let entries = read(&path);
    assert!(entries[0].detail.contains("初めての取得"));
    assert!(!entries[1].detail.contains("初めての取得"));
    assert_eq!(entries[0].source, HistorySource::Observed);
}

#[test]
fn a_fall_in_a_grant_is_used_elsewhere_unless_this_app_recorded_the_spend() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("reset-history.jsonl");
    let before = usage(vec![grant("a", 1, None), grant("b", 1, None)], NOW);
    // `b` was spent from this app: its own record is in the history.
    append(
        &path,
        &[HistoryEntry {
            ts: NOW + 100,
            kind: HistoryKind::Used,
            grant_id: Some("b".to_string()),
            label: None,
            detail: "used from this app".to_string(),
            source: HistorySource::Journal,
            key: "op:1:settled".to_string(),
        }],
    );
    let after = usage(vec![grant("a", 0, None), grant("b", 0, None)], NOW + 300);
    observe(&path, Some(&before), &after, NOW + 300);
    assert_eq!(
        kinds(&path),
        vec![
            (HistoryKind::Used, Some("b".to_string())),
            (HistoryKind::UsedElsewhere, Some("a".to_string())),
        ]
    );
    // Seeing the same state again records nothing more.
    observe(&path, Some(&before), &after, NOW + 600);
    assert_eq!(read(&path).len(), 2);
}

#[test]
fn a_grant_that_ends_is_expired_and_one_that_vanishes_early_is_used_elsewhere() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("reset-history.jsonl");
    let before = usage(
        vec![
            grant("a", 1, Some(NOW + 100)),
            grant("b", 1, Some(NOW + 9_999)),
        ],
        NOW,
    );
    let after = usage(vec![], NOW + 200);
    observe(&path, Some(&before), &after, NOW + 200);
    assert_eq!(
        kinds(&path),
        vec![
            (HistoryKind::Expired, Some("a".to_string())),
            (HistoryKind::UsedElsewhere, Some("b".to_string())),
        ]
    );
    assert_eq!(read(&path)[0].ts, NOW + 100, "an expiry is dated by its deadline");
}

#[test]
fn what_the_server_lists_as_already_used_or_ended_is_marked_as_its_own_word() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("reset-history.jsonl");
    let first = usage(
        vec![
            grant("old", 0, Some(NOW - 50)),
            grant("spent", 0, Some(NOW + 500)),
        ],
        NOW,
    );
    observe(&path, None, &first, NOW);
    let entries = read(&path);
    assert_eq!(
        entries
            .iter()
            .map(|e| (e.kind, e.source))
            .collect::<Vec<_>>(),
        vec![
            (HistoryKind::Expired, HistorySource::Endpoint),
            (HistoryKind::UsedElsewhere, HistorySource::Endpoint),
        ]
    );
}

#[test]
fn a_failed_read_records_nothing_about_grants_and_one_error_per_change_of_reason() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("reset-history.jsonl");
    let good = usage(vec![grant("a", 1, None)], NOW);
    let mut failed = good.clone();
    failed.reset_status = Some(ResetStatus {
        stale: true,
        ..ResetStatus::error("request_failed")
    });
    observe(&path, Some(&good), &failed, NOW + 300);
    observe(&path, Some(&failed), &failed, NOW + 600);
    let mut other = failed.clone();
    other.reset_status = Some(ResetStatus {
        stale: true,
        ..ResetStatus::error("auth_refused")
    });
    observe(&path, Some(&failed), &other, NOW + 900);
    assert_eq!(
        kinds(&path),
        vec![(HistoryKind::Error, None), (HistoryKind::Error, None)]
    );
}

#[test]
fn a_window_that_fell_is_a_reset_only_with_a_passed_deadline_or_a_real_drop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("reset-history.jsonl");
    // Deadline passed and usage fell: it turned over on schedule.
    let before = windowed(Some((88.0, NOW - 10)), None, NOW - 600);
    let after = windowed(Some((2.0, NOW + 18_000)), None, NOW);
    observe(&path, Some(&before), &after, NOW);
    // The same state again: recorded once.
    observe(&path, Some(&before), &after, NOW + 30);
    assert_eq!(kinds(&path), vec![(HistoryKind::QuotaReset, None)]);
    assert!(read(&path)[0].detail.contains("88% → 2%"));

    // A rolling window decays and its deadline creeps with the clock: not a reset.
    let rolling_before = windowed(Some((88.0, NOW + 20_000)), None, NOW);
    let rolling_after = windowed(Some((61.0, NOW + 20_000 + 3_600)), None, NOW + 3_600);
    observe(&path, Some(&rolling_before), &rolling_after, NOW + 3_600);
    assert_eq!(read(&path).len(), 1);

    // A drop with the deadline standing still is a reset out of band.
    let out_before = windowed(None, Some((70.0, NOW + 400_000)), NOW);
    let out_after = windowed(None, Some((1.0, NOW + 400_000)), NOW + 300);
    observe(&path, Some(&out_before), &out_after, NOW + 300);
    assert_eq!(read(&path).len(), 2);
    assert!(read(&path)[1].detail.contains("期限の前"));

    // Noise (under five points) and a first-ever read are not resets.
    let small = windowed(None, Some((68.0, NOW + 400_000)), NOW + 600);
    observe(&path, Some(&out_before), &small, NOW + 600);
    observe(&path, None, &small, NOW + 600);
    assert_eq!(read(&path).len(), 2);
}

#[test]
fn the_file_is_only_ever_appended_to_and_unreadable_lines_are_left_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("reset-history.jsonl");
    std::fs::write(&path, "not json\n").expect("seed");
    let first = usage(vec![grant("a", 1, None)], NOW);
    observe(&path, None, &first, NOW);
    let text = std::fs::read_to_string(&path).expect("text");
    assert!(text.starts_with("not json\n"), "the earlier line is untouched");
    assert_eq!(read(&path).len(), 1);
    assert!(!text.contains("access"), "no token is ever written");
}
