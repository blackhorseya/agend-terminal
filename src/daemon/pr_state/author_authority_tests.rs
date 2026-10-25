//! -18577-7 r3 (d-20261025004632404221-3, Sean 2026-10-25, option A): the PR
//! author is identified from AUTHORITATIVE sources only — the typed
//! review_assignment's `review_author`, the branch's live binding, the task
//! assignee — never from who subscribed to CI. Subscribers decide who gets
//! notices, not who the author is. When no source identifies exactly one
//! author the notice fails closed (d-20261025000739159728-1): nothing is
//! sent and one WARN per PR is logged.
//!
//! third-hq-1 REJECTED r2 (2c6a8b7e) on five P1s; every test here drives a
//! PRODUCTION entry (`scan_and_emit_with`, `record_validated_receipt`) and
//! asserts the inbox rows that were (not) enqueued — the r2 tests exercised
//! inner helpers and missed the poller gate (`pr_author.is_empty()`).
//!
//! The fixture mirrors the live fleet shape that produced P1-5: two
//! orchestrators in two teams share one `github_login`, both teams own the
//! repo, and the second team has a reviewer.

#![allow(clippy::unwrap_used)]

use super::gh_poll::tests::MockGhPoller;
use super::gh_poll::{GhPrMetadata, GhPrState};
use super::*;
use crate::mcp::handlers::comms_gates::ReviewAuthor;

const REPO: &str = "acme/tool";
const LOGIN: &str = "shared-bot";

/// personal-hq / third-hq shape: `personal` and `third` are orchestrators of
/// two teams that both own `acme/tool`; `third-rev` is third-hq's reviewer.
const TWO_TEAM_FLEET: &str = "instances:\n  \
     personal:\n    backend: claude\n    role_kind: orchestrator\n    github_login: shared-bot\n  \
     third:\n    backend: claude\n    role_kind: orchestrator\n    github_login: shared-bot\n  \
     third-rev:\n    backend: codex\n    role_kind: reviewer\n    github_login: shared-bot\n\
     teams:\n  \
     personal-hq:\n    orchestrator: personal\n    members: [personal]\n    source_repo: acme/tool\n  \
     third-hq:\n    orchestrator: third\n    members: [third, third-rev]\n    source_repo: acme/tool\n";

fn home_with_fleet(tag: &str, yaml: &str) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!(
        "agend-18577-7-r3-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(home.join("inbox")).unwrap();
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    home
}

fn registry() -> crate::agent::AgentRegistry {
    std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()))
}

fn scan(home: &std::path::Path, prs: Vec<GhPrMetadata>) {
    let poller = MockGhPoller::new(vec![Ok(prs)]);
    super::scanner::scan_and_emit_with(home, &registry(), &poller);
}

fn merged_state(branch: &str, subscribers: &[&str], pr_author: &str) -> PrState {
    let mut s = new_for_branch(REPO, branch, &"a".repeat(40), ReviewClass::Single);
    s.pr_number = 15;
    s.subscribers = subscribers.iter().map(|s| s.to_string()).collect();
    s.pr_author = pr_author.to_string();
    s.merge_state = MergeState::Merged {
        merge_commit: "m".repeat(40),
        merged_at: "2026-10-25T00:00:00Z".to_string(),
    };
    s
}

fn open_meta(branch: &str) -> GhPrMetadata {
    GhPrMetadata {
        number: 15,
        author_login: LOGIN.to_string(),
        head_ref: branch.to_string(),
        is_cross_repository: false,
        is_draft: false,
        state: GhPrState::Open,
        merged_at: None,
        head_ref_oid: Some("a".repeat(40)),
        base_ref_oid: Some("b".repeat(40)),
    }
}

/// A typed review assignment of `branch` to `target`, authored by `author`.
fn assign_review(home: &std::path::Path, branch: &str, target: &str, author: ReviewAuthor) {
    let record = crate::daemon::assignment_authority::ActiveAssignment::new_pending_typed(
        REPO,
        branch,
        target,
        crate::types::InstanceId::new(),
        15,
        "a".repeat(40),
        crate::review_receipt::ReviewSlot::Primary,
        "third",
        format!("t-review-{branch}"),
        ReviewClass::Single,
        author,
        "review",
        None,
        None,
        "2026-10-25T00:00:00Z",
    );
    crate::daemon::assignment_authority::persist(home, &record).unwrap();
}

