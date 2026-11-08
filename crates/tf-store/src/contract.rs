//! Behaviour every `Store` must have. Each case takes a fresh, empty store.

use crate::{Claim, Store, StoreError};
use std::sync::Arc;
use tf_core::*;

type S = Arc<dyn Store>;

async fn user_and_org(s: &S, tag: &str) -> (User, Org) {
    let u = s.create_user(&format!("{tag}@example.com"), "hash", tag).await.unwrap();
    let o = s.create_org(&format!("org-{tag}"), &u.id).await.unwrap();
    (u, o)
}

async fn ws_with_cfg(s: &S, org: &str, name: &str) -> (Workspace, ConfigVersion) {
    let ws = s.create_workspace(org, name).await.unwrap();
    let cv = s.create_config_version(&ws.id, "cfg", "test").await.unwrap();
    (ws, cv)
}

async fn paused(s: &S, run: &str) {
    s.apply_event(run, Event::PlanOk { has_changes: true, auto_apply: false, plan_only: false }).await.unwrap();
}

pub async fn workspace_names_are_validated_and_unique_per_org(s: S) {
    let (_, a) = user_and_org(&s, "a").await;
    let (_, b) = user_and_org(&s, "b").await;
    assert!(matches!(s.create_workspace(&a.id, "bad name").await, Err(StoreError::Invalid(_))));
    s.create_workspace(&a.id, "ok").await.unwrap();
    assert!(matches!(s.create_workspace(&a.id, "ok").await, Err(StoreError::Conflict(_))));
    s.create_workspace(&b.id, "ok").await.unwrap();
    assert!(matches!(s.create_workspace("no-such-org", "x").await, Err(StoreError::NotFound(_))));
}

pub async fn workspaces_are_listed_per_org(s: S) {
    let (_, a) = user_and_org(&s, "a").await;
    let (_, b) = user_and_org(&s, "b").await;
    s.create_workspace(&a.id, "zeta").await.unwrap();
    s.create_workspace(&a.id, "alpha").await.unwrap();
    s.create_workspace(&b.id, "other").await.unwrap();
    let names: Vec<_> = s.list_workspaces(&a.id).await.unwrap().into_iter().map(|w| w.name).collect();
    assert_eq!(names, vec!["alpha", "zeta"], "sorted, and only this org's");
    assert_eq!(s.list_workspaces(&b.id).await.unwrap().len(), 1);
}

pub async fn one_executing_run_per_workspace_and_fifo(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let (ws, cv) = ws_with_cfg(&s, &o.id, "prod").await;
    let r1 = s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "1", false, "t").await.unwrap();
    let r2 = s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "2", false, "t").await.unwrap();

    let Some(Claim::Plan(c)) = s.claim_next().await.unwrap() else { panic!("expected a claim") };
    assert_eq!(c.id, r1.id, "oldest run goes first");
    assert_eq!(c.status, RunStatus::Planning);
    assert!(s.claim_next().await.unwrap().is_none(), "second run waits while the first holds the lock");

    paused(&s, &r1.id).await;
    assert!(s.claim_next().await.unwrap().is_none(), "a paused run still blocks the queue");

    s.apply_event(&r1.id, Event::Discard).await.unwrap();
    assert!(s.get_workspace(&ws.id).await.unwrap().lock.is_none(), "discard releases the lock");
    let Some(Claim::Plan(c2)) = s.claim_next().await.unwrap() else { panic!("expected second run") };
    assert_eq!(c2.id, r2.id);
}

pub async fn manual_lock_blocks_runs_until_unlocked(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let (ws, cv) = ws_with_cfg(&s, &o.id, "prod").await;
    s.lock_workspace(&ws.id, "user", "fixing state", None, None).await.unwrap();
    assert!(matches!(s.lock_workspace(&ws.id, "user", "again", None, None).await, Err(StoreError::Conflict(_))));
    s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    assert!(s.claim_next().await.unwrap().is_none());
    s.unlock_workspace(&ws.id, false).await.unwrap();
    assert!(s.claim_next().await.unwrap().is_some());
    assert!(
        matches!(s.unlock_workspace(&ws.id, false).await, Err(StoreError::Conflict(_))),
        "a run-held lock needs force"
    );
    s.unlock_workspace(&ws.id, true).await.unwrap();
}

pub async fn speculative_plans_do_not_lock(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let (ws, cv) = ws_with_cfg(&s, &o.id, "prod").await;
    let p = s.create_run(&ws.id, &cv.id, Operation::PlanOnly, "", false, "t").await.unwrap();
    let Some(Claim::Plan(c)) = s.claim_next().await.unwrap() else { panic!() };
    assert_eq!(c.id, p.id);
    assert!(s.get_workspace(&ws.id).await.unwrap().lock.is_none());
    s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    assert!(s.claim_next().await.unwrap().is_some(), "a real run can start next to a speculative plan");
}

pub async fn applies_are_claimed_before_new_plans(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let (ws, cv) = ws_with_cfg(&s, &o.id, "one").await;
    let r1 = s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    s.claim_next().await.unwrap();
    paused(&s, &r1.id).await;
    s.apply_event(&r1.id, Event::Confirm).await.unwrap();
    let (ws2, cv2) = ws_with_cfg(&s, &o.id, "two").await;
    s.create_run(&ws2.id, &cv2.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    let Some(Claim::Apply(a)) = s.claim_next().await.unwrap() else { panic!("apply first") };
    assert_eq!(a.id, r1.id);
    assert_eq!(a.status, RunStatus::Applying);
}

pub async fn illegal_transitions_are_conflicts(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let (ws, cv) = ws_with_cfg(&s, &o.id, "prod").await;
    let r = s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    assert!(matches!(s.apply_event(&r.id, Event::Confirm).await, Err(StoreError::Conflict(_))));
    assert!(matches!(s.apply_event("missing", Event::Confirm).await, Err(StoreError::NotFound(_))));
}

pub async fn state_serials_increase_and_current_is_latest(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let (ws, cv) = ws_with_cfg(&s, &o.id, "prod").await;
    let run = s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    assert!(s.current_state(&ws.id).await.unwrap().is_none());
    let a = s.finalize_state(&ws.id, &run.id, "{}", 0).await.unwrap();
    let b = s.finalize_state(&ws.id, &run.id, "{\"resources\":{}}", 2).await.unwrap();
    assert_eq!((a.serial, b.serial), (1, 2));
    assert_eq!(s.current_state(&ws.id).await.unwrap().unwrap().serial, 2);
    assert_eq!(s.get_workspace(&ws.id).await.unwrap().resource_count, 2);
    let list = s.list_states(&ws.id).await.unwrap();
    assert_eq!(list[0].serial, 2, "newest first");
    assert_eq!(list.len(), 2);
}

pub async fn concurrent_claims_never_double_start_a_workspace(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let (ws, cv) = ws_with_cfg(&s, &o.id, "prod").await;
    for i in 0..20 {
        s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, &i.to_string(), false, "t").await.unwrap();
    }
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move { s.claim_next().await.unwrap().is_some() }));
    }
    let mut claimed = 0;
    for t in tasks {
        if t.await.unwrap() {
            claimed += 1;
        }
    }
    assert_eq!(claimed, 1, "exactly one run may start on one workspace");
}

