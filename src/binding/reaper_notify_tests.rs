//! t-20260813113928166255-89366-26: RED-first coverage for the boot-time
//! orphan reaper's notification behavior (`binding::reconcile_orphans`).
//!
//! Prior behavior: `reconcile_orphans` deleted a stale binding with only a
//! `tracing::info!` — silent to the revoked instance and its team (see the
//! task board description for the incident: three days of real stall). These
//! tests pin the fix black-box, through `reconcile_orphans`'s OBSERABLE side
//! effects (binding.json removal + inbox contents) — not through any
//! internal helper — so they stay valid across an internal refactor.
//!
//! Case 2 and case 3 are REGRESSION GUARDS on the pre-existing three-condition
//! judgment (daemon-restart sweep call site / >24h `issued_at` / >1h stale
//! heartbeat, #693) — they must already be green against the unmodified
//! `reconcile_orphans` and stay green after the notify wiring lands, proving
//! the judgment logic itself is untouched (see PR description for the
//! mutation-testing evidence that binds them to the right lines).

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

fn tmp_home(tag: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-reaper-notify-test-{}-{}-{}",
        std::process::id(),
        tag,
        id
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// Write a `binding.json` fixture with an explicit `issued_at`. Deliberately
/// NOT reusing `binding.rs`'s private `tests::write_binding_json` (hardcodes
/// `issued_at` to a fixed constant, unsuitable for case 3's precise <24h
/// age) — a local fixture avoids touching a helper shared by unrelated tests.
fn write_binding(
    home: &Path,
    agent: &str,
    task_id: &str,
    branch: &str,
    worktree: &str,
    issued_at: chrono::DateTime<chrono::Utc>,
) {
    let dir = crate::paths::runtime_dir(home).join(agent);
    std::fs::create_dir_all(&dir).unwrap();
    let v = serde_json::json!({
        "version": 1,
        "agent": agent,
        "task_id": task_id,
        "branch": branch,
        "worktree": worktree,
        "issued_at": issued_at.to_rfc3339(),
    });
    std::fs::write(
        dir.join("binding.json"),
        serde_json::to_string_pretty(&v).unwrap(),
    )
    .unwrap();
}

fn binding_path(home: &Path, agent: &str) -> std::path::PathBuf {
    crate::paths::runtime_dir(home)
        .join(agent)
        .join("binding.json")
}

/// heartbeat_pair is a process-global registry keyed by name ALONE (no
/// home/tmp-dir isolation, no test reset — see its own doc comment for a
/// confirmed cross-test-file pollution incident). Every fixture agent name
/// here is crate-unique (task-id suffixed) per that module's stated mitigation.
fn set_heartbeat_stale_over_1h(agent: &str) {
    crate::daemon::heartbeat_pair::update_with(agent, |p| {
        p.heartbeat_at_ms = crate::daemon::heartbeat_pair::now_ms().saturating_sub(2 * 3_600_000);
    });
}

fn set_heartbeat_fresh(agent: &str) {
    crate::daemon::heartbeat_pair::update_with(agent, |p| {
        p.heartbeat_at_ms = crate::daemon::heartbeat_pair::now_ms();
    });
}

fn seed_team(home: &Path, orchestrator: &str, member: &str) {
    std::fs::write(
        crate::fleet::fleet_yaml_path(home),
        format!(
            "teams:\n  ops:\n    members: [{member}, {orchestrator}]\n    orchestrator: {orchestrator}\n"
        ),
    )
    .unwrap();
}

/// Case 1: all three removal conditions met (>24h issued_at, >1h stale
/// heartbeat) → binding removed AND both the revoked instance and its team
/// orchestrator receive a durable inbox notification carrying worktree /
/// branch / task_id and a `bind_self` recovery instruction.
#[test]
fn reconcile_orphans_notifies_revoked_instance_and_orchestrator_case1_89366_26() {
    let home = tmp_home("case1");
    let agent = "worker-89366-26-case1";
    let orch = "lead-89366-26-case1";
    seed_team(&home, orch, agent);
    write_binding(
        &home,
        agent,
        "t-fixture-1",
        "feat/stuck",
        "/wt/worker",
        chrono::Utc::now() - chrono::Duration::hours(25),
    );
    set_heartbeat_stale_over_1h(agent);

    crate::binding::reconcile_orphans(&home);

    assert!(
        !binding_path(&home, agent).exists(),
        "three conditions met: binding.json must still be removed (judgment unchanged)"
    );

    let worker_inbox = crate::inbox::storage::drain(&home, agent);
    assert_eq!(
        worker_inbox.len(),
        1,
        "revoked instance must receive exactly one notification: {worker_inbox:?}"
    );
    let text = &worker_inbox[0].text;
    assert!(text.contains("t-fixture-1"), "must carry task_id: {text}");
    assert!(text.contains("feat/stuck"), "must carry branch: {text}");
    assert!(text.contains("/wt/worker"), "must carry worktree: {text}");
    assert!(
        text.contains("bind_self"),
        "must instruct bind_self recovery: {text}"
    );

    let orch_inbox = crate::inbox::storage::drain(&home, orch);
    assert_eq!(
        orch_inbox.len(),
        1,
        "team orchestrator must also receive exactly one notification: {orch_inbox:?}"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// Case 2 (regression guard, #693): heartbeat within 1h → binding is NOT
/// removed, even though `issued_at` is well past 24h.
#[test]
fn reconcile_orphans_skips_fresh_heartbeat_case2_693_regression() {
    let home = tmp_home("case2");
    let agent = "worker-89366-26-case2";
    write_binding(
        &home,
        agent,
        "t-fixture-2",
        "feat/active",
        "/wt/worker2",
        chrono::Utc::now() - chrono::Duration::hours(25),
    );
    set_heartbeat_fresh(agent);

    crate::binding::reconcile_orphans(&home);

    assert!(
        binding_path(&home, agent).exists(),
        "#693: a fresh-heartbeat binding must survive reconcile_orphans"
    );
    assert!(
        crate::inbox::storage::drain(&home, agent).is_empty(),
        "no removal must mean no notification"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// Case 3 (regression guard): binding under 24h old → NOT removed, even with
/// a stale heartbeat. Heartbeat is deliberately set STALE here (not fresh) —
/// so a mutation-tested age threshold (e.g. `hours(24)` → `hours(0)`) isn't
/// masked by the heartbeat guard also blocking removal for an unrelated
/// reason; this is what makes the mutation evidence bind to the age line.
#[test]
fn reconcile_orphans_skips_young_binding_case3_regression() {
    let home = tmp_home("case3");
    let agent = "worker-89366-26-case3";
    write_binding(
        &home,
        agent,
        "t-fixture-3",
        "feat/young",
        "/wt/worker3",
        chrono::Utc::now() - chrono::Duration::hours(1),
    );
    set_heartbeat_stale_over_1h(agent);

    crate::binding::reconcile_orphans(&home);

    assert!(
        binding_path(&home, agent).exists(),
        "a binding under 24h old must survive reconcile_orphans"
    );
    assert!(
        crate::inbox::storage::drain(&home, agent).is_empty(),
        "no removal must mean no notification"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// Case 4 (deliberate scope pin, d-20260814042446102235-10 follow-up): a
/// TEAMLESS revoked instance (no team lists it) is removed and notified
/// itself, but there is NO fallback-to-`general` escalation. This is a
/// decision, not an oversight — see PR description for the actionability
/// rationale (a non-owning `general` recipient can only relay, never act,
/// and an unactionable recurring notice is a documented noise source that
/// erodes attention to the whole notification class, t-20260814003031365538-
/// 20742-0).
#[test]
fn reconcile_orphans_teamless_notifies_only_self_no_general_fallback_case4() {
    let home = tmp_home("case4");
    let agent = "worker-89366-26-case4";
    // fleet.yaml exists but lists no team for `agent` at all.
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "teams:\n  ops:\n    members: [someoneelse]\n    orchestrator: someoneelse\n",
    )
    .unwrap();
    write_binding(
        &home,
        agent,
        "t-fixture-4",
        "feat/lonely",
        "/wt/worker4",
        chrono::Utc::now() - chrono::Duration::hours(25),
    );
    set_heartbeat_stale_over_1h(agent);

    crate::binding::reconcile_orphans(&home);

    assert!(
        !binding_path(&home, agent).exists(),
        "three conditions met: binding.json must still be removed"
    );
    assert_eq!(
        crate::inbox::storage::drain(&home, agent).len(),
        1,
        "the teamless revoked instance itself must still be notified"
    );
    assert!(
        crate::inbox::storage::drain(&home, "general").is_empty(),
        "teamless must NOT fall back to a `general` notification — no actionable owner, no send"
    );

    std::fs::remove_dir_all(&home).ok();
}
