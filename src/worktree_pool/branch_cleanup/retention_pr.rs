//! Tie a review branch's retention obligation to the PR it reviewed.
//!
//! A reviewer's review branch never equals the PR head branch, so neither the
//! release-time open-PR check nor the merge-time lane retirement ever sees it.
//! The link has to come from daemon-held PR identity, never from the branch name.

use crate::daemon::pr_state::by_number;
use std::path::Path;

/// The PR a review binding is for, with the repository slug it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReviewPr {
    pub repo: String,
    pub pr_number: u64,
}

pub(crate) fn retention_pr_tag(repo: &str, pr_number: u64) -> String {
    format!("retention-pr:{repo}#{pr_number}")
}

/// Which PR does this released binding belong to, per daemon-held identity.
///
/// In order: the PR the daemon recorded in the binding when it provisioned the
/// review; the reviewer's active assignment; a validated receipt (which
/// outlives the assignment once the verdict is in). Each source must name
/// `local_slug`'s own repository, and each must name exactly one PR — a second,
/// different PR makes the evidence ambiguous and adopts nothing.
pub(super) fn resolve_review_pr(
    home: &Path,
    binding: &serde_json::Value,
    local_slug: &str,
) -> Option<ReviewPr> {
    let ours = |repo: &str, pr_number: u64| {
        (repo == local_slug && pr_number > 0).then(|| ReviewPr {
            repo: repo.to_string(),
            pr_number,
        })
    };
    if let (Some(pr_number), Some(repo)) = (
        binding["review_pr_number"].as_u64(),
        binding["review_pr_repo"].as_str(),
    ) {
        return ours(repo, pr_number);
    }
    let agent = binding["agent"].as_str().filter(|s| !s.is_empty())?;
    let task_id = binding["task_id"].as_str().filter(|s| !s.is_empty())?;

    let mut assigned: Vec<u64> =
        crate::daemon::assignment_authority::list_active_by_target_task(home, agent, task_id)
            .into_iter()
            .filter(|record| record.repo == local_slug)
            .map(|record| record.pr_number)
            .collect();
    assigned.sort_unstable();
    assigned.dedup();
    match assigned.as_slice() {
        [] => {}
        [pr_number] => return ours(local_slug, *pr_number),
        _ => return None,
    }

    let mut received: Vec<u64> = by_number::review_prs_for_task(home, task_id, agent)
        .into_iter()
        .filter(|(repo, _)| repo == local_slug)
        .map(|(_, pr_number)| pr_number)
        .collect();
    received.dedup();
    match received.as_slice() {
        [pr_number] => ours(local_slug, *pr_number),
        _ => None,
    }
}

/// Raise the retention obligation for a released lane, tied to its PR when one
/// is known.
///
/// A PR still open can yet settle by merge, and one already merged has nothing
/// left to ask — both raise nothing, as a head branch's open PR already does.
/// A PR the daemon has no state for raises as before: unknown is not evidence.
pub(in crate::worktree_pool) fn raise_retention_for_release(
    home: &Path,
    source_repo: &str,
    branch: &str,
    head: &str,
    task_id: &str,
    binding: &serde_json::Value,
) {
    let owner = binding["agent"].as_str().unwrap_or("");
    let local_slug =
        crate::git_helpers::git_cmd(Path::new(source_repo), &["remote", "get-url", "origin"])
            .ok()
            .and_then(|url| crate::branch_sweep::extract_github_repo_for_intent(url.trim()));
    let review_pr = local_slug
        .as_deref()
        .and_then(|slug| resolve_review_pr(home, binding, slug));
    let mut pr_tag = None;
    if let Some(review_pr) = &review_pr {
        if let Some(state) = by_number::find_by_number(home, &review_pr.repo, review_pr.pr_number) {
            if by_number::is_open(&state) || by_number::is_merged(&state) {
                tracing::info!(
                    %branch, repo = %review_pr.repo, pr = review_pr.pr_number,
                    "branch-retention obligation skipped — the reviewed PR is open or already merged"
                );
                return;
            }
        }
        pr_tag = Some(retention_pr_tag(&review_pr.repo, review_pr.pr_number));
    }
    super::record_retention_obligation_with_pr(
        home,
        source_repo,
        branch,
        head,
        task_id,
        Some(owner),
        pr_tag,
    );
}
