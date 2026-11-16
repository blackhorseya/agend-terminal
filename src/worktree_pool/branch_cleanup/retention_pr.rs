//! Tie a review branch's retention obligation to the PR it reviewed.
//!
//! A reviewer's review branch never equals the PR head branch, so neither the
//! release-time open-PR check nor the merge-time lane retirement ever sees it.
//! The link has to come from daemon-held PR identity, never from the branch name.

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
pub(super) fn resolve_review_pr(
    _home: &Path,
    _binding: &serde_json::Value,
    _local_slug: &str,
) -> Option<ReviewPr> {
    None
}

/// Raise the retention obligation for a released lane, tied to its PR when one
/// is known.
pub(in crate::worktree_pool) fn raise_retention_for_release(
    home: &Path,
    source_repo: &str,
    branch: &str,
    head: &str,
    task_id: &str,
    binding: &serde_json::Value,
) {
    super::record_retention_obligation(
        home,
        source_repo,
        branch,
        head,
        task_id,
        Some(binding["agent"].as_str().unwrap_or("")),
    );
}
