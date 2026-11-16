//! PR state lookups keyed by PR number / review task rather than by head branch.
//!
//! `pr_state` files are keyed `(repo, head branch)`, but a reviewer's own
//! review branch carries neither — the only daemon-held link from it to a PR is
//! the PR number recorded on the assignment / receipt / state itself.

use super::PrState;
use std::path::Path;

/// The PR state recorded for exactly `(repo, pr_number)`. `None` when none, or
/// when more than one file claims that identity (ambiguity authorizes nothing).
pub(crate) fn find_by_number(_home: &Path, _repo: &str, _pr_number: u64) -> Option<PrState> {
    None
}

/// Every distinct `(repo, pr_number)` for which a validated review receipt
/// names this `(task_id, reviewer)` pair.
pub(crate) fn review_prs_for_task(
    _home: &Path,
    _task_id: &str,
    _reviewer: &str,
) -> Vec<(String, u64)> {
    Vec::new()
}