pub async fn run_must_use_a_config_from_its_own_workspace(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let (ws, _) = ws_with_cfg(&s, &o.id, "prod").await;
    let other = s.create_workspace(&o.id, "other").await.unwrap();
    let foreign = s.create_config_version(&other.id, "x", "t").await.unwrap();
    assert!(matches!(
        s.create_run(&ws.id, &foreign.id, Operation::PlanAndApply, "", false, "t").await,
        Err(StoreError::NotFound(_))
    ));
}

pub async fn a_pending_config_runs_only_after_its_upload(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let ws = s.create_workspace(&o.id, "prod").await.unwrap();
    let cv = s.create_pending_config(&ws.id, "api-upload").await.unwrap();
    assert_eq!((cv.status.as_str(), cv.text.as_str()), ("pending", ""));
    assert!(matches!(s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "", false, "t").await, Err(StoreError::Invalid(_))), "no content yet");
    let up = s.upload_config(&cv.id, "resource \"a\" \"b\" {}").await.unwrap();
    assert_eq!((up.status.as_str(), up.text.as_str()), ("uploaded", "resource \"a\" \"b\" {}"));
    assert!(matches!(s.upload_config(&cv.id, "second").await, Err(StoreError::Conflict(_))), "an upload cannot be replaced");
    assert_eq!(s.get_config_version(&cv.id).await.unwrap().text, "resource \"a\" \"b\" {}");
    assert!(matches!(s.upload_config("missing", "x").await, Err(StoreError::NotFound(_))));
    assert!(matches!(s.create_pending_config("missing", "t").await, Err(StoreError::NotFound(_))));
    s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
}

pub async fn a_lock_with_a_lease_lapses_and_frees_the_workspace(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let (ws, cv) = ws_with_cfg(&s, &o.id, "prod").await;
    let locked = s.lock_workspace(&ws.id, "cli:abc", "apply by ana", None, Some(2)).await.unwrap();
    let until = locked.lock.as_ref().unwrap().expires_at.expect("a lease sets an expiry");
    assert!(until >= locked.lock.as_ref().unwrap().since);
    let run = s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    assert!(s.claim_next().await.unwrap().is_none(), "while the lease runs the lock holds the run back");
    assert!(s.get_workspace(&ws.id).await.unwrap().lock.is_some());

    tokio::time::sleep(std::time::Duration::from_millis(3200)).await;
    // claim first: nothing has read the workspace since the lease ran out, so claiming must notice by itself
    match s.claim_next().await.unwrap() {
        Some(Claim::Plan(r)) => assert_eq!(r.id, run.id, "the queued run starts once the lock lapsed"),
        other => panic!("the run should have started, got {other:?}"),
    }
    // reads see a lapsed lock as no lock
    let other = s.create_workspace(&o.id, "other").await.unwrap();
    s.lock_workspace(&other.id, "cli:z", "x", None, Some(2)).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(3200)).await;
    assert!(s.list_workspaces(&o.id).await.unwrap().iter().find(|w| w.id == other.id).unwrap().lock.is_none(), "listing");
    assert!(s.get_workspace(&other.id).await.unwrap().lock.is_none(), "a lapsed lock is no lock");
    // the run's own lock never lapses
    let held = s.get_workspace(&ws.id).await.unwrap().lock.unwrap();
    assert!(held.owner.starts_with("run:") && held.expires_at.is_none());
}

pub async fn a_lapsed_lock_can_be_taken_over_and_a_live_one_cannot(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let ws = s.create_workspace(&o.id, "prod").await.unwrap();
    s.lock_workspace(&ws.id, "cli:one", "x", None, Some(2)).await.unwrap();
    assert!(matches!(s.lock_workspace(&ws.id, "cli:two", "y", None, Some(60)).await, Err(StoreError::Conflict(_))), "a live lock is not taken over");
    tokio::time::sleep(std::time::Duration::from_millis(3200)).await;
    let l = s.lock_workspace(&ws.id, "cli:two", "y", None, Some(60)).await.unwrap().lock.unwrap();
    assert_eq!(l.owner, "cli:two");
    // the old holder's unlock no longer works, the new holder's does
    assert!(matches!(s.unlock_workspace_owned(&ws.id, "cli:one").await, Err(StoreError::Conflict(_))));
    s.unlock_workspace_owned(&ws.id, "cli:two").await.unwrap();
    // a lock without a lease never lapses
    s.lock_workspace(&ws.id, "user", "by hand", None, None).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert!(s.get_workspace(&ws.id).await.unwrap().lock.is_some());
}

