//! 審查分支的 retention 單在它審查的 PR 被 daemon 確認合併後退役；
//! PR 號來自 `retention-pr:` tag，不看分支名。

use super::*;
use crate::daemon::pr_state::gh_poll::{GhPrMetadata, GhPrState};
use crate::daemon::pr_state::{self, MergeState, ReviewClass};

const SLUG: &str = "acme/neutral-repo";
const OTHER_SLUG: &str = "other/unrelated-repo";
const REPO_PATH: &str = "/nonexistent/neutral-repo";
const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

fn tmp_home(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "agend-retention-pr-retire-{}-{}-{}",
        std::process::id(),
        tag,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("home dir");
    dir
}

fn plant_pr(home: &Path, repo: &str, pr: u64, state: GhPrState) {
    let mut s = pr_state::new_for_branch(
        repo,
        "feat/neutral-change",
        &"a".repeat(40),
        ReviewClass::Single,
    );
    s.pr_number = pr;
    if state == GhPrState::Merged {
        s.merge_state = MergeState::Merged {
            merge_commit: "b".repeat(40),
            merged_at: "2026-11-17T00:00:00Z".into(),
        };
    }
    s.last_gh_state = Some(GhPrMetadata {
        number: pr,
        author_login: "someone".into(),
        head_ref: "feat/neutral-change".into(),
        is_cross_repository: false,
        is_draft: false,
        state,
        merged_at: None,
        head_ref_oid: None,
        base_ref_oid: None,
    });
    pr_state::save(home, &s).expect("save");
}

/// Raise one review-branch obligation tagged for `(slug, pr)`; returns its row.
fn raise(home: &Path, branch: &str, tag: Option<String>) -> crate::task_events::TaskRecord {
    crate::worktree_pool::record_retention_obligation_with_pr(
        home,
        REPO_PATH,
        branch,
        HEAD,
        "",
        Some("rev-1"),
        tag,
    );
    let project = retention_project(home, REPO_PATH);
    retention_obligations(home, &project)
        .into_iter()
        .find(|t| {
            t.tags
                .contains(&crate::worktree_pool::retention_lane_key(REPO_PATH, branch))
        })
        .expect("obligation raised")
}

fn status_of(home: &Path, id: &crate::task_events::TaskId) -> crate::task_events::TaskStatus {
    let project = retention_project(home, REPO_PATH);
    retention_obligations(home, &project)
        .into_iter()
        .find(|t| &t.id == id)
        .expect("row")
        .status
}

fn retire(home: &Path) {
    owner_attestation::retire_merged_pr_obligations(home, &retention_project(home, REPO_PATH));
}

#[test]
fn a_merged_pr_retires_the_review_branch_obligation_tagged_with_it() {
    let home = tmp_home("merged");
    plant_pr(&home, SLUG, 60, GhPrState::Merged);
    let row = raise(
        &home,
        "review/neutral-r1",
        Some(crate::worktree_pool::retention_pr_tag(SLUG, 60)),
    );
    retire(&home);
    assert!(
        status_of(&home, &row.id).is_terminal(),
        "a merged PR retires its review branch's obligation"
    );
}

#[test]
fn a_closed_unmerged_pr_does_not_retire_it() {
    let home = tmp_home("closed");
    plant_pr(&home, SLUG, 61, GhPrState::Closed);
    let row = raise(
        &home,
        "review/neutral-r1",
        Some(crate::worktree_pool::retention_pr_tag(SLUG, 61)),
    );
    retire(&home);
    assert!(!status_of(&home, &row.id).is_terminal());
}

#[test]
fn another_prs_merge_does_not_retire_it() {
    let home = tmp_home("other-pr");
    plant_pr(&home, SLUG, 62, GhPrState::Merged);
    let row = raise(
        &home,
        "review/neutral-r1",
        Some(crate::worktree_pool::retention_pr_tag(SLUG, 63)),
    );
    retire(&home);
    assert!(!status_of(&home, &row.id).is_terminal());
}

#[test]
fn the_same_pr_number_in_another_repository_does_not_retire_it() {
    let home = tmp_home("other-repo");
    plant_pr(&home, OTHER_SLUG, 64, GhPrState::Merged);
    let row = raise(
        &home,
        "review/neutral-r1",
        Some(crate::worktree_pool::retention_pr_tag(SLUG, 64)),
    );
    retire(&home);
    assert!(!status_of(&home, &row.id).is_terminal());
}

#[test]
fn an_obligation_without_a_pr_tag_is_left_alone() {
    let home = tmp_home("untagged");
    plant_pr(&home, SLUG, 65, GhPrState::Merged);
    let row = raise(&home, "feat/neutral-local", None);
    retire(&home);
    assert!(!status_of(&home, &row.id).is_terminal());
}

/// 同一支 PR 會先後綁多個 review 分支，head 各是不同的中間 commit：
/// PR 合併後每一張都要退役，不能只退役 head 等於最終 head 的那張。
#[test]
fn every_review_branch_of_one_merged_pr_is_retired_whatever_its_head() {
    let home = tmp_home("many-branches");
    plant_pr(&home, SLUG, 66, GhPrState::Closed);
    let tag = crate::worktree_pool::retention_pr_tag(SLUG, 66);
    let rows: Vec<_> = [
        "review/neutral-r1",
        "review/neutral-red",
        "review/neutral-r2",
    ]
    .iter()
    .enumerate()
    .map(|(i, branch)| {
        let head = format!("{i:040x}");
        crate::worktree_pool::record_retention_obligation_with_pr(
            &home,
            REPO_PATH,
            branch,
            &head,
            "",
            Some("rev-1"),
            Some(tag.clone()),
        );
        let lane = crate::worktree_pool::retention_lane_key(REPO_PATH, branch);
        retention_obligations(&home, &retention_project(&home, REPO_PATH))
            .into_iter()
            .find(|t| t.tags.contains(&lane))
            .expect("raised")
    })
    .collect();
    assert_eq!(rows.len(), 3, "pre: one obligation per review branch");

    retire(&home);
    assert!(
        rows.iter().all(|r| !status_of(&home, &r.id).is_terminal()),
        "pre: nothing retires while the PR is not merged"
    );

    plant_pr(&home, SLUG, 66, GhPrState::Merged);
    retire(&home);
    for row in &rows {
        assert!(
            status_of(&home, &row.id).is_terminal(),
            "obligation {} must retire with the PR's merge",
            row.id.0
        );
    }
}
