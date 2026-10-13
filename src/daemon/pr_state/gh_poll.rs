//! #986: GitHub observation feeder for the [`super::PrState`] aggregator.
//!
//! Closes the (repo, branch) → (PR#, pr_author) resolution gap left by
//! #972: the aggregator's internal state machine works correctly but
//! cannot map a ci-watch observation to a GitHub PR identity without
//! talking to GitHub. This module is the IO surface that bridges them.
//!
//! ## Architecture
//!
//! - [`GhPoller`] trait — production [`CliGhPoller`] shells out to
//!   `gh pr list --json author,number,headRefName,isCrossRepository,isDraft,state,mergedAt,headRefOid,baseRefOid
//!   --state all`; tests inject [`MockGhPoller`] with canned responses
//!   (§3.20 SOP 1 — no subprocess invocation in unit tests).
//! - Single batched call per repo per scanner tick (NOT per-PR
//!   `gh pr view`). Worst case 360 calls/hr at default cadence; 1440/hr
//!   at armed cadence — both well under 5000/hr authenticated budget.
//! - Tiered polling cadence (dev-2 Pushback 4): 15s when
//!   `PrState.auto_armed=true` (active flow — minimize [pr-merged]
//!   latency); 60s otherwise.
//! - Exponential backoff on failures: `2^failures × tick` capped at
//!   300s. Cleared on first success.
//! - Slip observability (dev-2 BLOCKING #1): `tracing::warn!` when the
//!   gh CLI call elapsed > 1s — operator visibility into scanner
//!   thread blocking. PR body commitment: if empirical slip > 10% of
//!   tick budget, follow-up refactors to fire-and-forget worker.
//!
//! ## Author resolution (dev-2 Pushback 2 — INVERTED chain)
//!
//! Operator-explicit `github_login` field on fleet.yaml InstanceConfig
//! wins over heuristic name match. Final fallback is `fixup-lead` with
//! a tracing::warn observability hook (reviewer MANDATORY).
//!
//! ## Webhook future-compat
//!
//! Webhook subscription (deferred per #972 docstring) is a drop-in
//! `GhPoller` impl OR sibling trait — webhook payload feeds the same
//! event variants. Polling becomes catch-up backup for missed webhooks.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Single PR's observation from `gh pr list`. Mirrors GitHub's
/// graphql-derived fields needed to drive the PrState reducer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GhPrMetadata {
    pub number: u64,
    pub author_login: String,
    pub head_ref: String,
    /// #1750-B4: true when the head branch lives in a FORK (`isCrossRepository`),
    /// so its `head_ref` is NOT a base-repo branch — remote-orphan GC must skip it
    /// (a fork head_ref colliding with a base-repo branch name would otherwise
    /// delete the unrelated base-repo branch).
    #[serde(default)]
    pub is_cross_repository: bool,
    pub is_draft: bool,
    pub state: GhPrState,
    pub merged_at: Option<String>,
    /// #2749: the PR head + base tip OIDs from the SAME `gh pr view --json
    /// headRefOid,baseRefOid` response — the ATOMIC observation `apply_gh_poll`
    /// writes into `PrState::observed_head_sha`/`observed_base_sha` together
    /// (never composed across two reads). `#[serde(default)]` so pre-existing
    /// `last_gh_state` snapshots load with `None`.
    #[serde(default)]
    pub head_ref_oid: Option<String>,
    #[serde(default)]
    pub base_ref_oid: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GhPrState {
    Open,
    Merged,
    Closed,
}

/// IO surface for observing GitHub PR state. Production:
/// [`CliGhPoller`]; tests: [`MockGhPoller`].
pub trait GhPoller: Send + Sync {
    /// Batch-poll all PRs for `repo`. Returns `Err` on transport / CLI errors;
    /// `Ok((prs, polled_at))` on success (empty `prs` when no PRs match).
    ///
    /// #986: `polled_at` (RFC3339) is WHEN the data was actually fetched from
    /// GitHub. For the live pollers it is ~now; for [`SnapshotGhPoller`] it is the
    /// background worker's poll time, which may LAG — so the scanner can gate
    /// "positively no PR" on `polled_at >= pr_state.created_at` (a snapshot taken
    /// before a branch's PR existed must never assert "no PR" for that branch).
    fn poll(&self, repo: &str) -> anyhow::Result<(Vec<GhPrMetadata>, String)>;
}

/// Production poller — invokes `gh pr list --json ... --state all`.
pub struct CliGhPoller;

impl GhPoller for CliGhPoller {
    fn poll(&self, repo: &str) -> anyhow::Result<(Vec<GhPrMetadata>, String)> {
        // #PR-B: route the `gh pr list` shell-out through `ScmProvider`
        // instead of calling `Command::new("gh")` directly. The emitted
        // argv is byte-identical to the prior inline call (pinned by
        // `scm::tests::pr_list_args_match_site7_byte_identical`); the
        // `--json` field set is passed verbatim. Behavior is unchanged:
        // on any failure pr_list returns Err (same as the prior all-Err
        // contract), Ok(vec) on success.
        let start = std::time::Instant::now();
        let result = crate::scm::make_scm_provider(repo, None).pr_list(
            repo,
            &crate::scm::ListFilter {
                state: Some("all"),
                limit: Some(100),
                ..Default::default()
            },
            &[
                "author",
                "number",
                "headRefName",
                "isCrossRepository",
                "isDraft",
                "state",
                "mergedAt",
                // #2749: atomic head+base tip OIDs in the SAME response.
                "headRefOid",
                "baseRefOid",
            ],
            None, // #PR-D: uses --repo (no cwd), byte-identical to before
        );
        let elapsed = start.elapsed();
        // dev-2 BLOCKING #1: slip observability. >1s flags potential
        // scanner-thread blocking; >10% of tick budget triggers the
        // fire-and-forget refactor commitment in the PR body. Timing the
        // whole trait call preserves the original semantics (the prior
        // code warned after the gh process ran, regardless of exit).
        if elapsed > Duration::from_secs(1) {
            tracing::warn!(
                repo = %repo,
                elapsed_ms = elapsed.as_millis() as u64,
                "#986 gh-poll: scanner-thread slip > 1s — \
                 if recurrent, refactor to fire-and-forget worker"
            );
        }
        let prs = result?
            .into_iter()
            .filter_map(summary_to_gh_metadata)
            .collect();
        // A live poll: the data is current as of now.
        Ok((prs, chrono::Utc::now().to_rfc3339()))
    }
}

/// Map a provider-neutral [`crate::scm::PrSummary`] to [`GhPrMetadata`].
/// Reproduces the prior `parse_one` contract verbatim: `number`,
/// `author_login`, `head_ref` and a recognized `state` are required (a
/// missing/unknown one drops the entry — defensive skip rather than
/// aborting the batch), `is_draft` defaults to false, and `merged_at` is
/// the already-nonempty-filtered optional. (`PrSummary.number` is 0 only
/// when the JSON had no usable `number`, which a real PR never is, so it
/// stands in for `parse_one`'s `number?` presence check.)
fn summary_to_gh_metadata(s: crate::scm::PrSummary) -> Option<GhPrMetadata> {
    let state = match s.state.as_deref()? {
        "OPEN" => GhPrState::Open,
        "MERGED" => GhPrState::Merged,
        "CLOSED" => GhPrState::Closed,
        _ => return None,
    };
    if s.number == 0 {
        return None;
    }
    Some(GhPrMetadata {
        number: s.number,
        author_login: s.author_login?,
        head_ref: s.head_ref?,
        is_cross_repository: s.is_cross_repository.unwrap_or(false),
        is_draft: s.is_draft.unwrap_or(false),
        state,
        merged_at: s.merged_at,
        // #2749: carry the atomic head/base OIDs through (may be None if the
        // provider did not surface them — the gate then fails closed).
        head_ref_oid: s.head_ref_oid,
        base_ref_oid: s.base_ref_oid,
    })
}