pub async fn a_heartbeat_renews_only_its_own_leased_lock(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let (ws, cv) = ws_with_cfg(&s, &o.id, "prod").await;
    s.lock_workspace(&ws.id, "user", "maintenance", None, Some(2)).await.unwrap();
    let before = s.get_workspace(&ws.id).await.unwrap().lock.unwrap().expires_at.unwrap();
    let after = s.heartbeat_lock(&ws.id, "user", 30).await.unwrap().lock.unwrap().expires_at.unwrap();
    assert!(after > before + 20, "renewed to about 30 s from now: {before} -> {after}");
    tokio::time::sleep(std::time::Duration::from_millis(2300)).await;
    assert!(s.get_workspace(&ws.id).await.unwrap().lock.is_some(), "the renewed lock outlived its first lease");

    assert!(matches!(s.heartbeat_lock(&ws.id, "someone-else", 30).await, Err(StoreError::Conflict(_))), "not the holder");
    s.unlock_workspace(&ws.id, false).await.unwrap();
    assert!(matches!(s.heartbeat_lock(&ws.id, "user", 30).await, Err(StoreError::Conflict(_))), "not locked");
    s.lock_workspace(&ws.id, "user", "no lease", None, None).await.unwrap();
    assert!(matches!(s.heartbeat_lock(&ws.id, "user", 30).await, Err(StoreError::Conflict(_))), "a lock without a lease has nothing to renew");
    s.unlock_workspace(&ws.id, false).await.unwrap();
    // a run's lock is not renewable either
    s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    let Some(Claim::Plan(run)) = s.claim_next().await.unwrap() else { panic!("run should start") };
    assert!(matches!(s.heartbeat_lock(&ws.id, &format!("run:{}", run.id), 30).await, Err(StoreError::Conflict(_))));
    assert!(matches!(s.heartbeat_lock("missing", "user", 30).await, Err(StoreError::NotFound(_))));
}

pub async fn workspace_settings_change_and_are_validated(s: S) {
    let (_, a) = user_and_org(&s, "a").await;
    let (_, b) = user_and_org(&s, "b").await;
    let ws = s.create_workspace(&a.id, "prod").await.unwrap();
    assert!(!ws.auto_apply && ws.description.is_empty(), "new workspaces start with the safe defaults");
    s.create_workspace(&a.id, "staging").await.unwrap();
    s.create_workspace(&b.id, "prod").await.unwrap();

    let up = s.update_workspace(&ws.id, WorkspacePatch { description: Some("production".into()), auto_apply: Some(true), ..Default::default() }).await.unwrap();
    assert_eq!((up.name.as_str(), up.description.as_str(), up.auto_apply), ("prod", "production", true));
    let renamed = s.update_workspace(&ws.id, WorkspacePatch { name: Some("prod-eu".into()), ..Default::default() }).await.unwrap();
    assert_eq!((renamed.name.as_str(), renamed.description.as_str(), renamed.auto_apply), ("prod-eu", "production", true), "fields left out stay as they were");
    assert_eq!(s.get_workspace(&ws.id).await.unwrap().name, "prod-eu");
    assert!(matches!(s.update_workspace(&ws.id, WorkspacePatch { name: Some("staging".into()), ..Default::default() }).await, Err(StoreError::Conflict(_))), "taken in this organization");
    s.update_workspace(&ws.id, WorkspacePatch { name: Some("prod-eu".into()), ..Default::default() }).await.unwrap(); // its own name is fine
    for bad in ["", "has space", "a/b", &"x".repeat(91)] {
        assert!(matches!(s.update_workspace(&ws.id, WorkspacePatch { name: Some(bad.into()), ..Default::default() }).await, Err(StoreError::Invalid(_))), "{bad:?}");
    }
    assert!(matches!(s.update_workspace(&ws.id, WorkspacePatch { description: Some("x".repeat(501)), ..Default::default() }).await, Err(StoreError::Invalid(_))));
    assert!(matches!(s.update_workspace("missing", WorkspacePatch::default()).await, Err(StoreError::NotFound(_))));
    // the other organization may use the name freely
    s.update_workspace(&ws.id, WorkspacePatch { name: Some("prod".into()), ..Default::default() }).await.unwrap();
}

pub async fn a_workspace_is_deleted_with_everything_in_it(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let (ws, cv) = ws_with_cfg(&s, &o.id, "prod").await;
    let keep = s.create_workspace(&o.id, "keep").await.unwrap();
    let run = s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    s.apply_event(&run.id, Event::Discard).await.unwrap();
    s.set_variable(&ws.id, NewVariable { key: "K".into(), category: "env".into(), hcl: false, sensitive: false, value: "v".into(), description: String::new() }).await.unwrap();
    s.finalize_state(&ws.id, &run.id, "{}", 0).await.unwrap();

    // refused while something holds it
    s.lock_workspace(&ws.id, "user", "hold", None, None).await.unwrap();
    assert!(matches!(s.delete_workspace(&ws.id).await, Err(StoreError::Conflict(_))), "locked");
    s.unlock_workspace(&ws.id, false).await.unwrap();
    s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    let Some(Claim::Plan(working)) = s.claim_next().await.unwrap() else { panic!("run should start") };
    assert!(matches!(s.delete_workspace(&ws.id).await, Err(StoreError::Conflict(_))), "a run holds the lock");
    s.apply_event(&working.id, Event::Cancel).await.unwrap();

    s.delete_workspace(&ws.id).await.unwrap();
    assert!(matches!(s.get_workspace(&ws.id).await, Err(StoreError::NotFound(_))));
    assert!(matches!(s.get_run(&run.id).await, Err(StoreError::NotFound(_))), "its runs go with it");
    assert!(s.list_states(&ws.id).await.unwrap().is_empty() && s.current_state(&ws.id).await.unwrap().is_none());
    assert!(s.list_variables(&ws.id).await.unwrap().is_empty());
    assert!(matches!(s.delete_workspace(&ws.id).await, Err(StoreError::NotFound(_))), "gone is gone");
    assert_eq!(s.list_workspaces(&o.id).await.unwrap().len(), 1);
    assert_eq!(s.get_workspace(&keep.id).await.unwrap().name, "keep", "other workspaces are untouched");
    // the name can be used again
    s.create_workspace(&o.id, "prod").await.unwrap();
}

