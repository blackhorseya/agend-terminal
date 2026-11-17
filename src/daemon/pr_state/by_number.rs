//! PR state lookups keyed by PR number / review task rather than by head branch.
//!
//! `pr_state` files are keyed `(repo, head branch)`, but a reviewer's own
//! review branch carries neither — the only daemon-held link from it to a PR is
//! the PR number recorded on the assignment / receipt / state itself.

use super::{gh_poll::GhPrState, is_pr_state_file, pr_state_dir, MergeState, PrState};
use std::path::Path;

/// Every readable PR state. An unreadable file is skipped: it can neither
/// confirm nor refute anything here, and every caller treats absence as
/// "unknown", never as evidence.
fn all_states(home: &Path) -> Vec<PrState> {
    let Ok(entries) = std::fs::read_dir(pr_state_dir(home)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_pr_state_file(path))
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .filter_map(|body| serde_json::from_str::<PrState>(&body).ok())
        .collect()
}

/// The PR state recorded for exactly `(repo, pr_number)`. `None` when none, or
/// when more than one file claims that identity (ambiguity authorizes nothing).
pub(crate) fn find_by_number(home: &Path, repo: &str, pr_number: u64) -> Option<PrState> {
    let mut hits = all_states(home)
        .into_iter()
        .filter(|state| state.repo == repo && state.pr_number == pr_number);
    let first = hits.next()?;
    hits.next().is_none().then_some(first)
}

/// Every distinct `(repo, pr_number)` for which a validated review receipt
/// names this `(task_id, reviewer)` pair. Receipts live in the PR state, so they
/// outlive the assignment that produced them.
pub(crate) fn review_prs_for_task(
    home: &Path,
    task_id: &str,
    reviewer: &str,
) -> Vec<(String, u64)> {
    let mut prs: Vec<(String, u64)> = all_states(home)
        .into_iter()
        .flat_map(|state| state.validated_review_receipts)
        .filter(|receipt| receipt.task_id == task_id && receipt.reviewer_name == reviewer)
        .map(|receipt| (receipt.repo, receipt.pr_number))
        .collect();
    prs.sort();
    prs.dedup();
    prs
}

/// The daemon has observed this PR merged (its own merge record, or the last
/// forge poll).
pub(crate) fn is_merged(state: &PrState) -> bool {
    matches!(state.merge_state, MergeState::Merged { .. })
        || state
            .last_gh_state
            .as_ref()
            .is_some_and(|gh| gh.state == GhPrState::Merged)
}

/// The last forge poll saw this PR open.
pub(crate) fn is_open(state: &PrState) -> bool {
    state
        .last_gh_state
        .as_ref()
        .is_some_and(|gh| gh.state == GhPrState::Open)
}
