//! -18577-7 r3 — WHO is the PR author (d-20261025004632404221-3, option A).
//!
//! The fleet shares GitHub logins, so the gh `author_login` alone cannot name
//! the agent that opened a PR, and the CI subscribers only say who wants to
//! hear about it. The author is identified from the daemon's own
//! AUTHORITATIVE records, in this order:
//!
//! 1. the typed review assignment's `review_author = Agent(name)` — set by the
//!    orchestrator at dispatch, immutable;
//! 2. the live binding on the branch (the agent working in its worktree);
//! 3. the assignee of a task carrying the branch.
//!
//! Within a tier exactly one eligible non-reviewer must remain; several →
//! the tier is in conflict and the chain stops (fail closed — a weaker tier
//! must not "break the tie"); none → the next tier. Reviewers never author:
//! the assignment targets, the terminal-retirement instances and every
//! `role_kind: reviewer` instance are excluded from every tier.
//!
//! Only when no record exists does the gh login decide, and only if it maps
//! to exactly one team (see [`super::gh_poll::resolve_author_outcome`]).
//!
//! The resolved name is persisted on the PR state together with its
//! [`AuthorSource`]; a stored author WITHOUT a source (written by the pre-r3
//! resolvers: alphabetical pick, subscriber guess, raw login) is never used
//! for routing. Records are transient — the assignment is retired when the
//! verdict lands, the binding when the worktree is released — so the stored
//! provenance is what carries the author to the merged/closed notice.
//!
//! All reads here are filesystem / catalog reads done OUTSIDE the pr-state
//! flock; [`AuthorEvidence`] is handed into the locked section.

use std::path::Path;

use super::gh_poll::AuthorIndex;
use super::PrState;

/// Where a persisted `pr_author` came from. Absent on legacy state ⇒ untrusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuthorSource {
    ReviewAssignment,
    Binding,
    TaskAssignee,
    /// `github_login` mapped to exactly one eligible team.
    GhLogin,
}

/// The per-branch records that can identify the author, gathered outside
/// the pr-state flock.
#[derive(Debug, Default, Clone)]
pub(crate) struct AuthorEvidence {
    repo: String,
    branch: String,
    /// `review_author = Agent(..)` of the active typed assignments, with the
    /// PR generation (`pr_number`) each one was dispatched for.
    assignment_authors: Vec<(String, u64)>,
    /// r4: an assignment record exists but cannot be read. A higher tier that
    /// cannot be read is not absent: the chain fails closed.
    assignments_unreadable: bool,
    /// Assignment targets (active + terminal-retired) with the PR number they
    /// review: reviewers of THAT generation only (r6 P2).
    reviewer_targets: Vec<(String, u64)>,
    /// Agents bound to the branch (repo-matched first, legacy branch-only after).
    bound_agents: Vec<String>,
    /// Owners of live tasks that carry the branch (any board; done, cancelled
    /// and superseded tasks belong to an earlier PR generation).
    task_assignees: Vec<String>,
}

/// Outcome of the authoritative tiers, before logging.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Authoritative {
    Resolved(String, AuthorSource),
    /// A tier named more than one author.
    Conflict(AuthorSource, Vec<String>),
    /// No tier named anyone.
    None,
    /// r4: the assignment records could not be read.
    Unreadable,
}

impl AuthorEvidence {
    /// Gather the records for `repo`/`branch`. Assignments are read with the
    /// corruption-aware reader (r4 P1-1). May run `git remote get-url` for a
    /// binding's local `source_repo` — never call under the pr-state flock
    /// (#1617).
    pub(crate) fn gather(home: &Path, repo: &str, branch: &str) -> Self {
        let (assignments, assignments_unreadable) =
            match crate::daemon::assignment_authority::list_active_checked(home, repo, branch) {
                Ok(a) => (a, false),
                Err(e) => {
                    tracing::debug!(%repo, %branch, error = %e, "author evidence: unreadable assignment");
                    (Vec::new(), true)
                }
            };
        let mut assignment_authors: Vec<(String, u64)> = Vec::new();
        let mut reviewer_targets = Vec::new();
        for a in &assignments {
            if !reviewer_targets.contains(&(a.target.clone(), a.pr_number)) {
                reviewer_targets.push((a.target.clone(), a.pr_number));
            }
            if let crate::mcp::handlers::comms_gates::ReviewAuthor::Agent(name) = &a.review_author {
                if !name.is_empty()
                    && !assignment_authors
                        .iter()
                        .any(|(n, p)| n == name && *p == a.pr_number)
                {
                    assignment_authors.push((name.clone(), a.pr_number));
                }
            }
        }
        for retired in
            crate::daemon::assignment_authority::retired_review_targets(home, repo, branch)
        {
            if !reviewer_targets.contains(&retired) {
                reviewer_targets.push(retired);
            }
        }
        let bound_agents = bound_agents_on_branch(home, repo, branch);
        let task_assignees = task_assignees_on_branch(home, branch);
        Self {
            repo: repo.to_string(),
            branch: branch.to_string(),
            assignment_authors,
            assignments_unreadable,
            reviewer_targets,
            bound_agents,
            task_assignees,
        }
    }