pub async fn a_single_state_version_can_be_fetched(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let (ws, cv) = ws_with_cfg(&s, &o.id, "prod").await;
    let run = s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    let v1 = s.finalize_state(&ws.id, &run.id, "{\"a\":1}", 1).await.unwrap();
    let v2 = s.finalize_state(&ws.id, &run.id, "{\"a\":2}", 1).await.unwrap();
    let got = s.get_state_version(&v1.id).await.unwrap();
    assert_eq!((got.serial, got.content.as_str()), (1, "{\"a\":1}"), "decrypted, as stored");
    assert_eq!(s.get_state_version(&v2.id).await.unwrap().serial, 2);
    assert!(matches!(s.get_state_version("missing").await, Err(StoreError::NotFound(_))));
}

pub async fn lint_findings_are_kept_with_the_run(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let (ws, cv) = ws_with_cfg(&s, &o.id, "prod").await;
    let run = s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    assert!(run.lint.is_empty() && s.get_run(&run.id).await.unwrap().lint.is_empty(), "a new run has no findings");
    let f = |rule: &str, line: Option<u32>| LintFinding { rule: rule.into(), severity: "warning".into(), message: format!("{rule} here"), file: Some("main.tf".into()), line };
    s.set_lint(&run.id, vec![f("unused-variable", Some(3)), f("hardcoded-secret", None)]).await.unwrap();
    let got = s.get_run(&run.id).await.unwrap().lint;
    assert_eq!(got, vec![f("unused-variable", Some(3)), f("hardcoded-secret", None)], "kept in order, with file and line");
    assert_eq!(s.list_runs(Some(&ws.id)).await.unwrap()[0].lint.len(), 2, "lists carry them too");
    s.set_lint(&run.id, vec![]).await.unwrap();
    assert!(s.get_run(&run.id).await.unwrap().lint.is_empty(), "replaced, not appended");
    assert!(matches!(s.set_lint("missing", vec![]).await, Err(StoreError::NotFound(_))));
    // findings survive the run's moves through its states
    s.set_lint(&run.id, vec![f("unused-variable", Some(1))]).await.unwrap();
    s.apply_event(&run.id, Event::Discard).await.unwrap();
    assert_eq!(s.get_run(&run.id).await.unwrap().lint.len(), 1);
}

pub async fn run_options_are_kept_and_a_drift_check_must_be_speculative(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let (ws, cv) = ws_with_cfg(&s, &o.id, "prod").await;
    let plain = s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    assert!(!plain.skip_refresh && !plain.refresh_only, "plain runs refresh as usual");
    let skip = s.create_run_with(&ws.id, &cv.id, Operation::PlanAndApply, "", false, "t", RunOptions { skip_refresh: true, refresh_only: false }).await.unwrap();
    assert!(skip.skip_refresh && !skip.refresh_only);
    assert!(s.get_run(&skip.id).await.unwrap().skip_refresh, "kept");
    let drift = s.create_run_with(&ws.id, &cv.id, Operation::PlanOnly, "drift", false, "schedule", RunOptions { skip_refresh: false, refresh_only: true }).await.unwrap();
    assert!(drift.refresh_only && !drift.skip_refresh);
    assert!(s.list_runs(Some(&ws.id)).await.unwrap().iter().any(|r| r.id == drift.id && r.refresh_only), "lists carry it");
    for (what, op, opts) in [
        ("a refresh-only run that applies", Operation::PlanAndApply, RunOptions { skip_refresh: false, refresh_only: true }),
        ("a refresh-only run that skips the refresh", Operation::PlanOnly, RunOptions { skip_refresh: true, refresh_only: true }),
    ] {
        assert!(matches!(s.create_run_with(&ws.id, &cv.id, op, "", false, "t", opts).await, Err(StoreError::Invalid(_))), "{what}");
    }
}

pub async fn drift_checks_fall_due_by_their_interval(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let (ws, cv) = ws_with_cfg(&s, &o.id, "prod").await;
    let run = s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    let due = |s: &S| {
        let s = s.clone();
        let id = ws.id.clone();
        async move { s.due_drift_checks().await.unwrap().iter().any(|w| w.id == id) }
    };
    assert!(!due(&s).await, "no interval, nothing is due");
    assert!(s.get_workspace(&ws.id).await.unwrap().drift_interval.is_none() && s.get_workspace(&ws.id).await.unwrap().drift.is_none());

    let on = s.update_workspace(&ws.id, WorkspacePatch { drift_interval: Some(Some(2)), ..Default::default() }).await.unwrap();
    assert_eq!(on.drift_interval, Some(2));
    assert!(!due(&s).await, "an interval but no state yet: there is nothing to refresh");
    s.finalize_state(&ws.id, &run.id, "{}", 0).await.unwrap();
    assert!(due(&s).await, "state and an interval, never checked: due now");

    // a drift check that is waiting or working holds the next one back
    let check = s.create_run_with(&ws.id, &cv.id, Operation::PlanOnly, "", false, "schedule", RunOptions { skip_refresh: false, refresh_only: true }).await.unwrap();
    assert!(!due(&s).await, "one is already queued");
    s.apply_event(&check.id, Event::Cancel).await.unwrap();
    assert!(due(&s).await, "a canceled check does not count");

    // recording a result starts the interval over
    let status = DriftStatus { checked_at: tf_core::now(), run_id: Some(check.id.clone()), status: "drifted".into(), changes: 3 };
    s.record_drift(&ws.id, status.clone()).await.unwrap();
    assert_eq!(s.get_workspace(&ws.id).await.unwrap().drift, Some(status));
    assert!(!due(&s).await, "just checked");
    tokio::time::sleep(std::time::Duration::from_millis(3200)).await;
    assert!(due(&s).await, "the interval has passed");

    // turning the schedule off, and refusing nonsense
    s.update_workspace(&ws.id, WorkspacePatch { drift_interval: Some(None), ..Default::default() }).await.unwrap();
    assert!(!due(&s).await && s.get_workspace(&ws.id).await.unwrap().drift_interval.is_none());
    assert!(s.get_workspace(&ws.id).await.unwrap().drift.is_some(), "the last result stays");
    for bad in [0u64, 31_536_001] {
        assert!(matches!(s.update_workspace(&ws.id, WorkspacePatch { drift_interval: Some(Some(bad)), ..Default::default() }).await, Err(StoreError::Invalid(_))), "{bad}");
    }
    // a patch that leaves the interval out leaves it alone
    s.update_workspace(&ws.id, WorkspacePatch { drift_interval: Some(Some(60)), ..Default::default() }).await.unwrap();
    s.update_workspace(&ws.id, WorkspacePatch { description: Some("x".into()), ..Default::default() }).await.unwrap();
    assert_eq!(s.get_workspace(&ws.id).await.unwrap().drift_interval, Some(60));
    assert!(matches!(s.record_drift("missing", DriftStatus { checked_at: 1, run_id: None, status: "clean".into(), changes: 0 }).await, Err(StoreError::NotFound(_))));
}