// ─── #986 fire-and-forget snapshot poller ──────────────────────────────
//
// The scanner thread used to call `CliGhPoller::poll` SYNCHRONOUSLY — a
// `gh pr list` subprocess (>1s) per repo per tick — which blocked the whole
// scanner tick and surfaced as operator INPUT LAG (#986: slips 1000-1500ms,
// ~every 60s, even with 0 ci-watches because the #972 aggregator maintains the
// full-repo PR-state map). The poll is moved OFF the scanner thread into a single
// background worker; the scanner reads a cached snapshot, never the live gh.

/// Shared snapshot cache + demand set bridging the scanner (reader) and the
/// background gh-poll worker (writer). Race contract: the worker writes, the
/// scanner reads; the worker polls into a LOCAL then briefly locks to swap, never
/// holding the cache lock across the `gh` subprocess.
#[derive(Default)]
pub struct GhPollCache {
    /// repo slug → (latest successful poll snapshot, `polled_at` RFC3339). A repo
    /// PRESENT (even with an empty vec) means the worker has polled it (→ `Ok`);
    /// ABSENT means cold / never-polled (→ `Err`, so a consumer treats it as
    /// ambiguous, NOT a positive "no PR" — the #986 cold-start race). `polled_at`
    /// rides along so the scanner can gate a positive "no PR" on it being newer
    /// than the branch's `created_at` (the stale-snapshot-reuse fix).
    cache: std::sync::Mutex<std::collections::HashMap<String, (Vec<GhPrMetadata>, String)>>,
    /// repos the scanner has requested since the worker last drained — the worker
    /// polls exactly these (zero duplication of the scanner's repo enumeration;
    /// drain = retention: a no-longer-requested repo stops being polled).
    demand: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl GhPollCache {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }

    /// Test seam: seed a per-repo snapshot with an explicit `polled_at`, so a
    /// freshness/stale-reuse test can drive a snapshot whose poll time predates a
    /// branch's `created_at` without a live worker.
    #[cfg(test)]
    pub(crate) fn seed_for_test(&self, repo: &str, prs: Vec<GhPrMetadata>, polled_at: &str) {
        if let Ok(mut c) = self.cache.lock() {
            c.insert(repo.to_string(), (prs, polled_at.to_string()));
        }
    }

    /// Test seam: did the scanner (`SnapshotGhPoller::poll`) record demand for
    /// `repo`? Verifies the discovery half of the chain (scanner seeds demand → the
    /// worker later polls + acts).
    #[cfg(test)]
    pub(crate) fn demand_contains_for_test(&self, repo: &str) -> bool {
        self.demand
            .lock()
            .map(|d| d.contains(repo))
            .unwrap_or(false)
    }
}

/// Non-blocking [`GhPoller`] (#986): records demand + returns the worker's latest
/// snapshot for `repo`. NEVER spawns `gh` — the scanner thread never blocks.
/// Cold cache (worker has not polled `repo` yet) returns `Err` (ambiguous), so a
/// downstream positive-no-PR consumer is never fed a false "0 PRs" during warm-up.
pub struct SnapshotGhPoller {
    shared: std::sync::Arc<GhPollCache>,
}

impl SnapshotGhPoller {
    pub fn new(shared: std::sync::Arc<GhPollCache>) -> Self {
        Self { shared }
    }
}

impl GhPoller for SnapshotGhPoller {
    fn poll(&self, repo: &str) -> anyhow::Result<(Vec<GhPrMetadata>, String)> {
        if let Ok(mut d) = self.shared.demand.lock() {
            d.insert(repo.to_string());
        }
        match self.shared.cache.lock() {
            Ok(c) => match c.get(repo) {
                Some((prs, polled_at)) => Ok((prs.clone(), polled_at.clone())),
                // Cold / never-polled → ambiguous, NOT a positive "no PR".
                None => anyhow::bail!(
                    "#986 gh-poll snapshot not ready for {repo} (worker has not polled yet)"
                ),
            },
            Err(_) => anyhow::bail!("#986 gh-poll cache lock poisoned"),
        }
    }
}

/// Worker poll cadence. Off the scanner thread, so we keep armed-flow
/// responsiveness without any scanner-tick cost; rate ~240/hr/repo, far under the
/// 5000/hr authenticated ceiling.
const WORKER_CADENCE: Duration = Duration::from_secs(15);

/// One worker cycle's work for a single repo (#986 round-4): poll via `poller`
/// (the real [`CliGhPoller`] in production) into a LOCAL, store the snapshot under
/// the cache lock (never held across the subprocess), AND run the
/// FRESHNESS-SENSITIVE `prs` consumers on the JUST-POLLED data so they NEVER see a
/// stale snapshot:
///
/// - `gc_remote_orphans` — DESTRUCTIVE (`delete_remote_ref`). On a stale snapshot
///   this hit the branch-REUSE hazard: `feat/x` PR#10 cached as Merged → `feat/x`
///   recreated/force-pushed with new work → a stale-Merged snapshot in the scanner
///   path would `delete_remote_ref` the now-live branch = lost work. Running it on
///   the worker's fresh poll closes that class. (Bonus: its `gh` calls leave the
///   scanner thread, removing more input lag.)
/// - `auto_arm_unwatched_open_prs` — idempotent + non-destructive, so
///   safe-on-stale, but moved here too so EVERY `prs` consumer either reads
///   fresh-poll data here or is per-branch freshness-gated in the scanner — leaving
///   no ungated consumer.
///
/// A snapshot is updated only on a SUCCESSFUL poll, so a transient `gh` failure
/// RETAINS the last-good snapshot. Extracted (vs inline in the loop) for unit
/// testing with a [`MockGhPoller`].
pub(crate) fn worker_poll_and_act(
    home: &std::path::Path,
    shared: &GhPollCache,
    repo: &str,
    poller: &dyn GhPoller,
) {
    match poller.poll(repo) {
        Ok((prs, polled_at)) => {
            if let Ok(mut c) = shared.cache.lock() {
                c.insert(repo.to_string(), (prs.clone(), polled_at));
            }
            super::remote_gc::gc_remote_orphans(repo, &prs);
            super::auto_arm::auto_arm_unwatched_open_prs(home, repo, &prs);
            // #2749 3b: off-tick producer for the read-only freshness gate — stamp
            // the deterministic latest-main ancestry tuple the scanner consumes.
            super::freshness_populator::stamp_freshness_off_tick(home, repo, &prs);
        }
        Err(e) => tracing::warn!(
            repo = %repo, error = %e,
            "#986 gh-poll worker: poll failed (retaining last snapshot)"
        ),
    }
}

/// Spawn the single background gh-poll worker (#986). Drains the demand set each
/// cycle and runs [`worker_poll_and_act`] per repo (poll + store + fresh-data
/// consumers). `home` is needed for `auto_arm`.
pub fn spawn_gh_poll_worker(home: std::path::PathBuf, shared: std::sync::Arc<GhPollCache>) {
    // fire-and-forget: daemon-lifetime gh-poll worker; the loop has no exit
    // condition and ends only when the process exits (§10.5).
    let spawned = std::thread::Builder::new()
        .name("gh-poll-worker".into())
        .spawn(move || {
            let real = CliGhPoller;
            loop {
                std::thread::sleep(WORKER_CADENCE);
                let repos: Vec<String> = match shared.demand.lock() {
                    Ok(mut d) => d.drain().collect(),
                    Err(_) => continue,
                };
                for repo in repos {
                    worker_poll_and_act(&home, &shared, &repo, &real);
                }
            }
        });
    // codex minor: a failed spawn would leave the cache permanently cold (every
    // repo Err → no auto-arm / auto-release). Surface it loudly rather than
    // silently degrading.
    if let Err(e) = spawned {
        tracing::error!(error = %e, "#986 FAILED to spawn gh-poll worker — pr_state gh-poll is now permanently cold");
    }
}