    /// The evidence was gathered for this exact PR (the locked state must be
    /// the one the pre-lock snapshot described).
    pub(crate) fn is_for(&self, state: &PrState) -> bool {
        self.repo == state.repo && self.branch == state.branch
    }

    /// `name` reviews this PR (assignment target, retired or active) or is a
    /// reviewer instance: never the author.
    pub(crate) fn is_reviewer(&self, index: &AuthorIndex, state: &PrState, name: &str) -> bool {
        index.is_reviewer(name)
            || self.reviewer_targets.iter().any(|(t, pr)| {
                // A state without a PR number cannot tell generations apart:
                // keep excluding (the safe direction).
                t == name && (state.pr_number == 0 || *pr == state.pr_number)
            })
            || verdict_reviewers(state).any(|r| r == name)
    }

    /// Run the three authoritative tiers against the fleet eligibility `index`.
    pub(crate) fn resolve(&self, index: &AuthorIndex, state: &PrState) -> Authoritative {
        if self.assignments_unreadable {
            return Authoritative::Unreadable;
        }
        // r4 P1-3: only assignments dispatched for THIS PR generation. A state
        // without a PR number cannot prove the generation, so the tier is
        // skipped (assignments are persisted only with a nonzero pr_number).
        let current_assignment_authors: Vec<String> = self
            .assignment_authors
            .iter()
            .filter(|(_, pr)| state.pr_number != 0 && *pr == state.pr_number)
            .map(|(n, _)| n.clone())
            .collect();
        let tiers: [(AuthorSource, &[String]); 3] = [
            (AuthorSource::ReviewAssignment, &current_assignment_authors),
            (AuthorSource::Binding, &self.bound_agents),
            (AuthorSource::TaskAssignee, &self.task_assignees),
        ];
        for (source, names) in tiers {
            // A typed assignment is keyed by repo+branch+PR number and its
            // `review_author` is set by the owning team's orchestrator, so it
            // needs no fleet-eligibility gate. A binding and a task can belong
            // to an agent borrowed from another repo (r4 P1-2: a reviewer with
            // a `repo` override is invisible to the reviewer predicate), so
            // their names must be eligible for this repo.
            let require_eligible = !matches!(source, AuthorSource::ReviewAssignment);
            let mut candidates: Vec<String> = Vec::new();
            for n in names {
                if (!require_eligible || index.is_eligible(n)) && !self.is_reviewer(index, state, n)
                {
                    push_unique(&mut candidates, n);
                }
            }
            match candidates.len() {
                0 => continue,
                1 => return Authoritative::Resolved(candidates.remove(0), source),
                _ => return Authoritative::Conflict(source, candidates),
            }
        }
        Authoritative::None
    }
}

/// The poller must (re)resolve this state's author: nothing stored, a stored
/// value without provenance (pre-r3 resolvers), or one attributed for another
/// PR generation (r6).
pub(crate) fn needs_author(state: &PrState) -> bool {
    state.pr_author.is_empty() || state.pr_author_source.is_none() || !stored_for_current_pr(state)
}

/// r6 P1: the stored attribution was made for the PR this state now tracks.
fn stored_for_current_pr(state: &PrState) -> bool {
    state.pr_number != 0 && state.pr_author_pr_number == state.pr_number
}

/// The persisted `pr_author`, if it was written by an r3 resolver (has a
/// source) and is still an eligible non-reviewer. Legacy values (no source)
/// and reviewers are never trusted.
///
/// r5: the stored value only stands in for records that are GONE (an
/// assignment retired at verdict, a released binding). When `evidence` is
/// given, live evidence from the same or a higher tier than the stored
/// source — including an unreadable assignment — overrides it, and the
/// caller re-resolves from the live records.
pub(crate) fn trusted_stored_author(
    state: &PrState,
    index: &AuthorIndex,
    evidence: Option<&AuthorEvidence>,
) -> Option<String> {
    if state.pr_author.is_empty() || state.pr_author_source.is_none() {
        return None;
    }
    // r6 P1: attribution from an earlier PR on a reused branch is not this
    // PR's author (this check also covers the merge-authority candidates).
    if !stored_for_current_pr(state) {
        return None;
    }
    let name = state.pr_author.as_str();
    if !index.is_eligible(name) || index.is_reviewer(name) {
        return None;
    }
    // r4 P1-4 (d-20261025014213077350-7): a gh-login author holds only while
    // the login still maps to exactly one team.
    if state.pr_author_source == Some(AuthorSource::GhLogin)
        && !index.login_author_still_unique(name)
    {
        return None;
    }
    if evidence.is_some_and(|e| e.is_reviewer(index, state, name)) {
        return None;
    }
    if verdict_reviewers(state).any(|r| r == name) {
        return None;
    }
    if let (Some(e), Some(stored)) = (evidence, state.pr_author_source) {
        let live_rank = match e.resolve(index, state) {
            Authoritative::Resolved(_, source) | Authoritative::Conflict(source, _) => {
                Some(tier_rank(source))
            }
            Authoritative::Unreadable => Some(tier_rank(AuthorSource::ReviewAssignment)),
            Authoritative::None => None,
        };
        if live_rank.is_some_and(|live| live <= tier_rank(stored)) {
            return None;
        }
    }
    Some(state.pr_author.clone())
}