pub async fn the_latest_configuration_is_one_that_has_its_content(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let ws = s.create_workspace(&o.id, "prod").await.unwrap();
    assert!(s.latest_config(&ws.id).await.unwrap().is_none());
    let done = s.create_config_version(&ws.id, "real content", "t").await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let waiting = s.create_pending_config(&ws.id, "api-upload").await.unwrap();
    assert_ne!(done.id, waiting.id);
    assert_eq!(s.latest_config(&ws.id).await.unwrap().unwrap().id, done.id, "a version still waiting for its archive is not the latest configuration");
    s.upload_config(&waiting.id, "newer").await.unwrap();
    assert_eq!(s.latest_config(&ws.id).await.unwrap().unwrap().id, waiting.id);
}

pub async fn logs_keep_order(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let (ws, cv) = ws_with_cfg(&s, &o.id, "prod").await;
    let r = s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    for i in 0..50 {
        s.append_log(&r.id, &format!("line {i}")).await.unwrap();
    }
    let log = s.get_log(&r.id).await.unwrap();
    assert_eq!(log.len(), 50);
    assert_eq!(log[0], "line 0");
    assert_eq!(log[49], "line 49");
    assert!(s.get_log("missing").await.unwrap().is_empty());
    let tail = s.get_log_from(&r.id, 45).await.unwrap();
    assert_eq!(tail, vec!["line 45", "line 46", "line 47", "line 48", "line 49"], "resuming from a position");
    assert_eq!(s.get_log_from(&r.id, 0).await.unwrap().len(), 50);
    assert!(s.get_log_from(&r.id, 50).await.unwrap().is_empty() && s.get_log_from(&r.id, 5000).await.unwrap().is_empty());
}

pub async fn plans_and_state_round_trip_through_sealing(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let (ws, cv) = ws_with_cfg(&s, &o.id, "prod").await;
    let r = s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    let blob = vec![0u8, 1, 2, 255, 254, 10, 13];
    s.set_plan(&r.id, PlanSummary { add: 1, change: 0, destroy: 0, outputs: 0 }, vec![], blob.clone()).await.unwrap();
    assert_eq!(s.get_plan_blob(&r.id).await.unwrap(), blob);
    assert!(matches!(s.get_plan_blob("missing").await, Err(StoreError::NotFound(_))));

    let content = "{\"resources\":{\"a.b\":\"1\"},\"unicode\":\"héllo ✓\"}";
    s.finalize_state(&ws.id, &r.id, content, 1).await.unwrap();
    assert_eq!(s.current_state(&ws.id).await.unwrap().unwrap().content, content);
    assert_eq!(s.list_states(&ws.id).await.unwrap()[0].content, content);
}

pub async fn users_orgs_and_roles(s: S) {
    let u = s.create_user("Ana@Example.com", "hash1", "Ana").await.unwrap();
    assert!(!u.email_verified);
    assert!(matches!(s.create_user("ana@example.COM", "h", "x").await, Err(StoreError::Conflict(_))), "email is case-insensitive");
    let (found, hash) = s.find_user_by_email("ANA@example.com").await.unwrap().unwrap();
    assert_eq!((found.id.as_str(), hash.as_str()), (u.id.as_str(), "hash1"));
    assert!(s.find_user_by_email("nobody@example.com").await.unwrap().is_none());

    s.set_password(&u.id, "hash2").await.unwrap();
    assert_eq!(s.find_user_by_email("ana@example.com").await.unwrap().unwrap().1, "hash2");
    s.mark_email_verified(&u.id).await.unwrap();
    assert!(s.get_user(&u.id).await.unwrap().email_verified);

    assert!(matches!(s.create_org("bad name", &u.id).await, Err(StoreError::Invalid(_))));
    let org = s.create_org("acme", &u.id).await.unwrap();
    assert!(matches!(s.create_org("acme", &u.id).await, Err(StoreError::Conflict(_))));
    assert_eq!(s.member_role(&org.id, &u.id).await.unwrap().as_deref(), Some("owner"));
    let orgs = s.orgs_for_user(&u.id).await.unwrap();
    assert_eq!((orgs.len(), orgs[0].0.name.as_str(), orgs[0].1.as_str()), (1, "acme", "owner"));

    let other = s.create_user("bo@example.com", "h", "Bo").await.unwrap();
    assert!(s.member_role(&org.id, &other.id).await.unwrap().is_none(), "strangers have no role");
    assert!(s.orgs_for_user(&other.id).await.unwrap().is_empty());
    assert!(matches!(s.get_user("missing").await, Err(StoreError::NotFound(_))));
}

pub async fn sessions_expire_and_sign_out_everywhere(s: S) {
    let u = s.create_user("a@example.com", "h", "A").await.unwrap();
    let t = now();
    s.create_session(&u.id, b"hash-one", t + 100).await.unwrap();
    s.create_session(&u.id, b"hash-two", t + 100).await.unwrap();
    assert_eq!(s.session_user(b"hash-one", t).await.unwrap().as_deref(), Some(u.id.as_str()));
    assert!(s.session_user(b"hash-one", t + 101).await.unwrap().is_none(), "expired");
    assert!(s.session_user(b"unknown", t).await.unwrap().is_none());

    s.delete_session(b"hash-one").await.unwrap();
    assert!(s.session_user(b"hash-one", t).await.unwrap().is_none());
    assert!(s.session_user(b"hash-two", t).await.unwrap().is_some());
    s.delete_user_sessions(&u.id).await.unwrap();
    assert!(s.session_user(b"hash-two", t).await.unwrap().is_none(), "signed out everywhere");
}