// ─── cadence + backoff ─────────────────────────────────────────────────

/// Refresh cadence when `auto_armed` (active self-merge flow).
const ARMED_CADENCE: Duration = Duration::from_secs(15);
/// Default refresh cadence otherwise.
const DEFAULT_CADENCE: Duration = Duration::from_secs(60);
/// Backoff ceiling on consecutive failures.
const BACKOFF_CAP: Duration = Duration::from_secs(300);
/// Tick used as the backoff base unit.
const BACKOFF_TICK: Duration = Duration::from_secs(10);

/// Decide whether `state` is due for a gh-poll refresh based on the
/// last poll timestamp, tiered cadence, and exponential backoff.
///
/// Returns `true` if the scanner should issue a poll for this state's
/// repo on this tick.
pub fn should_poll(state: &super::PrState, now_rfc3339: &str) -> bool {
    // First observation: always poll.
    let Some(last) = state.last_gh_poll_at.as_deref() else {
        return true;
    };
    let Ok(last_t) = chrono::DateTime::parse_from_rfc3339(last) else {
        return true; // malformed timestamp — re-poll to recover
    };
    let Ok(now_t) = chrono::DateTime::parse_from_rfc3339(now_rfc3339) else {
        return false;
    };
    let elapsed = (now_t - last_t).num_seconds();
    if elapsed < 0 {
        return false;
    }
    let elapsed = Duration::from_secs(elapsed as u64);
    let base = if state.auto_armed {
        ARMED_CADENCE
    } else {
        DEFAULT_CADENCE
    };
    if state.gh_poll_failures > 0 {
        let backoff = backoff_window(state.gh_poll_failures);
        return elapsed >= backoff.max(base);
    }
    elapsed >= base
}

/// Exponential backoff: `2^failures × tick`, capped at [`BACKOFF_CAP`].
/// Saturates at the cap on overflow.
pub fn backoff_window(failures: u32) -> Duration {
    let shifted = BACKOFF_TICK
        .as_secs()
        .checked_shl(failures)
        .unwrap_or(u64::MAX);
    Duration::from_secs(shifted).min(BACKOFF_CAP)
}

// ─── author resolution (4-tier, INVERTED per dev-2 Pushback 2) ─────────

/// Resolve PR author given gh-poll observation + existing PrState.
/// Tier order (operator-explicit FIRST):
///   1. `fleet.yaml github_login` field match for `author.login`
///   2. `author.login` direct name lookup against fleet.yaml instances
///   3. first `subscribers` entry (from ci_watch)
///   4. `"fixup-lead"` final fallback
///
/// EVERY tier only yields an instance that is eligible for `state.repo`
/// (see [`AuthorIndex`]): a subscriber or the fallback with no matching
/// repo identity is skipped exactly like a tier-1/2 candidate. When no tier
/// yields an eligible instance the result is `None` (fail-closed, with a
/// `tracing::warn`) — callers must NOT deliver repo/branch-bearing notices.
///
/// Test convenience: builds the [`AuthorIndex`] inline (may run git). The
/// scanner builds it OUTSIDE the per-PR flock and calls
/// [`resolve_author_with_index`].
#[cfg(test)]
pub(crate) fn resolve_author_with_gh(
    home: &std::path::Path,
    gh_author: Option<&str>,
    state: &super::PrState,
) -> Option<String> {
    let index = AuthorIndex::build(home, &state.repo);
    resolve_author_with_index(&index, gh_author, state)
}

/// [`resolve_author_with_gh`] against a precomputed [`AuthorIndex`]
/// (built for `state.repo`). Pure — no fleet.yaml load, no subprocess —
/// so it is safe to call under the pr-state flock.
pub(crate) fn resolve_author_with_index(
    index: &AuthorIndex,
    gh_author: Option<&str>,
    state: &super::PrState,
) -> Option<String> {
    if let Some(login) = gh_author {
        if let Some(name) = index.match_github_login(login) {
            return Some(name);
        }
        if let Some(name) = index.match_instance_name(login) {
            return Some(name);
        }
    }
    if let Some(sub) = state.subscribers.iter().find(|s| index.is_eligible(s)) {
        return Some(sub.clone());
    }
    if index.is_eligible("fixup-lead") {
        return Some("fixup-lead".to_string());
    }
    // Reviewer MANDATORY: warn-log so the operator sees the chain fail closed.
    tracing::warn!(
        repo = %state.repo,
        branch = %state.branch,
        gh_author = ?gh_author,
        subscribers = ?state.subscribers,
        "#986 author resolution: no instance eligible for this repo — failing closed \
         (no author notice). Give the author a `repo`/`source_repo` or put it in a team \
         whose `source_repo` is this repo"
    );
    None
}

/// Fleet instances eligible to be the author of a PR in one `repo`, i.e.
/// whose repo identity matches it. Identity precedence per instance:
///   1. explicit `repo` override (Sprint 55 EC4) — invalid slug (e.g. the
///      `repo: none` opt-out sentinel) → no match, no fall-through;
///   2. instance `source_repo` (bare slug / URL / local origin remote);
///   3. `source_repo` of the team(s) listing the instance in `members` or
///      naming it `orchestrator`;
///   4. none of the above → NOT eligible (fail-closed).
///
/// Step 4 replaces the former match-any compat, under which every
/// same-`github_login` instance qualified for every repo — in a
/// shared-account fleet pr-state notices (carrying repo/branch names)
/// went to a random unrelated instance. Step 3 keeps fleets that only
/// configure `source_repo` at team level routable.
///
/// Building may run `git remote get-url` (local `source_repo` paths), so
/// the scanner builds it before entering the per-PR flock.
#[derive(Debug, Default)]
pub(crate) struct AuthorIndex {
    repo: String,
    /// Eligible instances in deterministic preference order: team
    /// orchestrators first, then instance name ascending — never the
    /// iteration order of the `FleetConfig.instances` HashMap.
    eligible: Vec<AuthorCandidate>,
    /// Fingerprint of the fleet.yaml bytes this index was parsed from
    /// (`None` = unreadable). See [`AuthorIndex::fleet_unchanged`].
    fleet_fingerprint: Option<u64>,
}

#[derive(Debug)]
struct AuthorCandidate {
    name: String,
    github_login: Option<String>,
    is_orchestrator: bool,
}

impl AuthorIndex {
    /// Read fleet.yaml and resolve eligibility for `repo`. May run
    /// `git remote get-url` for local `source_repo` paths, so it MUST be
    /// called outside the pr-state flock (#1617; debug-asserted).
    ///
    /// The fingerprint and the parsed config come from the SAME read, so a
    /// concurrent write can only make [`Self::fleet_unchanged`] report a
    /// change (conservative), never hide one.
    pub(crate) fn build(home: &std::path::Path, repo: &str) -> Self {
        let bytes = std::fs::read(crate::fleet::fleet_yaml_path(home)).ok();
        let fleet_fingerprint = bytes.as_deref().map(fingerprint);
        // Only `instances` (repo/source_repo/github_login) and `teams`
        // (members/orchestrator/source_repo) are read — raw fields that
        // `FleetConfig::normalize` does not touch, so a direct parse suffices.
        let eligible = bytes
            .as_deref()
            .and_then(|b| std::str::from_utf8(b).ok())
            .and_then(|y| serde_yaml_ng::from_str::<crate::fleet::FleetConfig>(y).ok())
            .map(|cfg| eligible_for(&cfg, repo))
            // Unreadable / unparsable fleet.yaml → nobody eligible (fail-closed).
            .unwrap_or_default();
        Self {
            repo: repo.to_string(),
            eligible,
            fleet_fingerprint,
        }
    }

