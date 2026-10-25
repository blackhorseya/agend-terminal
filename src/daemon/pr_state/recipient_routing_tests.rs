//! -18577-7 (d-20261024223922551853-2): pr-state recipient routing.
//!
//! (a) A team can own GitHub repos beyond its `source_repo` (e.g. a goapi
//!     checkout plus its frontend), so the frontend PR's notices reach the
//!     team instead of failing closed with no recipient.
//! (b) When the gh author matches nobody and only a reviewer is subscribed,
//!     the reviewer is NOT inferred as the PR author.
//! (c) An unresolvable recipient is logged once per PR, not once per poll.
//! r2 (third-hq-1 REJECTED dc9c3c83 + d-20261025000739159728-1):
//!  - a reviewer already persisted as `pr_author` by the old rule is not used;
//!  - author candidates spanning several teams fail closed unless the PR's own
//!    subscribers narrow them to one team;
//!  - the once-per-PR key is evicted when the PR's state is removed;
//!  - log counts use `tracing_test` (per-test scoped), not a thread-local
//!    `with_default` subscriber, which lost events under parallel tests.

use super::author_authority::AuthorSource;
use super::gh_poll::{resolve_author_with_gh, AuthorIndex};
use super::*;

fn home_with_fleet(tag: &str, yaml: &str) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!(
        "agend-18577-7-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    home
}

fn state_for(repo: &str, branch: &str, subscribers: &[&str]) -> PrState {
    let mut s = new_for_branch(repo, branch, "sha-A", ReviewClass::Single);
    s.subscribers = subscribers.iter().map(|s| s.to_string()).collect();
    s.pr_number = 7;
    s
}

/// The incident shape: one team, `source_repo` = the goapi repo, and the
/// lead + reviewer carry no repo identity of their own.
const TEAM_FLEET: &str = "instances:\n  \
     lead:\n    backend: claude\n    role_kind: orchestrator\n    github_login: shared-bot\n  \
     rev:\n    backend: codex\n    role_kind: reviewer\n\
     teams:\n  game:\n    orchestrator: lead\n    members: [lead, rev]\n    \
     source_repo: acme/game-goapi\n    repos: [acme/game-frontend]\n";