pub async fn api_tokens(s: S) {
    let (u, o) = user_and_org(&s, "a").await;
    let (_, other) = user_and_org(&s, "b").await;
    let t = now();
    let user_tok = s.create_api_token("user", Some(&u.id), &o.id, "ci", b"h-user", None).await.unwrap();
    let org_tok = s.create_api_token("org", None, &o.id, "bot", b"h-org", Some(t + 10)).await.unwrap();
    assert!(matches!(s.create_api_token("user", None, &o.id, "x", b"h-bad", None).await, Err(StoreError::Invalid(_))));
    assert!(matches!(s.create_api_token("weird", None, &o.id, "x", b"h-bad2", None).await, Err(StoreError::Invalid(_))));

    let found = s.find_api_token(b"h-user", t).await.unwrap().unwrap();
    assert_eq!((found.kind.as_str(), found.user_id.as_deref()), ("user", Some(u.id.as_str())));
    assert_eq!(s.find_api_token(b"h-org", t).await.unwrap().unwrap().org_id, o.id);
    assert!(s.find_api_token(b"h-org", t + 11).await.unwrap().is_none(), "expired");
    assert!(s.find_api_token(b"nope", t).await.unwrap().is_none());
    assert_eq!(s.list_api_tokens(&o.id).await.unwrap().len(), 2);
    assert!(s.list_api_tokens(&other.id).await.unwrap().is_empty());

    assert!(matches!(s.delete_api_token(&other.id, &user_tok.id).await, Err(StoreError::NotFound(_))), "not across orgs");
    s.delete_api_token(&o.id, &user_tok.id).await.unwrap();
    assert!(s.find_api_token(b"h-user", t).await.unwrap().is_none());
    assert_eq!(org_tok.kind, "org");
}

pub async fn email_tokens_are_single_use(s: S) {
    let u = s.create_user("a@example.com", "h", "A").await.unwrap();
    let t = now();
    s.create_email_token(&u.id, "verify", b"tok-v", t + 100).await.unwrap();
    assert!(s.consume_email_token(b"tok-v", "reset", t).await.unwrap().is_none(), "wrong purpose");
    assert_eq!(s.consume_email_token(b"tok-v", "verify", t).await.unwrap().as_deref(), Some(u.id.as_str()));
    assert!(s.consume_email_token(b"tok-v", "verify", t).await.unwrap().is_none(), "single use");

    s.create_email_token(&u.id, "reset", b"tok-r", t + 5).await.unwrap();
    assert!(s.consume_email_token(b"tok-r", "reset", t + 6).await.unwrap().is_none(), "expired");
}

pub async fn variables_upsert_redact_and_delete(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let (ws, _) = ws_with_cfg(&s, &o.id, "prod").await;
    let nv = |key: &str, cat: &str, sens: bool, val: &str| NewVariable {
        key: key.into(),
        category: cat.into(),
        hcl: false,
        sensitive: sens,
        value: val.into(),
        description: String::new(),
    };
    assert!(matches!(s.set_variable(&ws.id, nv("bad-key", "terraform", false, "x")).await, Err(StoreError::Invalid(_))));
    assert!(matches!(s.set_variable(&ws.id, nv("K", "weird", false, "x")).await, Err(StoreError::Invalid(_))));
    assert!(matches!(s.set_variable("missing", nv("K", "env", false, "x")).await, Err(StoreError::NotFound(_))));

    let secret = s.set_variable(&ws.id, nv("DB_PASSWORD", "env", true, "hunter2")).await.unwrap();
    assert!(secret.sensitive && secret.value.is_none(), "the response never echoes a sensitive value");
    s.set_variable(&ws.id, nv("region", "terraform", false, "eu-west-1")).await.unwrap();

    let listed = s.list_variables(&ws.id).await.unwrap();
    assert_eq!(listed.len(), 2);
    let l_secret = listed.iter().find(|v| v.key == "DB_PASSWORD").unwrap();
    assert!(l_secret.value.is_none(), "listing redacts sensitive values");
    assert_eq!(listed.iter().find(|v| v.key == "region").unwrap().value.as_deref(), Some("eu-west-1"));

    let for_run = s.variables_for_run(&ws.id).await.unwrap();
    assert_eq!(for_run.iter().find(|v| v.key == "DB_PASSWORD").unwrap().value.as_deref(), Some("hunter2"));

    // same (category, key) replaces
    s.set_variable(&ws.id, nv("DB_PASSWORD", "env", true, "rotated")).await.unwrap();
    assert_eq!(s.list_variables(&ws.id).await.unwrap().len(), 2);
    let for_run = s.variables_for_run(&ws.id).await.unwrap();
    assert_eq!(for_run.iter().find(|v| v.key == "DB_PASSWORD").unwrap().value.as_deref(), Some("rotated"));
    // same key in the other category is a different variable
    s.set_variable(&ws.id, nv("DB_PASSWORD", "terraform", false, "x")).await.unwrap();
    assert_eq!(s.list_variables(&ws.id).await.unwrap().len(), 3);

    assert!(matches!(s.delete_variable(&ws.id, "missing").await, Err(StoreError::NotFound(_))));
    let ids: Vec<_> = s.list_variables(&ws.id).await.unwrap().into_iter().map(|v| v.id).collect();
    for id in ids {
        s.delete_variable(&ws.id, &id).await.unwrap();
    }
    assert!(s.list_variables(&ws.id).await.unwrap().is_empty());
}