/// A task on `branch` owned by `owner` (the board route the daemon uses).
fn task_on_branch(home: &std::path::Path, task_id: &str, owner: &str, branch: &str) {
    use crate::task_events::{append_batch, InstanceName, TaskEvent, TaskId};
    append_batch(
        home,
        &InstanceName::from("test:seed"),
        vec![
            TaskEvent::Created {
                task_id: TaskId(task_id.into()),
                title: task_id.into(),
                description: String::new(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: vec![],
                parent_id: None,
                governing_decision_id: None,
                review_class: None,
            },
            TaskEvent::Claimed {
                task_id: TaskId(task_id.into()),
                by: InstanceName::from(owner),
            },
        ],
    )
    .unwrap();
    crate::tasks::link_branch_to_task(home, task_id, branch).unwrap();
}

fn kinds(home: &std::path::Path, who: &str) -> Vec<String> {
    crate::inbox::drain(home, who)
        .into_iter()
        .filter_map(|m| m.kind)
        .collect()
}

fn exactly(lines: &[&str], needle: &str, want: usize) -> Result<(), String> {
    let got = lines.iter().filter(|l| l.contains(needle)).count();
    if got == want {
        Ok(())
    } else {
        Err(format!(
            "`{needle}`: {got} lines, want {want}\n{}",
            lines.join("\n")
        ))
    }
}

// ── authoritative sources identify the author ────────────────────────────

/// P1-5 (the live PR #15 shape): both orchestrators watch the PR and share
/// the login. The typed assignment names the author, so `[pr-merged]` goes
/// to `personal` — not to nobody (r2) and not to `third`.
#[test]
fn two_watchers_same_login_author_comes_from_the_review_assignment() {
    let home = home_with_fleet("asg", TWO_TEAM_FLEET);
    let branch = "feat/asg";
    assign_review(
        &home,
        branch,
        "third-rev",
        ReviewAuthor::Agent("personal".into()),
    );
    save(&home, &merged_state(branch, &["personal", "third"], "")).unwrap();
    scan(&home, vec![]);
    assert_eq!(kinds(&home, "personal"), vec!["pr-merged"]);
    assert!(
        kinds(&home, "third").is_empty(),
        "the other watcher is not the author"
    );
    assert!(
        kinds(&home, "third-rev").is_empty(),
        "the reviewer is never the author"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// No assignment: the live binding on the branch identifies the author. The
/// reviewer's own binding (review worktree) does not make it the author.
#[test]
fn live_binding_identifies_the_author_and_a_bound_reviewer_does_not() {
    let home = home_with_fleet("bind", TWO_TEAM_FLEET);
    let branch = "feat/bind";
    crate::binding::bind(&home, "third-rev", "t-review", branch);
    crate::binding::bind(&home, "personal", "t-author", branch);
    save(&home, &merged_state(branch, &["personal", "third"], "")).unwrap();
    scan(&home, vec![]);
    assert_eq!(kinds(&home, "personal"), vec!["pr-merged"]);
    assert!(kinds(&home, "third").is_empty());
    assert!(kinds(&home, "third-rev").is_empty());
    let _ = std::fs::remove_dir_all(&home);
}

/// No assignment, no binding (released after push): the task carrying the
/// branch names its assignee. The reviewer's task on the same branch does not.
#[test]
fn task_assignee_identifies_the_author_after_the_binding_is_released() {
    let home = home_with_fleet("task", TWO_TEAM_FLEET);
    let branch = "feat/task";
    task_on_branch(&home, "t-author-task", "personal", branch);
    task_on_branch(&home, "t-review-task", "third-rev", branch);
    save(&home, &merged_state(branch, &["personal", "third"], "")).unwrap();
    scan(&home, vec![]);
    assert_eq!(kinds(&home, "personal"), vec!["pr-merged"]);
    assert!(kinds(&home, "third").is_empty());
    assert!(kinds(&home, "third-rev").is_empty());
    let _ = std::fs::remove_dir_all(&home);
}

// ── subscribers never identify the author ────────────────────────────────

/// P1-3: only another team's reviewer watches the PR (a cross-domain
/// `ci action=watch`). At the first poll r2 narrowed the shared-login hits to
/// that watcher's team, stored `third`, and the merge notice went to `third`.
/// Nothing identifies the author → nothing is sent, one WARN across the poll
/// and the emit.
#[test]
#[tracing_test::traced_test]
fn a_foreign_reviewer_watcher_cannot_steer_the_author_notice() {
    let home = home_with_fleet("steer", TWO_TEAM_FLEET);
    let branch = "feat/steer";
    crate::binding::bind(&home, "third-rev", "t-review", branch);
    let mut s = merged_state(branch, &["third-rev"], "");
    s.merge_state = MergeState::NotReady;
    save(&home, &s).unwrap();
    scan(&home, vec![open_meta(branch)]);
    let mut s = load(&home, REPO, branch).unwrap();
    s.merge_state = MergeState::Merged {
        merge_commit: "m".repeat(40),
        merged_at: "2026-10-25T00:00:00Z".to_string(),
    };
    save(&home, &s).unwrap();
    scan(&home, vec![]);
    // `third` may still receive merge-AUTHORITY notices through its reviewer's
    // subscription; it must not receive the AUTHOR notice.
    for who in ["personal", "third", "third-rev"] {
        assert!(
            !kinds(&home, who).iter().any(|k| k == "pr-merged"),
            "{who} must not receive [pr-merged]"
        );
    }
    logs_assert(|lines| exactly(lines, "failing closed (no author notice)", 1));
    let _ = std::fs::remove_dir_all(&home);
}

/// Two non-reviewer bindings disagree: a tier that names more than one author
/// fails closed instead of guessing, and does not fall through to a weaker tier.
#[test]
fn conflicting_authoritative_sources_fail_closed() {
    let home = home_with_fleet("conflict", TWO_TEAM_FLEET);
    let branch = "feat/conflict";
    crate::binding::bind(&home, "personal", "t-a", branch);
    crate::binding::bind(&home, "third", "t-b", branch);
    task_on_branch(&home, "t-only-task", "personal", branch);
    save(&home, &merged_state(branch, &["personal", "third"], "")).unwrap();
    scan(&home, vec![]);
    assert!(kinds(&home, "personal").is_empty());
    assert!(kinds(&home, "third").is_empty());
    let _ = std::fs::remove_dir_all(&home);
}

// ── persisted `pr_author` is never trusted without provenance ────────────

/// P1-1 through the production poller: a reviewer persisted as `pr_author`
/// by the old tier-3 guess is non-empty, so r2's poller never rebuilt the
/// index and never repaired it. The next poll must repair it from the
/// authoritative source.
#[test]
fn persisted_reviewer_author_is_repaired_by_the_production_poll() {
    let home = home_with_fleet("repair", TWO_TEAM_FLEET);
    let branch = "feat/repair";
    assign_review(
        &home,
        branch,
        "third-rev",
        ReviewAuthor::Agent("personal".into()),
    );
    let mut s = new_for_branch(REPO, branch, &"a".repeat(40), ReviewClass::Single);
    s.pr_number = 15;
    s.subscribers = vec!["third-rev".to_string()];
    s.pr_author = "third-rev".to_string();
    save(&home, &s).unwrap();
    scan(&home, vec![open_meta(branch)]);
    let after = load(&home, REPO, branch).unwrap();
    assert_eq!(after.pr_author, "personal");
    let _ = std::fs::remove_dir_all(&home);
}

/// P1-2: a non-reviewer persisted by r1's alphabetical pick (`third`) is
/// eligible, so r2's emit-time check accepted it without re-running the
/// cross-team rule. Without provenance and without an authoritative source
/// the stored value sends nothing.
#[test]
fn persisted_cross_team_author_without_provenance_sends_nothing() {
    let home = home_with_fleet("legacy", TWO_TEAM_FLEET);
    let branch = "feat/legacy";
    save(&home, &merged_state(branch, &[], "third")).unwrap();
    scan(&home, vec![]);
    assert!(kinds(&home, "third").is_empty());
    assert!(kinds(&home, "personal").is_empty());
    let _ = std::fs::remove_dir_all(&home);
}

/// P1-2 (merge-authority half): a persisted reviewer must not route
/// `[pr-ready-for-merge]` to its own team's orchestrator when nothing else
/// identifies the PR's team.
#[test]
fn merge_authority_ignores_an_untrusted_stored_reviewer_author() {
    let home = home_with_fleet("authority", TWO_TEAM_FLEET);
    let idx = gh_poll::AuthorIndex::build(&home, REPO);
    let mut s = new_for_branch(REPO, "feat/authority", "sha", ReviewClass::Single);
    s.pr_number = 15;
    s.subscribers.clear();
    s.pr_author = "third-rev".to_string();
    assert_eq!(resolve_merge_authority_checked(&home, &s, &idx), None);
    let _ = std::fs::remove_dir_all(&home);
}

/// P1-4: the validated-review fan-out used the unfiltered stored author. With
/// `pr_author = "third-rev"` the complete review report went to the reviewer.
/// The fan-out must go to the authoritative author instead.
#[test]
fn validated_review_fan_out_goes_to_the_authoritative_author_not_a_stored_reviewer() {
    let home = home_with_fleet("fanout", TWO_TEAM_FLEET);
    let branch = "feat/fanout";
    let head = "a".repeat(40);
    let reviewer_id = crate::types::InstanceId::new();
    let record = crate::daemon::assignment_authority::ActiveAssignment::new_pending_typed(
        REPO,
        branch,
        "third-rev",
        reviewer_id,
        15,
        &head,
        crate::review_receipt::ReviewSlot::Primary,
        "third",
        "t-review-fanout",
        ReviewClass::Single,
        ReviewAuthor::Agent("personal".into()),
        "review",
        None,
        None,
        "2026-10-25T00:00:00Z",
    );
    crate::daemon::assignment_authority::persist(&home, &record).unwrap();
    let mut s = new_for_branch(REPO, branch, &head, ReviewClass::Single);
    s.pr_number = 15;
    s.subscribers = vec!["third-rev".to_string()];
    s.pr_author = "third-rev".to_string();
    save(&home, &s).unwrap();
    let receipt = crate::review_receipt::ValidatedCodeReviewReceipt::for_test(
        crate::review_receipt::ReviewReceiptSummary {
            receipt_id: "review-receipt:m-fanout".into(),
            source_id: "m-fanout".into(),
            evidence_digest: "e".repeat(64),
            assignment_id: record.assignment_id,
            reviewer_instance_id: reviewer_id,
            reviewer_name: "third-rev".into(),
            repo: REPO.into(),
            pr_number: 15,
            branch: branch.into(),
            task_id: "t-review-fanout".into(),
            reviewed_head: head.clone(),
            review_class: ReviewClass::Single,
            slot: crate::review_receipt::ReviewSlot::Primary,
            verdict: crate::review_receipt::ReviewVerdict::Rejected,
        },
    );
    let report = crate::inbox::InboxMessage::new_system("third-rev", "report", "REJECTED …");
    assert!(record_validated_receipt(
        &home,
        &receipt,
        Some(("third", &report))
    ));
    assert!(
        kinds(&home, "third-rev").is_empty(),
        "the reviewer must not receive its own report as the 'author'"
    );
    assert_eq!(
        kinds(&home, "personal").len(),
        1,
        "the authoritative author gets the fan-out"
    );
    let _ = std::fs::remove_dir_all(&home);
}

// ── controls ─────────────────────────────────────────────────────────────

/// An operator-opened PR with no assignment, binding or task: the gh login
/// still identifies the author when it maps to exactly one team (x-brain,
/// goapi, x-infra today).
#[test]
fn single_team_login_still_identifies_the_author_without_other_sources() {
    let home = home_with_fleet(
        "login",
        "instances:\n  \
         lead:\n    backend: claude\n    role_kind: orchestrator\n    github_login: shared-bot\n  \
         rev:\n    backend: codex\n    role_kind: reviewer\n\
         teams:\n  one:\n    orchestrator: lead\n    members: [lead, rev]\n    source_repo: acme/tool\n",
    );
    let branch = "feat/login";
    let mut s = new_for_branch(REPO, branch, &"a".repeat(40), ReviewClass::Single);
    s.pr_number = 15;
    s.subscribers = vec!["rev".to_string()];
    save(&home, &s).unwrap();
    scan(&home, vec![open_meta(branch)]);
    assert_eq!(load(&home, REPO, branch).unwrap().pr_author, "lead");
    let _ = std::fs::remove_dir_all(&home);
}

// ---------------------------------------------------------------------------
// -18577-7 r4 — third-hq-1 REJECTED r3 (a24adfe9) on four P1s, each reproduced
// through the production scanner (receipt review-receipt:m-20261025015044219575-164).
// The attack tests below are the reviewer's (scratch-161/pr15-r3-adversarial-
// tests.patch, pr15-r3-stale-task.patch), taken as the RED set; the `r4_control_*`
// tests are the reviewer's controls that already pass on r3.
// ---------------------------------------------------------------------------

/// r4 P1-1: an UNREADABLE assignment record is not "no assignment". The author
/// chain must fail closed instead of falling to the binding tier.
#[test]
fn corrupt_assignment_does_not_downgrade_author_to_a_different_binding() {
    let home = home_with_fleet("corrupt-assignment-review", TWO_TEAM_FLEET);
    let branch = "feat/corrupt-assignment-review";
    assign_review(
        &home,
        branch,
        "third-rev",
        ReviewAuthor::Agent("personal".into()),
    );
    let path =
        crate::daemon::assignment_authority::record_path_for_test(&home, REPO, branch, "third-rev");
    std::fs::write(path, b"{").unwrap();
    crate::binding::bind(&home, "third", "t-new-binding", branch);
    save(&home, &merged_state(branch, &["third"], "")).unwrap();
    scan(&home, vec![]);
    assert!(
        kinds(&home, "third").is_empty(),
        "an unreadable higher-priority author record must not route to another binding"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// r4 P1-2: a reviewer whose `repo` override makes it ineligible for this repo
/// is bound to the branch (borrowed review). It is outside the `AuthorIndex`,
/// so the reviewer predicate cannot see it; the binding tier must require
/// repo eligibility like every other tier.
#[test]
fn repo_ineligible_reviewer_binding_does_not_become_author() {
    let home = home_with_fleet(
        "borrowed-reviewer-review",
        "instances:\n  rev:\n    backend: codex\n    role_kind: reviewer\n    repo: acme/other\n",
    );
    let branch = "feat/borrowed-reviewer-review";
    crate::binding::bind(&home, "rev", "t-review", branch);
    save(&home, &merged_state(branch, &[], "")).unwrap();
    scan(&home, vec![]);
    assert!(
        kinds(&home, "rev").is_empty(),
        "a reviewer borrowed into this branch must not be treated as the PR author"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// r4 P1-4: a stored `gh_login` author was valid while the login mapped to one
/// team (d-20261025014213077350-7). Once a second eligible team shares the
/// login the stored value must be re-validated and fail closed.
#[test]
fn stored_gh_login_author_is_rechecked_after_a_second_team_joins() {
    let home = home_with_fleet(
        "login-drift-review",
        "instances:\n  personal:\n    backend: claude\n    role_kind: orchestrator\n    github_login: shared-bot\nteams:\n  personal-hq:\n    orchestrator: personal\n    members: [personal]\n    source_repo: acme/tool\n",
    );
    let branch = "feat/login-drift-review";
    let mut state = merged_state(branch, &[], "personal");
    state.pr_author_source = Some(super::author_authority::AuthorSource::GhLogin);
    state.pr_author_pr_number = state.pr_number;
    save(&home, &state).unwrap();
    std::fs::write(crate::fleet::fleet_yaml_path(&home), TWO_TEAM_FLEET).unwrap();
    scan(&home, vec![]);
    assert!(
        kinds(&home, "personal").is_empty(),
        "a saved GH-login inference must fail closed once two teams match that login"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// r4 P1-3a: a typed assignment of PR #14 survives on a reused branch. It must
/// not name the author of PR #15.
#[test]
fn stale_assignment_for_prior_pr_number_cannot_name_new_pr_author() {
    let home = home_with_fleet("stale-assignment-review", TWO_TEAM_FLEET);
    let branch = "feat/reused-branch-review";
    let record = crate::daemon::assignment_authority::ActiveAssignment::new_pending_typed(
        REPO,
        branch,
        "third-rev",
        crate::types::InstanceId::new(),
        14,
        "b".repeat(40),
        crate::review_receipt::ReviewSlot::Primary,
        "third",
        "t-pr-14-review",
        ReviewClass::Single,
        ReviewAuthor::Agent("personal".into()),
        "review",
        None,
        None,
        "2026-10-25T00:00:00Z",
    );
    crate::daemon::assignment_authority::persist(&home, &record).unwrap();
    save(&home, &merged_state(branch, &[], "")).unwrap();
    scan(&home, vec![]);
    assert!(
        kinds(&home, "personal").is_empty(),
        "an assignment for PR #14 must not name the author of PR #15 on the reused branch"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// r4 P1-3b: a Done task of PR #14 on a reused branch must not name the author
/// of PR #15.
#[test]
fn done_task_for_prior_pr_cannot_name_new_pr_author_on_reused_branch() {
    use crate::task_events::{DoneSource, InstanceName, TaskEvent, TaskId};

    let home = home_with_fleet("stale-done-task-review", TWO_TEAM_FLEET);
    let branch = "feat/reused-task-branch-review";
    task_on_branch(&home, "t-pr-14-author", "personal", branch);
    crate::task_events::append(
        &home,
        &InstanceName::from("test:seed"),
        TaskEvent::Done {
            task_id: TaskId("t-pr-14-author".into()),
            by: InstanceName::from("personal"),
            source: DoneSource::OperatorManual {
                authored_at: "2026-10-25T00:00:00Z".into(),
                result: Some("PR #14 complete".into()),
            },
        },
    )
    .unwrap();
    save(&home, &merged_state(branch, &[], "")).unwrap();
    scan(&home, vec![]);
    assert!(
        kinds(&home, "personal").is_empty(),
        "a completed task from PR #14 must not name PR #15's author"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Control (passes on r3): legacy JSON without provenance deserializes.
#[test]
fn r4_control_legacy_json_without_author_source_deserializes() {
    let state = new_for_branch(REPO, "feat/legacy-json", "sha", ReviewClass::Single);
    let mut json = serde_json::to_value(&state).unwrap();
    json.as_object_mut().unwrap().remove("pr_author_source");
    let loaded: PrState = serde_json::from_value(json).unwrap();
    assert_eq!(loaded.pr_author_source, None);
}

/// Control (passes on r3): a legacy stored lead is repaired from the typed
/// assignment by the real poller and the provenance is written.
#[test]
fn r4_control_legacy_lead_is_repaired_from_assignment_by_real_poller() {
    let home = home_with_fleet("legacy-lead-replay", TWO_TEAM_FLEET);
    let branch = "feat/legacy-lead-replay";
    assign_review(
        &home,
        branch,
        "third-rev",
        ReviewAuthor::Agent("personal".into()),
    );
    let mut state = new_for_branch(REPO, branch, &"a".repeat(40), ReviewClass::Single);
    state.pr_number = 15;
    state.pr_author = "third".into();
    save(&home, &state).unwrap();
    scan(&home, vec![open_meta(branch)]);
    let loaded = load(&home, REPO, branch).unwrap();
    assert_eq!(loaded.pr_author, "personal");
    assert_eq!(
        loaded.pr_author_source,
        Some(super::author_authority::AuthorSource::ReviewAssignment)
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Control (passes on r3): the typed assignment precedes a different live
/// binding; the bound non-author gets nothing.
#[test]
fn r4_control_assignment_precedes_different_live_binding() {
    let home = home_with_fleet("cross-tier-order", TWO_TEAM_FLEET);
    let branch = "feat/cross-tier-order";
    assign_review(
        &home,
        branch,
        "third-rev",
        ReviewAuthor::Agent("personal".into()),
    );
    crate::binding::bind(&home, "third", "t-transfer", branch);
    save(&home, &merged_state(branch, &["personal", "third"], "")).unwrap();
    scan(&home, vec![]);
    assert_eq!(kinds(&home, "personal"), vec!["pr-merged"]);
    assert!(kinds(&home, "third").is_empty());
    let _ = std::fs::remove_dir_all(&home);
}

// ---------------------------------------------------------------------------
// -18577-7 r5 — third-hq-1 REJECTED r4 (729ed932, receipt
// review-receipt:m-20261025021625191313-174): a stored author with
// provenance was trusted BEFORE the live records were consulted, so a current
// higher-tier assignment (or an unreadable one) was bypassed. Attack tests
// taken verbatim from scratch-161/pr15-r4-adversarial-tests.patch; the two
// `r5_control_*` tests are the reviewer's attacks that r4 already blocks.
// ---------------------------------------------------------------------------

#[test]
fn attack_current_typed_assignment_overrides_stored_binding_author() {
    let home = home_with_fleet("typed-vs-stored-binding-review", TWO_TEAM_FLEET);
    let branch = "feat/typed-vs-stored-binding-review";
    assign_review(
        &home,
        branch,
        "third-rev",
        ReviewAuthor::Agent("third".into()),
    );
    let mut state = merged_state(branch, &["personal", "third"], "personal");
    state.pr_author_source = Some(super::author_authority::AuthorSource::Binding);
    state.pr_author_pr_number = state.pr_number;
    save(&home, &state).unwrap();
    scan(&home, vec![]);
    let personal = kinds(&home, "personal");
    let third = kinds(&home, "third");
    assert!(
        personal.is_empty(),
        "stored binding author got {personal:?}; current assignment author got {third:?}"
    );
    assert_eq!(
        third,
        vec!["pr-merged"],
        "current typed assignment is higher priority than stored binding provenance"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn attack_corrupt_assignment_cannot_reuse_stored_binding_author() {
    let home = home_with_fleet("corrupt-vs-stored-binding-review", TWO_TEAM_FLEET);
    let branch = "feat/corrupt-vs-stored-binding-review";
    assign_review(
        &home,
        branch,
        "third-rev",
        ReviewAuthor::Agent("third".into()),
    );
    let path =
        crate::daemon::assignment_authority::record_path_for_test(&home, REPO, branch, "third-rev");
    std::fs::write(path, b"{").unwrap();
    let mut state = merged_state(branch, &["personal", "third"], "personal");
    state.pr_author_source = Some(super::author_authority::AuthorSource::Binding);
    state.pr_author_pr_number = state.pr_number;
    save(&home, &state).unwrap();
    scan(&home, vec![]);
    assert!(
        kinds(&home, "personal").is_empty(),
        "unreadable authoritative assignment cannot be bypassed by old binding provenance"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn r5_control_one_good_one_corrupt_assignment_fails_closed() {
    let home = home_with_fleet("mixed-corrupt-assignment-review", TWO_TEAM_FLEET);
    let branch = "feat/mixed-corrupt-assignment-review";
    assign_review(
        &home,
        branch,
        "third-rev",
        ReviewAuthor::Agent("personal".into()),
    );
    assign_review(
        &home,
        branch,
        "other-rev",
        ReviewAuthor::Agent("personal".into()),
    );
    let path =
        crate::daemon::assignment_authority::record_path_for_test(&home, REPO, branch, "other-rev");
    std::fs::write(path, b"{").unwrap();
    crate::binding::bind(&home, "third", "t-new-binding", branch);
    save(&home, &merged_state(branch, &["personal", "third"], "")).unwrap();
    scan(&home, vec![]);
    assert!(kinds(&home, "personal").is_empty());
    assert!(kinds(&home, "third").is_empty());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn r5_control_force_push_same_pr_number_keeps_assignment_author() {
    let home = home_with_fleet("same-pr-force-push-review", TWO_TEAM_FLEET);
    let branch = "feat/same-pr-force-push-review";
    assign_review(
        &home,
        branch,
        "third-rev",
        ReviewAuthor::Agent("personal".into()),
    );
    let mut state = merged_state(branch, &["personal", "third"], "");
    state.head_sha = "c".repeat(40); // the assignment still records head a*40
    save(&home, &state).unwrap();
    scan(&home, vec![]);
    assert_eq!(kinds(&home, "personal"), vec!["pr-merged"]);
    assert!(kinds(&home, "third").is_empty());
    let _ = std::fs::remove_dir_all(&home);
}

/// Control: a stored author from a HIGHER tier than the live evidence is kept
/// (the assignment retired at verdict; a later bound agent does not take over).
#[test]
fn r5_control_stored_assignment_author_outranks_a_later_binding() {
    let home = home_with_fleet("stored-assignment-vs-binding", TWO_TEAM_FLEET);
    let branch = "feat/stored-assignment-vs-binding";
    crate::binding::bind(&home, "third", "t-later-binding", branch);
    let mut state = merged_state(branch, &["personal", "third"], "personal");
    state.pr_author_source = Some(super::author_authority::AuthorSource::ReviewAssignment);
    state.pr_author_pr_number = state.pr_number;
    save(&home, &state).unwrap();
    scan(&home, vec![]);
    assert_eq!(kinds(&home, "personal"), vec!["pr-merged"]);
    assert!(kinds(&home, "third").is_empty());
    let _ = std::fs::remove_dir_all(&home);
}

/// Reviewer attack: when a branch is reused by PR #15, provenance saved for
/// PR #14 must not be carried forward as an author for the new generation.
#[test]
fn attack_stored_assignment_author_cleared_when_pr_number_changes() {
    let home = home_with_fleet("stored-generation-drift", TWO_TEAM_FLEET);
    let branch = "feat/stored-generation-drift";
    let mut state = new_for_branch(REPO, branch, &"a".repeat(40), ReviewClass::Single);
    state.pr_number = 14;
    state.pr_author = "personal".into();
    state.pr_author_source = Some(super::author_authority::AuthorSource::ReviewAssignment);
    let mut old_meta = open_meta(branch);
    old_meta.number = 14;
    state.last_gh_state = Some(old_meta);
    save(&home, &state).unwrap();
    let mut new_meta = open_meta(branch);
    new_meta.head_ref_oid = Some("c".repeat(40));
    scan(&home, vec![new_meta]); // new PR #15, same branch and a different head
    let updated = load(&home, REPO, branch).unwrap();
    assert_eq!(
        updated.pr_number, 15,
        "the poll must observe the new PR generation"
    );
    assert_eq!(updated.head_sha, "c".repeat(40));
    assert!(
        updated.pr_author.is_empty() || updated.pr_author_source.is_none(),
        "PR #14's stored assignment author survived on PR #15: {:?}",
        updated.pr_author_source
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Reviewer attack: if the prior generation's stored author survives, a
/// current PR binding cannot identify the new author because rank 0 wins.
#[test]
fn attack_prior_pr_stored_assignment_routes_current_merged_notice() {
    let home = home_with_fleet("stale-stored-generation-notice", TWO_TEAM_FLEET);
    let branch = "feat/stale-stored-generation-notice";
    crate::binding::bind(&home, "third", "t-current-pr", branch);
    let mut state = merged_state(branch, &["personal", "third"], "personal");
    state.pr_author_source = Some(super::author_authority::AuthorSource::ReviewAssignment); // saved for PR #14
    save(&home, &state).unwrap();
    scan(&home, vec![]);
    let personal = kinds(&home, "personal");
    let third = kinds(&home, "third");
    assert!(
        personal.is_empty(),
        "prior PR author got {personal:?}; current bound author got {third:?}"
    );
    assert_eq!(third, vec!["pr-merged"]);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn attack_same_tier_binding_conflict_over_stored_binding_fails_closed() {
    let home = home_with_fleet("stored-binding-conflict", TWO_TEAM_FLEET);
    let branch = "feat/stored-binding-conflict";
    crate::binding::bind(&home, "personal", "t-prior-binding", branch);
    crate::binding::bind(&home, "third", "t-current-binding", branch);
    let mut state = merged_state(branch, &["personal", "third"], "personal");
    state.pr_author_source = Some(super::author_authority::AuthorSource::Binding);
    state.pr_author_pr_number = state.pr_number;
    save(&home, &state).unwrap();
    scan(&home, vec![]);
    assert!(kinds(&home, "personal").is_empty());
    assert!(kinds(&home, "third").is_empty());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn attack_stale_stored_author_misroutes_review_dispatch_authority() {
    let home = home_with_fleet("stale-merge-authority", TWO_TEAM_FLEET);
    let branch = "feat/stale-merge-authority";
    crate::binding::bind(&home, "third", "t-current-pr", branch);
    let mut state = new_for_branch(REPO, branch, &"a".repeat(40), ReviewClass::Single);
    state.pr_number = 15;
    state.pr_author = "personal".into();
    state.pr_author_source = Some(super::author_authority::AuthorSource::ReviewAssignment); // PR #14
    state.last_gh_state = Some(open_meta(branch));
    save(&home, &state).unwrap();
    scan(&home, vec![]);
    let personal = kinds(&home, "personal");
    assert!(
        personal.is_empty(),
        "current PR diagnostics routed to prior PR's team: {personal:?}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn attack_same_tier_assignment_conflict_over_stored_author_fails_closed() {
    let home = home_with_fleet("stored-assignment-conflict", TWO_TEAM_FLEET);
    let branch = "feat/stored-assignment-conflict";
    assign_review(
        &home,
        branch,
        "third-rev",
        ReviewAuthor::Agent("personal".into()),
    );
    assign_review(
        &home,
        branch,
        "other-rev",
        ReviewAuthor::Agent("third".into()),
    );
    let mut state = merged_state(branch, &["personal", "third"], "personal");
    state.pr_author_source = Some(super::author_authority::AuthorSource::ReviewAssignment);
    state.pr_author_pr_number = state.pr_number;
    save(&home, &state).unwrap();
    scan(&home, vec![]);
    assert!(kinds(&home, "personal").is_empty());
    assert!(kinds(&home, "third").is_empty());
    let _ = std::fs::remove_dir_all(&home);
}

/// A reviewer exclusion retired on PR #14 cannot exclude the same agent when
/// that agent authors PR #15 on the reused branch.
#[test]
fn attack_prior_pr_retired_reviewer_cannot_exclude_current_author() {
    // r7: rewritten. The pre-r6 ledger alone carries no PR number, so on its
    // own it now excludes (fail closed, see
    // reviewer_attack_legacy_terminal_ledger_cannot_route_notice_to_same_pr_reviewer).
    // Once the generation-tagged ledger exists it is the only source: a
    // PR #14 row there, plus the same agent in the old ledger, does not
    // exclude PR #15's author.
    let home = home_with_fleet("retired-reviewer-generation", TWO_TEAM_FLEET);
    let branch = "feat/retired-reviewer-generation";
    retired_review(&home, branch, "third", 14);
    let record_path = crate::daemon::assignment_authority::record_path_for_test(
        &home,
        REPO,
        branch,
        "old-reviewer",
    );
    let dir = record_path.parent().unwrap();
    std::fs::write(
        dir.join("terminal_retirements.json"),
        br#"[{"board":"old","task_id":"t-pr14","instance":"third","seq":1}]"#,
    )
    .unwrap();
    crate::binding::bind(&home, "third", "t-pr15", branch);
    save(&home, &merged_state(branch, &[], "")).unwrap();
    scan(&home, vec![]);
    assert_eq!(kinds(&home, "third"), vec!["pr-merged"]);
    let _ = std::fs::remove_dir_all(&home);
}

// ---------------------------------------------------------------------------
// -18577-7 r6 controls (d-20261025030819687590-11): each pairs with an
// existing test so that "this PR" and "an earlier PR on the same branch" are
// told apart. `r5_control_stored_assignment_author_outranks_a_later_binding`
// now records PR #15 as the attribution's generation; the cross-PR twin below
// records PR #14 and must not keep the stored author.
// ---------------------------------------------------------------------------

#[test]
fn r6_control_stored_assignment_author_from_an_earlier_pr_yields_to_the_binding() {
    let home = home_with_fleet("stored-assignment-earlier-pr", TWO_TEAM_FLEET);
    let branch = "feat/stored-assignment-earlier-pr";
    crate::binding::bind(&home, "third", "t-current-pr", branch);
    let mut state = merged_state(branch, &["personal", "third"], "personal");
    state.pr_author_source = Some(super::author_authority::AuthorSource::ReviewAssignment);
    state.pr_author_pr_number = 14; // attributed for the previous PR
    save(&home, &state).unwrap();
    scan(&home, vec![]);
    assert!(kinds(&home, "personal").is_empty());
    assert_eq!(kinds(&home, "third"), vec!["pr-merged"]);
    let _ = std::fs::remove_dir_all(&home);
}

fn retired_review(home: &std::path::Path, branch: &str, target: &str, pr_number: u64) {
    let record = crate::daemon::assignment_authority::ActiveAssignment::new_pending_typed(
        REPO,
        branch,
        target,
        crate::types::InstanceId::new(),
        pr_number,
        "a".repeat(40),
        crate::review_receipt::ReviewSlot::Primary,
        "third",
        format!("t-review-{pr_number}"),
        ReviewClass::Single,
        ReviewAuthor::Agent("someone".into()),
        "review",
        None,
        None,
        "2026-10-25T00:00:00Z",
    );
    let dir = crate::daemon::assignment_authority::record_path_for_test(home, REPO, branch, target);
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    crate::daemon::assignment_authority::note_retired_review_target(home, &record).unwrap();
}

/// The reviewer retired on THIS PR is still excluded from authorship.
#[test]
fn r6_control_reviewer_retired_on_this_pr_is_still_not_the_author() {
    let home = home_with_fleet("retired-reviewer-this-pr", TWO_TEAM_FLEET);
    let branch = "feat/retired-reviewer-this-pr";
    retired_review(&home, branch, "third", 15);
    crate::binding::bind(&home, "third", "t-review-worktree", branch);
    save(&home, &merged_state(branch, &[], "")).unwrap();
    scan(&home, vec![]);
    assert!(kinds(&home, "third").is_empty());
    let _ = std::fs::remove_dir_all(&home);
}

/// The same record for an EARLIER PR does not exclude the current author.
#[test]
fn r6_control_reviewer_retired_on_an_earlier_pr_can_author_this_one() {
    let home = home_with_fleet("retired-reviewer-earlier-pr", TWO_TEAM_FLEET);
    let branch = "feat/retired-reviewer-earlier-pr";
    retired_review(&home, branch, "third", 14);
    crate::binding::bind(&home, "third", "t-pr15", branch);
    save(&home, &merged_state(branch, &[], "")).unwrap();
    scan(&home, vec![]);
    assert_eq!(kinds(&home, "third"), vec!["pr-merged"]);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn reviewer_attack_corrupt_retired_targets_cannot_route_notice_to_reviewer() {
    let home = home_with_fleet("corrupt-retired-targets", TWO_TEAM_FLEET);
    let branch = "feat/corrupt-retired-targets";
    retired_review(&home, branch, "third", 15);
    let dir =
        crate::daemon::assignment_authority::record_path_for_test(&home, REPO, branch, "third");
    std::fs::write(
        dir.parent().unwrap().join("retired_review_targets.json"),
        b"{",
    )
    .unwrap();
    crate::binding::bind(&home, "third", "t-review-worktree", branch);
    save(&home, &merged_state(branch, &[], "")).unwrap();
    scan(&home, vec![]);
    assert!(
        kinds(&home, "third").is_empty(),
        "corrupt reviewer exclusion routed an author notice to retired reviewer"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn reviewer_attack_legacy_terminal_ledger_cannot_route_notice_to_same_pr_reviewer() {
    let home = home_with_fleet("legacy-terminal-ledger", TWO_TEAM_FLEET);
    let branch = "feat/legacy-terminal-ledger";
    let dir =
        crate::daemon::assignment_authority::record_path_for_test(&home, REPO, branch, "third");
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::fs::write(
        dir.parent().unwrap().join("terminal_retirements.json"),
        br#"[{"board":"old","task_id":"t-pr15","instance":"third","seq":1}]"#,
    )
    .unwrap();
    crate::binding::bind(&home, "third", "t-review-worktree", branch);
    save(&home, &merged_state(branch, &[], "")).unwrap();
    scan(&home, vec![]);
    assert!(
        kinds(&home, "third").is_empty(),
        "legacy reviewer exclusion routed an author notice to retired reviewer"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// r7: a later retirement must not rewrite an unreadable exclusion ledger from
/// an empty set (that would erase the earlier exclusions).
#[test]
fn reviewer_attack_corrupt_retired_targets_are_not_erased_by_a_later_retirement() {
    let home = home_with_fleet("corrupt-retired-targets-erase", TWO_TEAM_FLEET);
    let branch = "feat/corrupt-retired-targets-erase";
    retired_review(&home, branch, "third", 15);
    let dir =
        crate::daemon::assignment_authority::record_path_for_test(&home, REPO, branch, "third");
    let ledger = dir.parent().unwrap().join("retired_review_targets.json");
    std::fs::write(&ledger, b"{").unwrap();
    retired_review(&home, branch, "other-rev", 15);
    assert_eq!(
        std::fs::read(&ledger).unwrap(),
        b"{".to_vec(),
        "an unreadable ledger was rewritten and its earlier exclusions erased"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// r7 seeding: the first r6+ retirement on a branch carries the pre-r6 rows
/// over with an unknown generation, so they keep excluding afterwards.
#[test]
fn r7_control_first_new_retirement_keeps_pre_r6_exclusions() {
    let home = home_with_fleet("legacy-seeded-into-new-ledger", TWO_TEAM_FLEET);
    let branch = "feat/legacy-seeded-into-new-ledger";
    let dir = crate::daemon::assignment_authority::record_path_for_test(&home, REPO, branch, "x");
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::fs::write(
        dir.parent().unwrap().join("terminal_retirements.json"),
        br#"[{"board":"old","task_id":"t-pr15","instance":"third","seq":1}]"#,
    )
    .unwrap();
    retired_review(&home, branch, "other-rev", 15);
    crate::binding::bind(&home, "third", "t-review-worktree", branch);
    save(&home, &merged_state(branch, &[], "")).unwrap();
    scan(&home, vec![]);
    assert!(kinds(&home, "third").is_empty());
    let _ = std::fs::remove_dir_all(&home);
}

/// r7 seeding: pre-r6 rows carried into the new ledger survive a SECOND new
/// retirement on the branch (the writer reads the new file, inserts, rewrites).
#[test]
fn r7_control_legacy_rows_survive_a_second_new_retirement() {
    let home = home_with_fleet("legacy-rows-second-retirement", TWO_TEAM_FLEET);
    let branch = "feat/legacy-rows-second-retirement";
    let dir = crate::daemon::assignment_authority::record_path_for_test(&home, REPO, branch, "x");
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::fs::write(
        dir.parent().unwrap().join("terminal_retirements.json"),
        br#"[{"board":"old","task_id":"t-pr15","instance":"third","seq":1}]"#,
    )
    .unwrap();
    retired_review(&home, branch, "other-rev", 15);
    retired_review(&home, branch, "third-rev", 16);
    crate::binding::bind(&home, "third", "t-review-worktree", branch);
    save(&home, &merged_state(branch, &[], "")).unwrap();
    scan(&home, vec![]);
    assert!(kinds(&home, "third").is_empty());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn reviewer_attack_unknown_legacy_row_cannot_demote_current_typed_author_to_binding() {
    let home = home_with_fleet("legacy-row-demotes-author", TWO_TEAM_FLEET);
    let branch = "feat/legacy-row-demotes-author";
    assign_review(
        &home,
        branch,
        "third-rev",
        ReviewAuthor::Agent("personal".into()),
    );
    let dir =
        crate::daemon::assignment_authority::record_path_for_test(&home, REPO, branch, "third-rev");
    std::fs::write(
        dir.parent().unwrap().join("terminal_retirements.json"),
        br#"[{"board":"old","task_id":"t-pr14","instance":"personal","seq":1}]"#,
    )
    .unwrap();
    crate::binding::bind(&home, "third", "t-current-binding", branch);
    save(&home, &merged_state(branch, &["personal", "third"], "")).unwrap();
    scan(&home, vec![]);
    let third = kinds(&home, "third");
    let personal = kinds(&home, "personal");
    assert!(third.is_empty(),
        "unknown-generation legacy row demoted the current typed author: personal={personal:?}, third={third:?}");
    let _ = std::fs::remove_dir_all(&home);
}

/// r8: the same demotion through the gh-login tier. `personal` and `third`
/// share the login inside ONE team (one domain); `personal` ranks first as the
/// orchestrator. An unknown-generation row naming `personal` must not hand
/// the author to `third` by dropping `personal` from the login hits.
#[test]
fn unknown_legacy_row_cannot_demote_the_login_author_to_another_member() {
    let home = home_with_fleet(
        "legacy-row-demotes-login",
        "instances:\n  \
         personal:\n    backend: claude\n    role_kind: orchestrator\n    github_login: shared-bot\n  \
         third:\n    backend: claude\n    github_login: shared-bot\n\
         teams:\n  personal-hq:\n    orchestrator: personal\n    members: [personal, third]\n    source_repo: acme/tool\n",
    );
    let branch = "feat/legacy-row-demotes-login";
    let dir = crate::daemon::assignment_authority::record_path_for_test(&home, REPO, branch, "x");
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::fs::write(
        dir.parent().unwrap().join("terminal_retirements.json"),
        br#"[{"board":"old","task_id":"t-pr14","instance":"personal","seq":1}]"#,
    )
    .unwrap();
    let mut state = new_for_branch(REPO, branch, &"a".repeat(40), ReviewClass::Single);
    state.pr_number = 15;
    save(&home, &state).unwrap();
    scan(&home, vec![open_meta(branch)]);
    let loaded = load(&home, REPO, branch).unwrap();
    assert_ne!(
        loaded.pr_author, "third",
        "an unknown-generation row demoted the login author to another member"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// r8 (third's request): an evidence-based reviewer exclusion inside a tier
/// fails the chain closed even when another candidate of the SAME tier
/// remains — the excluded one may be the real author.
#[test]
fn reviewer_exclusion_inside_a_tier_does_not_hand_the_notice_to_a_peer() {
    let home = home_with_fleet("legacy-row-same-tier-peer", TWO_TEAM_FLEET);
    let branch = "feat/legacy-row-same-tier-peer";
    let dir = crate::daemon::assignment_authority::record_path_for_test(&home, REPO, branch, "x");
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::fs::write(
        dir.parent().unwrap().join("terminal_retirements.json"),
        br#"[{"board":"old","task_id":"t-pr14","instance":"personal","seq":1}]"#,
    )
    .unwrap();
    crate::binding::bind(&home, "personal", "t-author", branch);
    crate::binding::bind(&home, "third", "t-peer", branch);
    save(&home, &merged_state(branch, &["personal", "third"], "")).unwrap();
    scan(&home, vec![]);
    assert!(kinds(&home, "third").is_empty());
    assert!(kinds(&home, "personal").is_empty());
    let _ = std::fs::remove_dir_all(&home);
}

/// r8: every tier — an evidence-excluded candidate at the binding tier does
/// not let the task tier answer.
#[test]
fn reviewer_exclusion_at_binding_tier_does_not_fall_to_the_task_tier() {
    let home = home_with_fleet("excluded-binding-to-task", TWO_TEAM_FLEET);
    let branch = "feat/excluded-binding-to-task";
    retired_review(&home, branch, "personal", 15);
    crate::binding::bind(&home, "personal", "t-review-worktree", branch);
    task_on_branch(&home, "t-third-task", "third", branch);
    save(&home, &merged_state(branch, &["personal", "third"], "")).unwrap();
    scan(&home, vec![]);
    assert!(kinds(&home, "third").is_empty());
    assert!(kinds(&home, "personal").is_empty());
    let _ = std::fs::remove_dir_all(&home);
}
