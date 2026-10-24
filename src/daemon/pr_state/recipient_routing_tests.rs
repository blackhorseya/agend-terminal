//! -18577-7 (d-20261024223922551853-2): pr-state recipient routing.
//!
//! (a) A team can own GitHub repos beyond its `source_repo` (e.g. a goapi
//!     checkout plus its frontend), so the frontend PR's notices reach the
//!     team instead of failing closed with no recipient.
//! (b) When the gh author matches nobody and only a reviewer is subscribed,
//!     the reviewer is NOT inferred as the PR author.
//! (c) An unresolvable recipient is logged once per PR, not once per poll.

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

/// Controls: a non-reviewer subscriber is still the tier-3 author, and the
/// reviewer's subscription still routes merge authority to its orchestrator.
#[test]
fn non_reviewer_subscriber_still_resolves_and_reviewer_still_finds_the_lead() {
    let home = home_with_fleet("rev-ctl", TEAM_FLEET);
    let state = state_for("acme/game-goapi", "feat/z", &["lead"]);
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

fn capture_logs<F: FnOnce()>(f: F) -> String {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    #[derive(Clone)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Buf {
            self.clone()
        }
    }
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sub = tracing_subscriber::fmt()
        .with_writer(Buf(buf.clone()))
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .without_time()
        .finish();
    tracing::subscriber::with_default(sub, f);
    let bytes = buf.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap()
}

/// (c): the emit-time author and merge-authority resolvers run every scanner
/// tick; with nothing eligible each used to warn every time.
#[test]
fn unresolvable_recipients_warn_once_per_pr() {
    let home = home_with_fleet("log-once", "instances: {}\n");
    let repo = format!("acme/log-once-{}", uuid::Uuid::new_v4().simple());
    let idx = AuthorIndex::build(&home, &repo);
    let state = state_for(&repo, "feat/quiet", &[]);
    let other = state_for(&repo, "feat/other-pr", &[]);
    let logs = capture_logs(|| {
        for _ in 0..3 {
            assert_eq!(resolve_author_checked(&state, &idx), None);
            assert_eq!(resolve_merge_authority_checked(&home, &state, &idx), None);
        }
        // Control: a different PR is a different key and still warns.
        assert_eq!(resolve_author_checked(&other, &idx), None);
    });
    let author_warns = logs.matches("no instance eligible for this repo").count();
    let authority_warns = logs.matches("no merge authority eligible").count();
    assert_eq!(
        author_warns, 2,
        "author warn: once for feat/quiet + once for the control PR\n{logs}"
    );
    assert_eq!(authority_warns, 1, "authority warn once per PR\n{logs}");
    let _ = std::fs::remove_dir_all(&home);
}