pub async fn deleting_a_user_deletes_what_only_they_owned(s: S) {
    let a = s.create_user("a@example.com", "h", "A").await.unwrap();
    let b = s.create_user("b@example.com", "h", "B").await.unwrap();
    let solo = s.create_org("solo", &a.id).await.unwrap();
    let (solo_ws, solo_cv) = ws_with_cfg(&s, &solo.id, "solo-ws").await;
    let solo_run = s.create_run(&solo_ws.id, &solo_cv.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    let shared = s.create_org("shared", &a.id).await.unwrap();
    let (shared_ws, _) = ws_with_cfg(&s, &shared.id, "shared-ws").await;
    s.add_member(&shared.id, &b.id, "member").await.unwrap();
    assert!(matches!(s.add_member(&shared.id, &b.id, "member").await, Err(StoreError::Conflict(_))));
    assert!(matches!(s.add_member(&shared.id, &b.id, "boss").await, Err(StoreError::Invalid(_))));

    s.create_session(&a.id, b"a-session", now() + 100).await.unwrap();
    s.create_api_token("user", Some(&a.id), &shared.id, "ci", b"a-token", None).await.unwrap();

    s.delete_user(&a.id).await.unwrap();

    assert!(matches!(s.get_user(&a.id).await, Err(StoreError::NotFound(_))));
    assert!(s.find_user_by_email("a@example.com").await.unwrap().is_none());
    assert!(s.session_user(b"a-session", now()).await.unwrap().is_none());
    assert!(s.find_api_token(b"a-token", now()).await.unwrap().is_none());
    assert!(matches!(s.get_workspace(&solo_ws.id).await, Err(StoreError::NotFound(_))), "sole-owner org and its data are gone");
    assert!(matches!(s.get_run(&solo_run.id).await, Err(StoreError::NotFound(_))));
    assert!(s.get_workspace(&shared_ws.id).await.is_ok(), "an org with another member survives");
    assert_eq!(s.member_role(&shared.id, &b.id).await.unwrap().as_deref(), Some("member"));
    assert!(matches!(s.delete_user("missing").await, Err(StoreError::NotFound(_))));
}

pub async fn force_unlock_ends_the_run_that_held_the_lock(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let (ws, cv) = ws_with_cfg(&s, &o.id, "one").await;
    let a = s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "a", false, "t").await.unwrap();
    let b = s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "b", false, "t").await.unwrap();

    // a planning run: force-unlock cancels it, and the next run may start
    let Some(Claim::Plan(c)) = s.claim_next().await.unwrap() else { panic!() };
    assert_eq!(c.id, a.id);
    s.unlock_workspace(&ws.id, true).await.unwrap();
    assert_eq!(s.get_run(&a.id).await.unwrap().status, RunStatus::Canceled, "the run that held the lock must not keep running");
    let Some(Claim::Plan(c)) = s.claim_next().await.unwrap() else { panic!("the queue must move on") };
    assert_eq!(c.id, b.id);

    // a run waiting for confirmation: force-unlock discards it
    paused(&s, &b.id).await;
    s.unlock_workspace(&ws.id, true).await.unwrap();
    assert_eq!(s.get_run(&b.id).await.unwrap().status, RunStatus::Discarded);
    assert!(s.get_workspace(&ws.id).await.unwrap().lock.is_none());

    // a run that is applying cannot be unlocked from under itself
    let d = s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "d", false, "t").await.unwrap();
    s.claim_next().await.unwrap();
    paused(&s, &d.id).await;
    s.apply_event(&d.id, Event::Confirm).await.unwrap();
    let Some(Claim::Apply(_)) = s.claim_next().await.unwrap() else { panic!() };
    assert!(matches!(s.unlock_workspace(&ws.id, true).await, Err(StoreError::Conflict(_))));
    assert_eq!(s.get_run(&d.id).await.unwrap().status, RunStatus::Applying);
    assert!(s.get_workspace(&ws.id).await.unwrap().lock.is_some(), "the lock stays while applying");
}

