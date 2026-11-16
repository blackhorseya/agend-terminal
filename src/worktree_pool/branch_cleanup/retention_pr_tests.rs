//! 審查分支的 retention 單要綁到它審查的 PR：PR 號只能來自 daemon 持久化的權威資料
//! （binding / active assignment / 已驗證 receipt），不得由分支名推測。

use super::retention_pr::{resolve_review_pr, retention_pr_tag, ReviewPr};
use super::{raise_retention_for_release, RETENTION_TAG};
use crate::daemon::pr_state::gh_poll::{GhPrMetadata, GhPrState};
use crate::daemon::pr_state::{self, MergeState, ReviewClass};
use serde_json::json;
use std::path::{Path, PathBuf};

const SLUG: &str = "acme/neutral-repo";
const OTHER_SLUG: &str = "other/unrelated-repo";
const REVIEWER: &str = "rev-1";
const TASK: &str = "t-neutral-review";
const HEAD_BRANCH: &str = "feat/neutral-change";
const REVIEW_BRANCH: &str = "review/neutral-r1";

fn tmp_home(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "agend-retention-pr-{}-{}-{}",
        std::process::id(),
        tag,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("home dir");
    dir
}

fn git_in(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A source repo whose `origin` resolves to [`SLUG`], with [`REVIEW_BRANCH`]
/// carrying one commit. Returns (repo path, branch tip).
fn source_repo(tag: &str) -> (PathBuf, String) {
    let repo = tmp_home(&format!("repo-{tag}"));
    git_in(&repo, &["init", "-q", "-b", "main"]);
    git_in(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            &format!("https://github.com/{SLUG}.git"),
        ],
    );
    std::fs::write(repo.join("f.txt"), "base").expect("write");
    git_in(&repo, &["add", "."]);
    git_in(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "base",
        ],
    );
    git_in(&repo, &["branch", REVIEW_BRANCH]);
    let tip = crate::git_helpers::git_cmd(&repo, &["rev-parse", REVIEW_BRANCH])
        .expect("rev-parse")
        .trim()
        .to_string();
    (repo, tip)
}

fn binding() -> serde_json::Value {
    json!({"agent": REVIEWER, "task_id": TASK, "branch": REVIEW_BRANCH})
}

fn gh_meta(pr: u64, state: GhPrState) -> GhPrMetadata {
    GhPrMetadata {
        number: pr,
        author_login: "someone".into(),
        head_ref: HEAD_BRANCH.into(),
        is_cross_repository: false,
        is_draft: false,
        state,
        merged_at: None,
        head_ref_oid: None,
        base_ref_oid: None,
    }
}

/// Plant a PR state file for `(repo, HEAD_BRANCH)` observed in `state`.
fn plant_pr(home: &Path, repo: &str, pr: u64, state: GhPrState) {
    let head = "a".repeat(40);
    let mut s = pr_state::new_for_branch(repo, HEAD_BRANCH, &head, ReviewClass::Single);
    s.pr_number = pr;
    if state == GhPrState::Merged {
        s.merge_state = MergeState::Merged {
            merge_commit: "b".repeat(40),
            merged_at: "2026-11-17T00:00:00Z".into(),
        };
    }
    s.last_gh_state = Some(gh_meta(pr, state));
    pr_state::save(home, &s).expect("save pr state");
}

/// Plant a PR state file carrying one validated receipt for `(TASK, REVIEWER)`.
fn plant_receipt(home: &Path, repo: &str, pr: u64) {
    let head = "a".repeat(40);
    let mut s = pr_state::new_for_branch(
        repo,
        &format!("{HEAD_BRANCH}-{pr}"),
        &head,
        ReviewClass::Single,
    );
    s.pr_number = pr;
    s.validated_review_receipts
        .push(crate::review_receipt::ReviewReceiptSummary {
            receipt_id: format!("review-receipt:{pr}"),
            source_id: format!("{pr}"),
            evidence_digest: "a".repeat(64),
            assignment_id: uuid::Uuid::new_v4(),
            reviewer_instance_id: crate::types::InstanceId::new(),
            reviewer_name: REVIEWER.into(),
            repo: repo.into(),
            pr_number: pr,
            branch: format!("{HEAD_BRANCH}-{pr}"),
            task_id: TASK.into(),
            reviewed_head: head,
            review_class: ReviewClass::Single,
            slot: crate::review_receipt::ReviewSlot::Primary,
            verdict: crate::review_receipt::ReviewVerdict::Verified,
        });
    pr_state::save(home, &s).expect("save receipt state");
}

fn plant_assignment(home: &Path, repo: &str, pr: u64) {
    let record = crate::daemon::assignment_authority::ActiveAssignment::new_pending_typed(
        repo,
        HEAD_BRANCH,
        REVIEWER,
        crate::types::InstanceId::new(),
        pr,
        "a".repeat(40),
        crate::review_receipt::ReviewSlot::Primary,
        "test:operator",
        TASK,
        ReviewClass::Single,
        crate::mcp::handlers::comms_gates::ReviewAuthor::External("external-author".into()),
        "neutral fixture assignment",
        None,
        None,
        "2026-11-17T00:00:00Z",
    );
    crate::daemon::assignment_authority::persist(home, &record).expect("persist assignment");
}

fn retention_rows(home: &Path, repo: &Path) -> Vec<crate::task_events::TaskRecord> {
    let project = crate::tasks::resolve_repository_project(home, repo);
    crate::task_events::projected_state_at(&crate::task_events::board_root(home, &project))
        .map(|state| {
            state
                .tasks
                .into_values()
                .filter(|t| t.tags.iter().any(|tag| tag == RETENTION_TAG))
                .collect()
        })
        .unwrap_or_default()
}