/// Priority of an author source; lower is stronger.
fn tier_rank(source: AuthorSource) -> u8 {
    match source {
        AuthorSource::ReviewAssignment => 0,
        AuthorSource::Binding => 1,
        AuthorSource::TaskAssignee => 2,
        AuthorSource::GhLogin => 3,
    }
}

/// Persist `name`/`source` as the PR author. Returns true when the state
/// changed (callers mark it dirty).
pub(crate) fn remember_author(state: &mut PrState, name: &str, source: AuthorSource) -> bool {
    if state.pr_author == name
        && state.pr_author_source == Some(source)
        && state.pr_author_pr_number == state.pr_number
    {
        return false;
    }
    state.pr_author = name.to_string();
    state.pr_author_source = Some(source);
    state.pr_author_pr_number = state.pr_number;
    true
}

/// r6 P1: the poller observed a different PR number on this branch (the
/// branch was reused). The stored attribution belongs to the old PR and
/// becomes display-only until it is re-resolved for the new one.
pub(crate) fn forget_author_for_new_pr(state: &mut PrState, observed_pr: u64) {
    if observed_pr != 0 && state.pr_number != 0 && observed_pr != state.pr_number {
        state.pr_author_source = None;
        state.pr_author_pr_number = 0;
    }
}

fn verdict_reviewers(state: &PrState) -> impl Iterator<Item = &str> {
    use super::VerdictState;
    let v: Vec<&str> = match &state.verdict_state {
        VerdictState::Verified { reviewers } => reviewers.iter().map(|(r, _)| r.as_str()).collect(),
        VerdictState::Rejected { reviewer, .. } | VerdictState::Unverified { reviewer, .. } => {
            vec![reviewer.as_str()]
        }
        VerdictState::None | VerdictState::Pending => Vec::new(),
    };
    v.into_iter()
}

fn push_unique(v: &mut Vec<String>, name: &str) {
    if !name.is_empty() && !v.iter().any(|n| n == name) {
        v.push(name.to_string());
    }
}

/// Agents bound to `branch`. A binding whose `source_repo` canonicalizes to
/// `repo` counts; a legacy binding (no `source_repo`) counts only when no
/// repo-matched binding exists, mirroring the verdict recipient rule it
/// replaces. A non-empty `source_repo` that cannot be resolved never counts.
pub(crate) fn bound_agents_on_branch(home: &Path, repo: &str, branch: &str) -> Vec<String> {
    if repo.is_empty() || branch.is_empty() {
        return Vec::new();
    }
    let Some(canonical_repo) =
        crate::mcp::handlers::dispatch_hook::canonical_repo_slug_for_source(Path::new(repo))
    else {
        return Vec::new();
    };
    let mut matched = Vec::new();
    let mut legacy = Vec::new();
    for (agent, binding) in crate::binding::binding_scan_all(home) {
        if binding["branch"].as_str() != Some(branch) {
            continue;
        }
        let source_repo = binding
            .get("source_repo")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        if source_repo.is_empty() {
            push_unique(&mut legacy, &agent);
            continue;
        }
        let slug = crate::mcp::handlers::dispatch_hook::canonical_repo_slug_for_source(Path::new(
            source_repo,
        ));
        if slug.as_deref() == Some(canonical_repo.as_str()) {
            push_unique(&mut matched, &agent);
        }
    }
    if matched.is_empty() {
        legacy
    } else {
        matched
    }
}

/// Owners of live tasks (every board) whose `branch` is this branch. Done,
/// cancelled and superseded tasks are excluded: on a reused branch they
/// belong to an earlier PR (r4 P1-3).
fn task_assignees_on_branch(home: &Path, branch: &str) -> Vec<String> {
    use crate::task_events::TaskStatus;
    let mut out = Vec::new();
    let Ok(tasks) = crate::task_events::catalog::for_home(home).all_tasks() else {
        return out;
    };
    for task in tasks {
        if task.branch.as_deref() != Some(branch)
            || matches!(
                task.status,
                TaskStatus::Done | TaskStatus::Cancelled | TaskStatus::Superseded
            )
        {
            continue;
        }
        if let Some(owner) = &task.owner {
            push_unique(&mut out, owner.as_str());
        }
    }
    out
}
