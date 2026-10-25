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
    for who in ["personal", "third", "third-rev"] {
        assert!(kinds(&home, who).is_empty(), "{who} must receive nothing");
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