#[test]
fn team_repos_make_the_team_eligible_for_its_extra_repo() {
    let home = home_with_fleet("team-repos", TEAM_FLEET);
    let idx = AuthorIndex::build(&home, "acme/game-frontend");
    assert!(
        idx.is_eligible("lead"),
        "a repo listed in the team's `repos` must make its orchestrator eligible"
    );
    let state = state_for("acme/game-frontend", "feat/x", &["lead"]);
    assert_eq!(
        resolve_author_with_gh(&home, Some("shared-bot"), &state).as_deref(),
        Some("lead")
    );
    assert_eq!(
        resolve_merge_authority_checked(&home, &state, &idx).as_deref(),
        Some("lead"),
        "[pr-ready-for-merge] for the extra repo must reach the team orchestrator"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Controls: the `source_repo` repo still routes (the extra list is additive,
/// not an override), and a repo in neither list still fails closed.
#[test]
fn team_repos_are_additive_and_unlisted_repos_still_fail_closed() {
    let home = home_with_fleet("team-repos-ctl", TEAM_FLEET);
    assert!(AuthorIndex::build(&home, "acme/game-goapi").is_eligible("lead"));
    assert!(!AuthorIndex::build(&home, "acme/other").is_eligible("lead"));
    let _ = std::fs::remove_dir_all(&home);
}

/// Back-compat: a team configured the old way (single `source_repo`, no
/// `repos`) parses and routes exactly as before.
#[test]
fn team_without_repos_is_unchanged() {
    let home = home_with_fleet(
        "team-no-repos",
        "instances:\n  lead:\n    backend: claude\n\
         teams:\n  game:\n    orchestrator: lead\n    members: [lead]\n    \
         source_repo: acme/game-goapi\n",
    );
    assert!(AuthorIndex::build(&home, "acme/game-goapi").is_eligible("lead"));
    assert!(!AuthorIndex::build(&home, "acme/game-frontend").is_eligible("lead"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn reviewer_subscriber_is_not_inferred_as_pr_author() {
    let home = home_with_fleet("rev-not-author", TEAM_FLEET);
    let state = state_for("acme/game-goapi", "feat/y", &["rev"]);
    let author = resolve_author_with_gh(&home, Some("someone-else"), &state);
    assert_ne!(
        author.as_deref(),
        Some("rev"),
        "gh author matched nobody; the reviewer subscriber must not become pr_author"
    );
    assert_eq!(author, None);
    let _ = std::fs::remove_dir_all(&home);
}

/// Controls: a non-reviewer BOUND to the branch is the author (r3: subscribers
/// never are), and the reviewer's subscription still routes merge authority to
/// its orchestrator.
#[test]
fn bound_non_reviewer_still_resolves_and_reviewer_still_finds_the_lead() {
    let home = home_with_fleet("rev-ctl", TEAM_FLEET);
    let state = state_for("acme/game-goapi", "feat/z", &["lead"]);
    crate::binding::bind(&home, "lead", "t-z", "feat/z");
    assert_eq!(
        resolve_author_with_gh(&home, Some("someone-else"), &state).as_deref(),
        Some("lead")
    );
    let idx = AuthorIndex::build(&home, "acme/game-goapi");
    let reviewer_only = state_for("acme/game-goapi", "feat/z2", &["rev"]);
    assert_eq!(
        resolve_merge_authority_checked(&home, &reviewer_only, &idx).as_deref(),
        Some("lead")
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// `logs_assert` (injected by `#[traced_test]`, scoped to the calling test)
/// check: exactly `want` captured lines contain `needle`.
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

/// (c): the emit-time author and merge-authority resolvers run every scanner
/// tick; with nothing eligible each used to warn every time.
#[test]
#[tracing_test::traced_test]
fn unresolvable_recipients_warn_once_per_pr() {
    let home = home_with_fleet("log-once", "instances: {}\n");
    let repo = format!("acme/log-once-{}", uuid::Uuid::new_v4().simple());
    let idx = AuthorIndex::build(&home, &repo);
    let mut state = state_for(&repo, "feat/quiet", &[]);
    let mut other = state_for(&repo, "feat/other-pr", &[]);
    for _ in 0..3 {
        assert_eq!(resolve_author_checked(&mut state, &idx, None).0, None);
        assert_eq!(resolve_merge_authority_checked(&home, &state, &idx), None);
    }
    // Control: a different PR is a different key and still warns.
    assert_eq!(resolve_author_checked(&mut other, &idx, None).0, None);
    logs_assert(|lines| {
        exactly(lines, "no instance eligible for this repo", 2)?;
        exactly(lines, "no merge authority eligible", 1)
    });
    let _ = std::fs::remove_dir_all(&home);
}

/// r2 P1-1: a reviewer persisted as `pr_author` by the pre-fix tier-3 rule
/// must not receive author notices after upgrade.
#[test]
fn persisted_reviewer_author_is_not_used() {
    let home = home_with_fleet("persisted-rev", TEAM_FLEET);
    let idx = AuthorIndex::build(&home, "acme/game-goapi");
    let mut state = state_for("acme/game-goapi", "feat/old", &["rev"]);
    state.pr_author = "rev".to_string();
    state.pr_author_source = Some(AuthorSource::GhLogin);
    assert_eq!(resolve_author_checked(&mut state, &idx, None).0, None);
    // r3: a persisted non-reviewer is used only WITH provenance …
    state.pr_author = "lead".to_string();
    assert_eq!(
        resolve_author_checked(&mut state, &idx, None).0.as_deref(),
        Some("lead")
    );
    // … a legacy value (no source) is never routed, even when eligible.
    state.pr_author_source = None;
    assert_eq!(resolve_author_checked(&mut state, &idx, None).0, None);
    let _ = std::fs::remove_dir_all(&home);
}

/// Two teams both own `acme/shared` (one by `source_repo`, one by `repos`)
/// and both leads carry the fleet's shared login.
const SHARED_FLEET: &str = "instances:\n  \
     lead-a:\n    backend: claude\n    github_login: shared-bot\n  \
     lead-b:\n    backend: claude\n    github_login: shared-bot\n\
     teams:\n  a:\n    orchestrator: lead-a\n    members: [lead-a]\n    source_repo: acme/shared\n  \
     b:\n    orchestrator: lead-b\n    members: [lead-b]\n    source_repo: acme/b-api\n    \
     repos: [acme/shared]\n";

/// r2 P1-2 (d-20261025000739159728-1): candidates from more than one team
/// that nothing on the PR narrows down → no recipient, one WARN per PR.
#[test]
#[tracing_test::traced_test]
fn author_spanning_two_teams_fails_closed_once() {
    let home = home_with_fleet("two-teams", SHARED_FLEET);
    let idx = AuthorIndex::build(&home, "acme/shared");
    let state = state_for("acme/shared", "feat/amb", &[]);
    for _ in 0..3 {
        assert_eq!(
            super::gh_poll::resolve_author_logged_once(&idx, None, Some("shared-bot"), &state),
            None,
            "shared login + two owning teams must not pick one by sort order"
        );
    }
    // r3: subscribers from both teams do not narrow the login either (option A).
    let subs = state_for("acme/shared", "feat/amb-subs", &["lead-a", "lead-b"]);
    assert_eq!(
        super::gh_poll::resolve_author_logged_once(&idx, None, Some("shared-bot"), &subs),
        None
    );
    logs_assert(|lines| exactly(lines, "more than one team", 2));
    let _ = std::fs::remove_dir_all(&home);
}

/// Controls: the branch's binding identifies the author among the shared-login
/// hits (r3: the subscriber no longer does); a single owning team is unaffected.
#[test]
fn binding_disambiguates_and_single_team_is_unaffected() {
    let home = home_with_fleet("two-teams-ctl", SHARED_FLEET);
    let idx = AuthorIndex::build(&home, "acme/shared");
    let state = state_for("acme/shared", "feat/sub", &["lead-b"]);
    crate::binding::bind(&home, "lead-b", "t-sub", "feat/sub");
    assert_eq!(
        resolve_author_with_gh(&home, Some("shared-bot"), &state).as_deref(),
        Some("lead-b")
    );
    let only_b = state_for("acme/b-api", "feat/b", &[]);
    assert_eq!(
        resolve_author_with_gh(&home, Some("shared-bot"), &only_b).as_deref(),
        Some("lead-b")
    );
    let _ = idx;
    let _ = std::fs::remove_dir_all(&home);
}

/// The live agend-terminal shape: `personal` owns the repo by instance
/// `repo`, `third` by its team's `source_repo`, both with the shared login.
/// The author is the instance bound to the branch (r3), with `third` also
/// subscribed as the CI watcher.
#[test]
fn live_shape_routes_to_the_bound_instance() {
    let home = home_with_fleet(
        "live-shape",
        "instances:\n  \
         personal:\n    backend: claude\n    github_login: shared-bot\n    repo: [acme/notes, acme/term]\n  \
         third:\n    backend: claude\n    github_login: shared-bot\n  \
         rev3:\n    backend: codex\n    role_kind: reviewer\n\
         teams:\n  p:\n    orchestrator: personal\n    members: [personal]\n  \
         t:\n    orchestrator: third\n    members: [third, rev3]\n    source_repo: acme/term\n",
    );
    let state = state_for("acme/term", "fix/x", &["personal", "third"]);
    crate::binding::bind(&home, "personal", "t-x", "fix/x");
    assert_eq!(
        resolve_author_with_gh(&home, Some("shared-bot"), &state).as_deref(),
        Some("personal")
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// r2 P2: the once-per-PR key is dropped when the PR's state is removed
/// (terminal / GC), so the set does not grow with every unresolved PR ever
/// seen; a branch reused by a later PR warns again.
#[test]
#[tracing_test::traced_test]
fn removing_pr_state_evicts_the_once_key() {
    let home = home_with_fleet("evict", "instances: {}\n");
    let repo = format!("acme/evict-{}", uuid::Uuid::new_v4().simple());
    let idx = AuthorIndex::build(&home, &repo);
    let mut state = state_for(&repo, "feat/gone", &[]);
    assert_eq!(resolve_author_checked(&mut state, &idx, None).0, None);
    assert_eq!(resolve_author_checked(&mut state, &idx, None).0, None);
    remove(&home, &repo, "feat/gone").unwrap();
    assert_eq!(resolve_author_checked(&mut state, &idx, None).0, None);
    logs_assert(|lines| exactly(lines, "no instance eligible for this repo", 2));
    let _ = std::fs::remove_dir_all(&home);
}