pub async fn interrupted_runs_are_recovered(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let (w1, c1) = ws_with_cfg(&s, &o.id, "planning").await;
    let (w2, c2) = ws_with_cfg(&s, &o.id, "paused").await;
    let (w3, c3) = ws_with_cfg(&s, &o.id, "applying").await;
    let (w4, c4) = ws_with_cfg(&s, &o.id, "waiting").await;
    let r1 = s.create_run(&w1.id, &c1.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    let r2 = s.create_run(&w2.id, &c2.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    let r3 = s.create_run(&w3.id, &c3.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    let spec = s.create_run(&w4.id, &c4.id, Operation::PlanOnly, "", false, "t").await.unwrap();

    for _ in 0..4 {
        s.claim_next().await.unwrap(); // r1, r2, r3 and the speculative plan start planning
    }
    // r2 pauses for confirmation, r3 goes on to apply
    paused(&s, &r2.id).await;
    paused(&s, &r3.id).await;
    s.apply_event(&r3.id, Event::Confirm).await.unwrap();
    let Some(Claim::Apply(a)) = s.claim_next().await.unwrap() else { panic!("r3 should be applying") };
    assert_eq!(a.id, r3.id);
    // a run that has not started yet
    let r4 = s.create_run(&w4.id, &c4.id, Operation::PlanAndApply, "", false, "t").await.unwrap();

    let n = s.recover_interrupted().await.unwrap();
    assert_eq!(n, 3, "r1 (planning), r3 (applying) and the speculative plan are interrupted");

    for id in [&r1.id, &r3.id, &spec.id] {
        let r = s.get_run(id).await.unwrap();
        assert_eq!(r.status, RunStatus::Errored, "{id}");
        assert!(s.get_log(id).await.unwrap().iter().any(|l| l.contains("interrupted")), "the log says why");
    }
    assert!(s.get_workspace(&w1.id).await.unwrap().lock.is_none(), "an interrupted planning run releases its lock");
    assert!(s.get_workspace(&w3.id).await.unwrap().lock.is_none(), "an interrupted apply releases its lock");
    assert_eq!(s.get_run(&r2.id).await.unwrap().status, RunStatus::NeedsConfirmation, "a run waiting for a person survives a restart");
    assert!(s.get_workspace(&w2.id).await.unwrap().lock.is_some());
    assert_eq!(s.get_run(&r4.id).await.unwrap().status, RunStatus::Pending);
    assert_eq!(s.recover_interrupted().await.unwrap(), 0, "nothing left to recover");
    // and the queue works again on a recovered workspace
    s.create_run(&w1.id, &c1.id, Operation::PlanAndApply, "again", false, "t").await.unwrap();
    assert!(s.claim_next().await.unwrap().is_some());
}

pub async fn cli_locks_carry_their_info_and_only_their_owner_releases_them(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let (ws, cv) = ws_with_cfg(&s, &o.id, "prod").await;
    let info = r#"{"ID":"abc-123","Operation":"OperationTypeApply","Who":"ana@laptop"}"#;
    let locked = s.lock_workspace(&ws.id, "cli:abc-123", "OperationTypeApply by ana@laptop", Some(info), None).await.unwrap();
    assert_eq!(locked.lock.as_ref().unwrap().info.as_deref(), Some(info));
    assert_eq!(s.get_workspace(&ws.id).await.unwrap().lock.unwrap().info.as_deref(), Some(info), "the info is stored");
    assert!(matches!(s.lock_workspace(&ws.id, "cli:other", "x", None, None).await, Err(StoreError::Conflict(_))));

    // it blocks runs like any lock
    s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    assert!(s.claim_next().await.unwrap().is_none());

    assert!(matches!(s.unlock_workspace_owned(&ws.id, "cli:wrong").await, Err(StoreError::Conflict(_))), "only the owner releases it");
    assert!(s.get_workspace(&ws.id).await.unwrap().lock.is_some());
    let freed = s.unlock_workspace_owned(&ws.id, "cli:abc-123").await.unwrap();
    assert!(freed.lock.is_none());
    assert!(matches!(s.unlock_workspace_owned(&ws.id, "cli:abc-123").await, Err(StoreError::Conflict(_))), "already unlocked");
    assert!(matches!(s.unlock_workspace_owned("missing", "cli:x").await, Err(StoreError::NotFound(_))));
    assert!(s.claim_next().await.unwrap().is_some(), "the queued run starts once the CLI lets go");

    // a manual lock has no info
    s.unlock_workspace(&ws.id, true).await.unwrap();
    let manual = s.lock_workspace(&ws.id, "user", "by hand", None, None).await.unwrap();
    assert!(manual.lock.unwrap().info.is_none());
}

pub async fn state_pushed_without_a_run_has_no_run(s: S) {
    let (_, o) = user_and_org(&s, "a").await;
    let ws = s.create_workspace(&o.id, "prod").await.unwrap();
    let sv = s.finalize_state(&ws.id, "", "{\"version\":4,\"serial\":7,\"lineage\":\"L\",\"resources\":[{},{}]}", 2).await.unwrap();
    assert!(sv.run_id.is_none(), "an empty run id means no run");
    assert!(s.current_state(&ws.id).await.unwrap().unwrap().run_id.is_none());
    assert_eq!(s.list_states(&ws.id).await.unwrap().len(), 1);
}

pub async fn force_cancel_ends_a_working_run_and_frees_the_workspace(s: S) {
    let (_, o) = user_and_org(&s, "a").await;

    // planning
    let (w1, c1) = ws_with_cfg(&s, &o.id, "planning").await;
    let r1 = s.create_run(&w1.id, &c1.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    s.claim_next().await.unwrap();
    assert_eq!(s.apply_event(&r1.id, Event::ForceCancel).await.unwrap().status, RunStatus::ForceCanceled);
    assert!(s.get_workspace(&w1.id).await.unwrap().lock.is_none());

    // queued to apply
    let (w2, c2) = ws_with_cfg(&s, &o.id, "queued").await;
    let r2 = s.create_run(&w2.id, &c2.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    s.claim_next().await.unwrap();
    paused(&s, &r2.id).await;
    s.apply_event(&r2.id, Event::Confirm).await.unwrap();
    assert_eq!(s.apply_event(&r2.id, Event::ForceCancel).await.unwrap().status, RunStatus::ForceCanceled);
    assert!(s.get_workspace(&w2.id).await.unwrap().lock.is_none());

    // applying: the one a plain cancel refuses
    let (w3, c3) = ws_with_cfg(&s, &o.id, "applying").await;
    let r3 = s.create_run(&w3.id, &c3.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    s.claim_next().await.unwrap();
    paused(&s, &r3.id).await;
    s.apply_event(&r3.id, Event::Confirm).await.unwrap();
    let Some(Claim::Apply(_)) = s.claim_next().await.unwrap() else { panic!("applying") };
    assert!(matches!(s.apply_event(&r3.id, Event::Cancel).await, Err(StoreError::Conflict(_))), "a plain cancel is refused while applying");
    let ended = s.apply_event(&r3.id, Event::ForceCancel).await.unwrap();
    assert_eq!(ended.status, RunStatus::ForceCanceled);
    assert!(s.get_workspace(&w3.id).await.unwrap().lock.is_none(), "the lock is released");
    // late results from the stopped engine cannot revive it
    assert!(s.apply_event(&r3.id, Event::ApplyOk).await.is_err());
    // and the stored status round-trips
    assert_eq!(s.get_run(&r3.id).await.unwrap().status, RunStatus::ForceCanceled);
    assert!(s.list_runs_in_org(&o.id).await.unwrap().iter().all(|r| r.status == RunStatus::ForceCanceled));

    // a run waiting for a person, or not started, is not "working"
    let (w4, c4) = ws_with_cfg(&s, &o.id, "waiting").await;
    let r4 = s.create_run(&w4.id, &c4.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    assert!(matches!(s.apply_event(&r4.id, Event::ForceCancel).await, Err(StoreError::Conflict(_))), "pending");
    s.claim_next().await.unwrap();
    paused(&s, &r4.id).await;
    assert!(matches!(s.apply_event(&r4.id, Event::ForceCancel).await, Err(StoreError::Conflict(_))), "needs confirmation: discard it instead");

    // the workspace works again
    s.create_run(&w3.id, &c3.id, Operation::PlanAndApply, "again", false, "t").await.unwrap();
    assert!(s.claim_next().await.unwrap().is_some());
}