    /// The repo slug this index was built for.
    pub(crate) fn repo(&self) -> &str {
        &self.repo
    }

    /// Lock-safe staleness check: re-read fleet.yaml (plain file read, NO
    /// git) and compare with the bytes this index was built from. Called
    /// under the pr-state flock right before a latched notice is queued; a
    /// change means the pre-lock eligibility may be stale, so the caller
    /// defers the emit to the next tick (no send, no latch).
    pub(crate) fn fleet_unchanged(&self, home: &std::path::Path) -> bool {
        let now = std::fs::read(crate::fleet::fleet_yaml_path(home))
            .ok()
            .as_deref()
            .map(fingerprint);
        now == self.fleet_fingerprint
    }

    /// Tier 1: first eligible instance whose `github_login` matches.
    fn match_github_login(&self, gh_login: &str) -> Option<String> {
        let hits: Vec<&AuthorCandidate> = self
            .eligible
            .iter()
            .filter(|c| {
                c.github_login
                    .as_deref()
                    .is_some_and(|gl| gl.eq_ignore_ascii_case(gh_login))
            })
            .collect();
        let chosen = hits.first()?;
        // Orchestrator-first is the routing rule, not an ambiguity; only a
        // tie broken by name order is worth surfacing.
        if hits
            .get(1)
            .is_some_and(|next| next.is_orchestrator == chosen.is_orchestrator)
        {
            let others: Vec<&str> = hits[1..].iter().map(|c| c.name.as_str()).collect();
            tracing::warn!(
                repo = %self.repo,
                %gh_login,
                chosen = %chosen.name,
                ?others,
                "pr_state author resolution: several equally-ranked instances match \
                 github_login + repo — picked by name order; set `repo` on the intended \
                 instance to disambiguate"
            );
        }
        Some(chosen.name.clone())
    }

    /// Is `name` eligible to receive author notices for this repo?
    pub(crate) fn is_eligible(&self, name: &str) -> bool {
        self.eligible.iter().any(|c| c.name == name)
    }

    /// Tier 2: eligible instance literally named `gh_login`.
    fn match_instance_name(&self, gh_login: &str) -> Option<String> {
        self.eligible
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(gh_login))
            .map(|c| c.name.clone())
    }
}

#[cfg(test)]
thread_local! {
    /// Test seam: runs between the scanner's pre-lock `AuthorIndex` build and
    /// its pr-state flock, to simulate a concurrent fleet.yaml edit.
    pub(crate) static AFTER_AUTHOR_INDEX_HOOK: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        std::cell::RefCell::new(None);
}

/// Scanner call point for [`AFTER_AUTHOR_INDEX_HOOK`]; a no-op outside tests.
#[inline]
pub(crate) fn after_author_index_build() {
    #[cfg(test)]
    AFTER_AUTHOR_INDEX_HOOK.with(|h| {
        if let Some(f) = h.borrow().as_ref() {
            f();
        }
    });
}

/// Instances eligible for `repo`, sorted orchestrator-first then by name.
/// Identity per instance: explicit `repo` → `source_repo` → `source_repo`
/// of any team that lists it in `members` OR names it `orchestrator` →
/// none (not eligible).
fn eligible_for(cfg: &crate::fleet::FleetConfig, repo: &str) -> Vec<AuthorCandidate> {
    // Per-build memo only: a path shared by several instances/teams is
    // resolved once per build, never carried across builds.
    let mut slugs: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    let mut resolve = |src: &str| -> Option<String> {
        slugs
            .entry(src.to_string())
            .or_insert_with(|| slug_for_source(std::path::Path::new(src)))
            .clone()
    };
    let matches_repo = |slug: Option<String>| slug.is_some_and(|s| s.eq_ignore_ascii_case(repo));
    let mut eligible: Vec<AuthorCandidate> = Vec::new();
    for (name, inst) in &cfg.instances {
        let matches = if let Some(explicit) = inst.repo.as_deref() {
            // Invalid slug (e.g. the `repo: none` opt-out) → not eligible,
            // never falls through to source_repo / team.
            matches_repo(crate::mcp::handlers::dispatch_hook::canonicalize_repo_slug(
                explicit,
            ))
        } else if let Some(sr) = inst.source_repo.as_deref() {
            matches_repo(resolve(sr))
        } else {
            cfg.teams
                .values()
                .filter(|t| {
                    t.members.iter().any(|m| m == name)
                        || t.orchestrator.as_deref() == Some(name.as_str())
                })
                .filter_map(|t| t.source_repo.as_deref().and_then(|p| p.to_str()))
                .any(|src| matches_repo(resolve(src)))
        };
        if matches {
            let is_orchestrator = cfg
                .teams
                .values()
                .any(|t| t.orchestrator.as_deref() == Some(name.as_str()));
            eligible.push(AuthorCandidate {
                name: name.clone(),
                github_login: inst.github_login.clone(),
                is_orchestrator,
            });
        }
    }
    eligible.sort_by(|a, b| {
        b.is_orchestrator
            .cmp(&a.is_orchestrator)
            .then_with(|| a.name.cmp(&b.name))
    });
    eligible
}

/// `source_repo` → canonical GitHub slug: a bare slug / URL directly,
/// otherwise the local checkout's `origin` remote (a git subprocess).
fn slug_for_source(src: &std::path::Path) -> Option<String> {
    src.to_str()
        .and_then(crate::mcp::handlers::dispatch_hook::canonicalize_repo_slug)
        .or_else(|| {
            debug_assert!(
                !super::in_pr_state_flock(),
                "#1617: `git remote get-url` must not run under the pr-state flock"
            );
            crate::mcp::handlers::dispatch_hook::derive_repo_from_remote_pub(src)
        })
}