fn pr(repo: &str, n: u64) -> Option<ReviewPr> {
    Some(ReviewPr {
        repo: repo.into(),
        pr_number: n,
    })
}

// ── 來源 1：binding 內的 PR 識別 ──────────────────────────────────────────

#[test]
fn resolves_the_pr_from_the_daemon_provisioned_binding() {
    let home = tmp_home("src1");
    let mut b = binding();
    b["review_pr_number"] = json!(42);
    b["review_pr_repo"] = json!(SLUG);
    assert_eq!(resolve_review_pr(&home, &b, SLUG), pr(SLUG, 42));
}

#[test]
fn a_binding_pr_of_another_repository_is_not_adopted() {
    let home = tmp_home("src1-other");
    let mut b = binding();
    b["review_pr_number"] = json!(42);
    b["review_pr_repo"] = json!(OTHER_SLUG);
    assert_eq!(resolve_review_pr(&home, &b, SLUG), None);
}

// ── 來源 2：以 (agent, task_id) 查 active assignment ──────────────────────

#[test]
fn resolves_the_pr_from_the_active_assignment_of_agent_and_task() {
    let home = tmp_home("src2");
    plant_assignment(&home, SLUG, 43);
    assert_eq!(resolve_review_pr(&home, &binding(), SLUG), pr(SLUG, 43));
}

#[test]
fn an_assignment_of_another_repository_is_not_adopted() {
    let home = tmp_home("src2-other");
    plant_assignment(&home, OTHER_SLUG, 43);
    assert_eq!(resolve_review_pr(&home, &binding(), SLUG), None);
}

// ── 來源 3：assignment 已 retire 後仍在的已驗證 receipt ────────────────────

#[test]
fn resolves_the_pr_from_a_validated_receipt_after_the_assignment_is_gone() {
    let home = tmp_home("src3");
    plant_receipt(&home, SLUG, 44);
    assert_eq!(resolve_review_pr(&home, &binding(), SLUG), pr(SLUG, 44));
}

#[test]
fn a_receipt_of_another_repository_is_not_adopted() {
    let home = tmp_home("src3-other");
    plant_receipt(&home, OTHER_SLUG, 44);
    assert_eq!(resolve_review_pr(&home, &binding(), SLUG), None);
}

#[test]
fn receipts_naming_two_different_prs_are_ambiguous_and_adopt_nothing() {
    let home = tmp_home("src3-ambiguous");
    plant_receipt(&home, SLUG, 44);
    plant_receipt(&home, SLUG, 45);
    assert_eq!(resolve_review_pr(&home, &binding(), SLUG), None);
}

#[test]
fn a_binding_without_any_pr_evidence_resolves_to_nothing() {
    let home = tmp_home("none");
    assert_eq!(resolve_review_pr(&home, &binding(), SLUG), None);
}

// ── release 建單：open／merged 的 PR 不建單，其餘綁 tag ────────────────────

#[test]
fn an_open_pr_suppresses_the_obligation_like_a_head_branch_would() {
    let home = tmp_home("rel-open");
    let (repo, tip) = source_repo("open");
    plant_assignment(&home, SLUG, 50);
    plant_pr(&home, SLUG, 50, GhPrState::Open);
    raise_retention_for_release(
        &home,
        &repo.display().to_string(),
        REVIEW_BRANCH,
        &tip,
        TASK,
        &binding(),
    );
    assert!(
        retention_rows(&home, &repo).is_empty(),
        "an open PR can still settle by merge — no obligation"
    );
}

#[test]
fn an_already_merged_pr_suppresses_the_obligation() {
    let home = tmp_home("rel-merged");
    let (repo, tip) = source_repo("merged");
    plant_assignment(&home, SLUG, 51);
    plant_pr(&home, SLUG, 51, GhPrState::Merged);
    raise_retention_for_release(
        &home,
        &repo.display().to_string(),
        REVIEW_BRANCH,
        &tip,
        TASK,
        &binding(),
    );
    assert!(
        retention_rows(&home, &repo).is_empty(),
        "the merge already happened before release — nothing left to ask"
    );
}

#[test]
fn a_closed_unmerged_pr_raises_the_obligation_tagged_with_the_pr() {
    let home = tmp_home("rel-closed");
    let (repo, tip) = source_repo("closed");
    plant_assignment(&home, SLUG, 52);
    plant_pr(&home, SLUG, 52, GhPrState::Closed);
    raise_retention_for_release(
        &home,
        &repo.display().to_string(),
        REVIEW_BRANCH,
        &tip,
        TASK,
        &binding(),
    );
    let rows = retention_rows(&home, &repo);
    assert_eq!(rows.len(), 1, "pre: a closed PR cannot settle by merge");
    assert!(
        rows[0].tags.contains(&retention_pr_tag(SLUG, 52)),
        "obligation must carry the PR tag: {:?}",
        rows[0].tags
    );
}

#[test]
fn a_binding_with_no_pr_evidence_raises_exactly_as_before() {
    let home = tmp_home("rel-none");
    let (repo, tip) = source_repo("none");
    raise_retention_for_release(
        &home,
        &repo.display().to_string(),
        REVIEW_BRANCH,
        &tip,
        TASK,
        &binding(),
    );
    let rows = retention_rows(&home, &repo);
    assert_eq!(rows.len(), 1);
    assert!(
        !rows[0].tags.iter().any(|t| t.starts_with("retention-pr:")),
        "no PR known → no PR tag: {:?}",
        rows[0].tags
    );
}