fn fingerprint(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

// ─── tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;

    /// Test seam — returns canned responses; records call count.
    pub struct MockGhPoller {
        responses: Arc<Mutex<Vec<anyhow::Result<Vec<GhPrMetadata>>>>>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl MockGhPoller {
        pub fn new(responses: Vec<anyhow::Result<Vec<GhPrMetadata>>>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses)),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub fn call_count(&self) -> usize {
            self.calls.lock().len()
        }
    }

    impl GhPoller for MockGhPoller {
        fn poll(&self, repo: &str) -> anyhow::Result<(Vec<GhPrMetadata>, String)> {
            self.calls.lock().push(repo.to_string());
            let prs = self
                .responses
                .lock()
                .pop()
                .unwrap_or_else(|| Ok(Vec::new()))?;
            // Mock = a live poll: data is current "now" (gate always passes). Tests
            // exercising the stale-snapshot freshness gate drive the cache directly.
            Ok((prs, chrono::Utc::now().to_rfc3339()))
        }
    }

    /// T1: summary_to_gh_metadata maps a canonical PrSummary to full
    /// GhPrMetadata. (gh-JSON → PrSummary parsing is covered by
    /// `scm::tests`; this pins the gh_poll-side mapping that replaced the
    /// old `parse_one`.)
    #[test]
    fn t1_map_canonical_summary() {
        let s = crate::scm::PrSummary {
            number: 984,
            state: Some("MERGED".into()),
            author_login: Some("suzuke".into()),
            head_ref: Some("fix/969-channel-dedup-mirror-skip".into()),
            is_draft: Some(false),
            merged_at: Some("2026-05-20T04:17:09Z".into()),
            ..Default::default()
        };
        let meta = summary_to_gh_metadata(s).expect("map OK");
        assert_eq!(meta.number, 984);
        assert_eq!(meta.author_login, "suzuke");
        assert_eq!(meta.head_ref, "fix/969-channel-dedup-mirror-skip");
        assert!(!meta.is_draft);
        assert_eq!(meta.state, GhPrState::Merged);
        assert_eq!(meta.merged_at.as_deref(), Some("2026-05-20T04:17:09Z"));
    }

    /// T1b: missing optional fields default (is_draft → false, merged_at
    /// → None), matching the prior parse_one tolerance.
    #[test]
    fn t1b_map_missing_optional_fields() {
        let s = crate::scm::PrSummary {
            number: 970,
            state: Some("OPEN".into()),
            author_login: Some("suzuke".into()),
            head_ref: Some("fix/x".into()),
            is_draft: None,
            merged_at: None,
            ..Default::default()
        };
        let meta = summary_to_gh_metadata(s).expect("map OK");
        assert!(!meta.is_draft, "missing isDraft → false");
        assert_eq!(meta.merged_at, None, "missing mergedAt → None");
    }

    /// T1c: unknown state string → None (drop), matching parse_one.
    #[test]
    fn t1c_map_unknown_state_returns_none() {
        let s = crate::scm::PrSummary {
            number: 1,
            state: Some("DRAFT".into()), // not a valid PR state
            author_login: Some("x".into()),
            head_ref: Some("br".into()),
            ..Default::default()
        };
        assert!(summary_to_gh_metadata(s).is_none());
    }

    /// T1d: a missing required field (number absent → 0, or author/head
    /// None) drops the entry — the parse_one `?` semantics.
    #[test]
    fn t1d_map_missing_required_fields_drop() {
        // number 0 (no usable number in JSON) → drop.
        let no_num = crate::scm::PrSummary {
            number: 0,
            state: Some("OPEN".into()),
            author_login: Some("x".into()),
            head_ref: Some("br".into()),
            ..Default::default()
        };
        assert!(summary_to_gh_metadata(no_num).is_none());
        // missing author_login → drop.
        let no_author = crate::scm::PrSummary {
            number: 5,
            state: Some("OPEN".into()),
            author_login: None,
            head_ref: Some("br".into()),
            ..Default::default()
        };
        assert!(summary_to_gh_metadata(no_author).is_none());
    }

    fn fresh_state(branch: &str) -> super::super::PrState {
        let now = chrono::Utc::now().to_rfc3339();
        super::super::PrState {
            repo: "owner/repo".to_string(),
            pr_number: 0,
            branch: branch.to_string(),
            head_sha: "sha-A".to_string(),
            pr_author: String::new(),
            subscribers: vec!["dev".to_string()],
            ci_state: super::super::CiState::Pending,
            verdict_state: super::super::VerdictState::None,
            validated_review_receipts: Vec::new(),
            merge_state: super::super::MergeState::NotReady,
            draft_state: super::super::DraftState::Ready,
            review_class: super::super::ReviewClass::Single,
            ready_emitted_for_sha: None,
            diagnostic_emitted_for_sha: None,
            review_dispatch_emitted_for_sha: None,
            review_dispatch_unavailable_emitted_for_sha: None,
            auto_armed: false,
            auto_armed_for_sha: None,
            auto_armed_at: None,
            last_gh_poll_at: None,
            gh_poll_failures: 0,
            last_gh_state: None,
            closed_unmerged_pending: false,
            freshness_checked_head_sha: None,
            freshness_checked_base_sha: None,
            freshness_checked_at: None,
            freshness_behind_by: None,
            freshness_error: false,
            freshness_retry_after: None,
            observed_head_sha: None,
            observed_base_sha: None,
            observed_at: None,
            observed_error: false,
            reserved_assignments: Vec::new(),
            authority_unknown: false,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    /// T7-a: backoff window math. 0 failures → tick (10s). 1 → 20s.
    /// 2 → 40s. 5 → 320s capped to 300s.
    #[test]
    fn t7a_backoff_window_curve() {
        assert_eq!(backoff_window(0), Duration::from_secs(10));
        assert_eq!(backoff_window(1), Duration::from_secs(20));
        assert_eq!(backoff_window(2), Duration::from_secs(40));
        assert_eq!(backoff_window(5), Duration::from_secs(300), "capped");
        assert_eq!(backoff_window(20), Duration::from_secs(300), "saturated");
        assert_eq!(
            backoff_window(64),
            Duration::from_secs(300),
            "overflow safe"
        );
    }

    /// T7-b: should_poll honors backoff on failures. Backoff EXTENDS
    /// cadence: effective wait is `max(base_cadence, backoff_window)`.
    /// failures=1 + base=60s → 60s wins (backoff=20s < base).
    /// failures=3 + base=60s → backoff=80s wins.
    #[test]
    fn t7b_should_poll_honors_backoff_after_failure() {
        let mut state = fresh_state("feat/x");
        let now = chrono::Utc::now();
        // Pretend a poll just happened 70s ago (past 60s default
        // cadence) + failed 3 times (80s backoff).
        state.last_gh_poll_at = Some((now - chrono::Duration::seconds(70)).to_rfc3339());
        state.gh_poll_failures = 3;
        assert!(
            !should_poll(&state, &now.to_rfc3339()),
            "70s elapsed < 80s backoff with failures=3 → wait"
        );
        // 15s more (85s total) — past 80s backoff window.
        let now2 = now + chrono::Duration::seconds(15);
        assert!(
            should_poll(&state, &now2.to_rfc3339()),
            "85s elapsed >= 80s backoff → poll"
        );
    }

    /// T7-c: tiered cadence honors auto_armed.
    #[test]
    fn t7c_armed_cadence_is_tighter() {
        let mut state = fresh_state("feat/x");
        let now = chrono::Utc::now();
        // Last poll was 20s ago.
        state.last_gh_poll_at = Some((now - chrono::Duration::seconds(20)).to_rfc3339());
        // Default cadence: 60s — should NOT poll yet.
        state.auto_armed = false;
        assert!(
            !should_poll(&state, &now.to_rfc3339()),
            "default 60s not yet"
        );
        // Armed cadence: 15s — should poll (20 > 15).
        state.auto_armed = true;
        assert!(
            should_poll(&state, &now.to_rfc3339()),
            "armed 15s cadence reached"
        );
    }

    /// T7-d: first poll always fires (last_gh_poll_at None).
    #[test]
    fn t7d_first_poll_always_due() {
        let state = fresh_state("feat/x");
        assert!(state.last_gh_poll_at.is_none());
        assert!(should_poll(&state, &chrono::Utc::now().to_rfc3339()));
    }

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        // #3245: "agend-986-" was a prefix of both "agend-986-int-" and
        // "agend-986-worker-", so a tag like "int-x" resolved to another
        // helper's directory. "poll-" cannot prefix either.
        let dir =
            std::env::temp_dir().join(format!("agend-986-poll-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write `yaml` as the fleet.yaml under a fresh tmp home.
    fn home_with_fleet(tag: &str, yaml: &str) -> std::path::PathBuf {
        let home = tmp_home(tag);
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        home
    }

    fn resolve(
        home: &std::path::Path,
        login: Option<&str>,
        state: &super::super::PrState,
    ) -> Option<String> {
        resolve_author_with_gh(home, login, state)
    }

    /// T8-a: tier 1 — `github_login` field wins over name match.
    #[test]
    fn t8a_github_login_field_wins_over_name_match() {
        let home = home_with_fleet(
            "auth-tier1",
            "instances:\n  dev:\n    backend: claude\n    github_login: alice\n    repo: owner/repo\n  \
             alice:\n    backend: claude\n    repo: owner/repo\n",
        );
        let state = fresh_state("br");
        assert_eq!(
            resolve(&home, Some("alice"), &state).as_deref(),
            Some("dev"),
            "github_login=alice on `dev` MUST beat name-match on `alice`"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T8-b: tier 2 — direct name lookup when no github_login set.
    #[test]
    fn t8b_direct_name_match_when_no_github_login() {
        let home = home_with_fleet(
            "auth-tier2",
            "instances:\n  suzuke:\n    backend: claude\n    repo: owner/repo\n",
        );
        let state = fresh_state("br");
        assert_eq!(
            resolve(&home, Some("suzuke"), &state).as_deref(),
            Some("suzuke")
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T8-c: tier 3 — the first ELIGIBLE subscriber when no login match;
    /// an ineligible subscriber ahead of it is skipped.
    #[test]
    fn t8c_subscribers_fallback_requires_eligibility() {
        let home = home_with_fleet(
            "auth-tier3",
            "instances:\n  stranger:\n    backend: claude\n  \
             dev:\n    backend: claude\n    repo: owner/repo\n",
        );
        let mut state = fresh_state("br");
        state.subscribers = vec!["stranger".to_string(), "dev".to_string()];
        assert_eq!(
            resolve(&home, Some("nobody-matches"), &state).as_deref(),
            Some("dev")
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T8-d / T11: nothing eligible (not even a fallback) → `None` + warn.
    /// Reviewer #990 BLOCKING #2: assert the `tracing::warn` fires.
    #[test]
    #[tracing_test::traced_test]
    fn t8d_nothing_eligible_fails_closed_with_warn() {
        let home = home_with_fleet("auth-tier4", "instances: {}\n");
        let mut state = fresh_state("br");
        state.subscribers.clear();
        assert_eq!(resolve(&home, Some("nobody"), &state), None);
        assert!(
            logs_contain("no instance eligible for this repo"),
            "tracing::warn MUST fire when the chain fails closed"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T8-d2: `fixup-lead` is still the last resort, but only when it is
    /// itself eligible for the repo.
    #[test]
    fn t8d2_fixup_lead_only_when_eligible() {
        let home = home_with_fleet(
            "auth-fixup",
            "instances:\n  fixup-lead:\n    backend: claude\n    repo: owner/repo\n",
        );
        let mut state = fresh_state("br");
        state.subscribers.clear();
        assert_eq!(
            resolve(&home, Some("nobody"), &state).as_deref(),
            Some("fixup-lead")
        );
        state.repo = "owner/elsewhere".to_string();
        assert_eq!(resolve(&home, Some("nobody"), &state), None);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T8-e: gh_author=None still falls through to eligible subscribers.
    #[test]
    fn t8e_none_gh_author_falls_to_subscribers() {
        let home = home_with_fleet(
            "auth-none",
            "instances:\n  dev:\n    backend: claude\n    repo: owner/repo\n",
        );
        let state = fresh_state("br"); // subscribers = ["dev"]
        assert_eq!(resolve(&home, None, &state).as_deref(), Some("dev"));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T8-f / #2918 Tier 1: two instances share `github_login` but have
    /// different repos. `source_repo` paths are local/unresolvable so the
    /// explicit `repo` override is the only slug source — tests that
    /// `repo` takes precedence over `source_repo` derivation.
    #[test]
    fn t8f_repo_aware_github_login_resolves_correct_agent() {
        let home = home_with_fleet(
            "auth-repo-aware",
            "instances:\n  \
             bot-repoA:\n    backend: claude\n    github_login: shared-bot\n    \
               source_repo: /nonexistent/local/path-a\n    repo: owner/repo-a\n  \
             bot-repoB:\n    backend: claude\n    github_login: shared-bot\n    \
               source_repo: /nonexistent/local/path-b\n    repo: owner/repo-b\n",
        );
        let mut state = fresh_state("br");
        state.repo = "owner/repo-b".to_string();
        assert_eq!(
            resolve(&home, Some("shared-bot"), &state).as_deref(),
            Some("bot-repoB"),
            "explicit repo override must distinguish same-login instances"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T8-g / #2918 Tier 2: instance name matches gh_login but repo
    /// mismatches → falls through to an eligible subscriber.
    #[test]
    fn t8g_tier2_repo_mismatch_falls_to_subscriber() {
        let home = home_with_fleet(
            "auth-t2-repo",
            "instances:\n  \
             shared-bot:\n    backend: claude\n    \
               source_repo: /nonexistent/local/path\n    repo: owner/other-repo\n  \
             the-subscriber:\n    backend: claude\n    repo: owner/target-repo\n",
        );
        let mut state = fresh_state("br");
        state.repo = "owner/target-repo".to_string();
        state.subscribers = vec!["the-subscriber".to_string()];
        assert_eq!(
            resolve(&home, Some("shared-bot"), &state).as_deref(),
            Some("the-subscriber"),
            "Tier 2 repo mismatch must fall through to subscriber"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T8-h / #2918: explicit `repo` without `source_repo` must still
    /// filter; with no eligible subscriber the result fails closed.
    #[test]
    fn t8h_explicit_repo_no_source_repo_filters() {
        let home = home_with_fleet(
            "auth-t2-nosrc",
            "instances:\n  \
             shared-bot:\n    backend: claude\n    repo: owner/other-repo\n",
        );
        let mut state = fresh_state("br");
        state.repo = "owner/target-repo".to_string();
        state.subscribers = vec!["fallback-sub".to_string()];
        assert_eq!(
            resolve(&home, Some("shared-bot"), &state),
            None,
            "explicit repo mismatch + unknown subscriber must send nothing"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T8-i: an instance with NEITHER `repo` nor `source_repo`, in no team
    /// with a `source_repo`, is not eligible on ANY tier — including as a
    /// subscriber — so nothing is sent.
    #[test]
    fn t8i_no_repo_identity_fails_closed() {
        let home = home_with_fleet(
            "auth-failclosed",
            "instances:\n  \
             unrelated:\n    backend: claude\n    github_login: shared-bot\n  \
             shared-bot:\n    backend: claude\n\
             teams:\n  \
             no-repo-team:\n    members: [unrelated, shared-bot]\n    orchestrator: unrelated\n",
        );
        let mut state = fresh_state("br");
        state.subscribers = vec!["unrelated".to_string()];
        assert_eq!(
            resolve(&home, Some("shared-bot"), &state),
            None,
            "no repo identity anywhere → no recipient"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T8-j: `repo: none` (non-slug sentinel used in live fleet.yaml to
    /// opt an instance out) never matches — not even as a subscriber.
    #[test]
    fn t8j_repo_none_sentinel_never_matches() {
        let home = home_with_fleet(
            "auth-none-sentinel",
            "instances:\n  \
             opted-out:\n    backend: claude\n    github_login: shared-bot\n    repo: none\n  \
             shared-bot:\n    backend: claude\n    repo: none\n",
        );
        let mut state = fresh_state("br");
        state.subscribers = vec!["opted-out".to_string()];
        assert_eq!(resolve(&home, Some("shared-bot"), &state), None);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T8-k: several instances share `github_login` + `repo` and one is a
    /// team orchestrator — the orchestrator wins regardless of HashMap
    /// order or name order (it sorts last alphabetically here).
    #[test]
    fn t8k_multi_candidate_prefers_orchestrator() {
        let home = home_with_fleet(
            "auth-multi-orch",
            "instances:\n  \
             alpha:\n    backend: claude\n    github_login: shared-bot\n    repo: owner/repo\n  \
             bravo:\n    backend: claude\n    github_login: shared-bot\n    repo: owner/repo\n  \
             zulu-lead:\n    backend: claude\n    github_login: shared-bot\n    repo: owner/repo\n  \
             other-repo-lead:\n    backend: claude\n    github_login: shared-bot\n    repo: owner/other\n\
             teams:\n  \
             t1:\n    members: [alpha, bravo, zulu-lead]\n    orchestrator: zulu-lead\n  \
             t2:\n    members: [other-repo-lead]\n    orchestrator: other-repo-lead\n",
        );
        let state = fresh_state("br");
        assert_eq!(
            resolve(&home, Some("shared-bot"), &state).as_deref(),
            Some("zulu-lead")
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T8-l: several matching non-orchestrator candidates → alphabetical
    /// first, independent of HashMap iteration order.
    #[test]
    fn t8l_multi_candidate_without_orchestrator_picks_name_order() {
        let home = home_with_fleet(
            "auth-multi-name",
            "instances:\n  \
             delta:\n    backend: claude\n    github_login: shared-bot\n    repo: owner/repo\n  \
             charlie:\n    backend: claude\n    github_login: shared-bot\n    repo: owner/repo\n  \
             echo:\n    backend: claude\n    github_login: shared-bot\n    repo: owner/repo\n  \
             bravo:\n    backend: claude\n    github_login: shared-bot\n    repo: owner/repo\n  \
             alpha:\n    backend: claude\n    github_login: shared-bot\n    repo: owner/other\n",
        );
        let state = fresh_state("br");
        assert_eq!(
            resolve(&home, Some("shared-bot"), &state).as_deref(),
            Some("bravo"),
            "alpha's repo mismatches; bravo is the alphabetical-first match"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T8-m: instance with no repo fields inherits its team's `source_repo`
    /// (bare slug and ssh-alias remote forms); an unrelated team's member
    /// with the same login is excluded; the team orchestrator wins.
    #[test]
    fn t8m_team_source_repo_resolves_member() {
        let home = home_with_fleet(
            "auth-team-tier",
            "instances:\n  \
             infra-lead:\n    backend: claude\n    github_login: shared-bot\n  \
             infra-dev:\n    backend: claude\n    github_login: shared-bot\n  \
             apps-lead:\n    backend: claude\n    github_login: shared-bot\n\
             teams:\n  \
             infra:\n    members: [infra-lead, infra-dev]\n    orchestrator: infra-lead\n    \
               source_repo: \"git@github-acme:Acme/Infra.git\"\n  \
             apps:\n    members: [apps-lead]\n    orchestrator: apps-lead\n    \
               source_repo: acme/apps\n",
        );
        let mut state = fresh_state("br");
        state.repo = "acme/infra".to_string();
        assert_eq!(
            resolve(&home, Some("shared-bot"), &state).as_deref(),
            Some("infra-lead")
        );
        state.repo = "acme/apps".to_string();
        assert_eq!(
            resolve(&home, Some("shared-bot"), &state).as_deref(),
            Some("apps-lead")
        );
        state.repo = "acme/unknown".to_string();
        assert_eq!(
            resolve(&home, Some("shared-bot"), &state),
            None,
            "no team owns the repo and subscriber `dev` is unknown → nothing"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T8-n: an instance-level `repo` (here the `none` opt-out sentinel)
    /// takes precedence and does NOT fall through to its team's
    /// `source_repo` — keeps the live fleet.yaml stop-gap meaningful.
    #[test]
    fn t8n_instance_repo_overrides_team_tier() {
        let home = home_with_fleet(
            "auth-team-override",
            "instances:\n  \
             opted-out-lead:\n    backend: claude\n    github_login: shared-bot\n    repo: none\n  \
             member:\n    backend: claude\n    github_login: shared-bot\n\
             teams:\n  \
             t:\n    members: [opted-out-lead, member]\n    orchestrator: opted-out-lead\n    \
               source_repo: owner/repo\n",
        );
        let state = fresh_state("br");
        assert_eq!(
            resolve(&home, Some("shared-bot"), &state).as_deref(),
            Some("member")
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Init a real git repo at `dir` with `origin` = `url` (AGEND bypass).
    fn git_repo_with_origin(dir: &std::path::Path, url: &str) {
        std::fs::create_dir_all(dir).unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec!["remote", "add", "origin", url],
        ] {
            let out = std::process::Command::new("git")
                .args(&args)
                .current_dir(dir)
                .env("AGEND_GIT_BYPASS", "1")
                .output()
                .expect("git spawn");
            assert!(out.status.success(), "git {args:?} failed: {out:?}");
        }
    }

    /// TOCTOU (lock-safe check): `fleet_unchanged()` is true for the bytes
    /// the index was built from and false after any fleet.yaml edit.
    #[test]
    fn t8o_fleet_unchanged_detects_edit_after_build() {
        let home = home_with_fleet(
            "auth-fingerprint",
            "instances:\n  dev:\n    backend: claude\n    repo: owner/repo\n",
        );
        let index = AuthorIndex::build(&home, "owner/repo");
        assert!(index.is_eligible("dev"));
        assert!(index.fleet_unchanged(&home));
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  dev:\n    backend: claude\n    repo: none\n",
        )
        .unwrap();
        assert!(
            !index.fleet_unchanged(&home),
            "edit after build must be detected"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A LOCAL `source_repo` checkout is resolved from its CURRENT `origin`
    /// at build time (real git repo, team tier): a remote changed before the
    /// pre-lock build is honoured; no path→slug cache survives across builds.
    #[test]
    fn t8p_build_resolves_current_local_origin() {
        let home = tmp_home("auth-origin");
        let work = home.join("work");
        git_repo_with_origin(&work, "git@github.com:owner/other.git");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!(
                "instances:\n  dev:\n    backend: claude\n\
                 teams:\n  t:\n    members: [dev]\n    source_repo: {}\n",
                work.display()
            ),
        )
        .unwrap();
        assert!(
            !AuthorIndex::build(&home, "owner/repo").is_eligible("dev"),
            "origin is owner/other → ineligible for owner/repo"
        );
        let out = std::process::Command::new("git")
            .args([
                "remote",
                "set-url",
                "origin",
                "git@github.com:owner/repo.git",
            ])
            .current_dir(&work)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(
            AuthorIndex::build(&home, "owner/repo").is_eligible("dev"),
            "next build sees the moved origin"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// #1617 guard: resolving a local `source_repo` (git subprocess) inside
    /// a pr-state flock closure trips the debug assertion.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "#1617")]
    fn t8q_git_lookup_under_pr_state_flock_is_rejected() {
        let home = tmp_home("auth-flock-guard");
        let work = home.join("work");
        git_repo_with_origin(&work, "git@github.com:owner/repo.git");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!(
                "instances:\n  dev:\n    backend: claude\n    source_repo: {}\n",
                work.display()
            ),
        )
        .unwrap();
        let _ = super::super::with_pr_state_or_create(
            &home,
            "owner/repo",
            "br",
            || fresh_state("br"),
            |_| AuthorIndex::build(&home, "owner/repo"),
        );
    }

    /// MockGhPoller smoke — verifies the test seam itself works.
    #[test]
    fn mock_poller_returns_canned_responses() {
        let responses = vec![Ok(vec![GhPrMetadata {
            number: 970,
            author_login: "suzuke".into(),
            head_ref: "fix/x".into(),
            is_cross_repository: false,
            is_draft: false,
            state: GhPrState::Open,
            merged_at: None,
            head_ref_oid: None,
            base_ref_oid: None,
        }])];
        let poller = MockGhPoller::new(responses);
        let (prs, _polled_at) = poller.poll("owner/repo").unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(poller.call_count(), 1);
    }

    /// #986: the SnapshotGhPoller reads the worker's cache (never spawns `gh`), and
    /// a COLD repo returns `Err` (ambiguous) — never `Ok(empty)`, which a
    /// positive-no-PR consumer would misread as "no PR" and false-release.
    #[test]
    fn snapshot_poller_cold_is_err_polled_is_ok() {
        let cache = GhPollCache::new();
        let poller = SnapshotGhPoller::new(cache.clone());
        // Cold (worker hasn't polled) → Err.
        assert!(
            poller.poll("owner/repo").is_err(),
            "cold cache must be Err (ambiguous), never Ok(empty)"
        );
        // poll() recorded demand so the worker will pick the repo up.
        assert!(
            cache.demand_contains_for_test("owner/repo"),
            "demand recorded for the worker"
        );
        // Worker polled, found NO PRs (empty) → Ok((empty, polled_at)) = real "no PR".
        cache
            .cache
            .lock()
            .unwrap()
            .insert("owner/repo".into(), (vec![], "2026-06-06T00:00:00Z".into()));
        let (prs, _at) = poller.poll("owner/repo").unwrap();
        assert_eq!(
            prs,
            Vec::<GhPrMetadata>::new(),
            "polled-empty → Ok(empty) = positively no PR (distinct from cold)"
        );
        // Worker polled, found a PR → Ok((prs, polled_at)).
        let pr = GhPrMetadata {
            number: 5,
            author_login: "a".into(),
            head_ref: "fix/y".into(),
            is_cross_repository: false,
            is_draft: false,
            state: GhPrState::Open,
            merged_at: None,
            head_ref_oid: None,
            base_ref_oid: None,
        };
        cache.cache.lock().unwrap().insert(
            "o/r2".into(),
            (vec![pr.clone()], "2026-06-06T00:00:00Z".into()),
        );
        let (prs2, _at2) = poller.poll("o/r2").unwrap();
        assert_eq!(prs2, vec![pr]);
    }

    /// #986 Bug B: the gh-poll worker must be spawned EXACTLY ONCE in production —
    /// the `PrStateScanHandler`, the single scanner in every mode. A second spawn
    /// (e.g. re-adding the supervisor scan) means 2× gh + warm-up flapping with two
    /// independent caches. Guard the production call-site count across all entry
    /// points. (This test lives in gh_poll.rs — which holds the spawn fn DEFINITION,
    /// not a call site — and assembles the needle, so it never self-counts.)
    #[test]
    fn exactly_one_gh_poll_worker_spawn_site_986() {
        // Scan the WHOLE src/ tree, not a hardcoded 4-file list: a new call site
        // added in any other module (e.g. app/commands.rs, a new daemon handler)
        // would otherwise leave the count at 1 and silently pass. Call sites =
        // all worker-spawn references minus the single `fn` definition. (This
        // comment intentionally avoids the literal `spawn_gh_poll_worker` + `(`
        // sequence so the recursive scan does not count itself.)
        fn collect(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    collect(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let mut files = Vec::new();
        let root = if std::path::Path::new("src").is_dir() {
            std::path::PathBuf::from("src")
        } else {
            std::path::PathBuf::from("agend-terminal/src")
        };
        collect(&root, &mut files);
        // The needle/def are assembled from fragments so THIS test never
        // self-counts (its only reference is the split literal below).
        let needle = format!("{}(", "spawn_gh_poll_worker");
        let def = format!("fn {}(", "spawn_gh_poll_worker");
        let mut refs = 0usize;
        let mut defs = 0usize;
        for f in &files {
            let src = std::fs::read_to_string(f).unwrap_or_default();
            refs += src.matches(needle.as_str()).count();
            defs += src.matches(def.as_str()).count();
        }
        let call_sites = refs - defs;
        assert_eq!(
            call_sites, 1,
            "#986: gh-poll worker must be spawned exactly once across ALL of src/ \
             (found {call_sites} call sites; refs={refs}, defs={defs})"
        );
    }

    /// #986 round-4: the worker runs the moved fresh-data `prs` consumers on the
    /// JUST-polled data — verified via `auto_arm`'s observable effect (a ci-watch
    /// armed for an open PR with a bound agent). It also stores the snapshot.
    /// (`remote_gc`'s `delete_remote_ref` is destructive + not unit-observable, but
    /// rides the same code path here.)
    #[test]
    fn worker_poll_and_act_runs_fresh_consumers_986() {
        let home = std::env::temp_dir().join(format!(
            "agend-986-worker-{}-{}",
            std::process::id(),
            "fresh"
        ));
        let _ = std::fs::remove_dir_all(&home);
        // Bind an agent to feat/x so auto_arm can resolve a subscriber.
        let rt = crate::paths::runtime_dir(&home).join("dev-x");
        std::fs::create_dir_all(&rt).unwrap();
        std::fs::write(
            rt.join("binding.json"),
            serde_json::json!({
                "version": 1, "branch": "feat/x", "task_id": "t",
                "worktree": "", "source_repo": "/x", "issued_at": "2026-06-06T00:00:00Z"
            })
            .to_string(),
        )
        .unwrap();
        let open = GhPrMetadata {
            number: 7,
            author_login: "dev-x".into(),
            head_ref: "feat/x".into(),
            is_cross_repository: false,
            is_draft: false,
            state: GhPrState::Open,
            merged_at: None,
            head_ref_oid: None,
            base_ref_oid: None,
        };
        let cache = GhPollCache::new();
        let poller = MockGhPoller::new(vec![Ok(vec![open])]);
        worker_poll_and_act(&home, &cache, "owner/repo", &poller);
        assert!(
            cache.cache.lock().unwrap().contains_key("owner/repo"),
            "worker stored the fresh snapshot"
        );
        let ci_dir = crate::daemon::ci_watch::ci_watches_dir(&home);
        let armed = std::fs::read_dir(&ci_dir)
            .map(|d| d.flatten().count() > 0)
            .unwrap_or(false);
        assert!(
            armed,
            "worker ran auto_arm on the FRESH poll → a ci-watch was armed"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// #986 round-4 (codex branch-reuse hazard): the DESTRUCTIVE `gc_remote_orphans`
    /// must NEVER be reachable from the stale-snapshot scanner path. A source-scan
    /// pins it OUT of `scanner.rs` (it lives only in the worker, on fresh data) — so
    /// a stale-Merged snapshot can never drive `delete_remote_ref` against a
    /// since-reused live branch. Needle assembled so this test never self-matches.
    #[test]
    fn scanner_does_not_invoke_remote_gc_986() {
        let src = std::fs::read_to_string("src/daemon/pr_state/scanner.rs")
            .or_else(|_| std::fs::read_to_string("agend-terminal/src/daemon/pr_state/scanner.rs"))
            .expect("scanner.rs readable");
        let needle = format!("{}(", "gc_remote_orphans");
        assert!(
            !src.contains(needle.as_str()),
            "#986: gc_remote_orphans (destructive) must not run in the stale-snapshot \
             scanner path — it belongs in the worker on fresh poll data"
        );
    }
}
