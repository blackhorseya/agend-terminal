//! #178 — `resolve_in` resolution order and fail-closed cases. The env is a
//! stub: fixed home, an explicit set of existing dirs, and a source_repo →
//! slug table, so no git / real env / real filesystem is touched.

use super::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

struct StubEnv {
    vars: HashMap<String, String>,
    dirs: HashSet<PathBuf>,
    slugs: HashMap<PathBuf, String>,
}

impl StubEnv {
    fn new() -> Self {
        Self {
            vars: HashMap::new(),
            dirs: [
                "/h/.agend/gh/kickscrew",
                "/h/.agend/gh/blackhorseya",
                "/abs/gh",
            ]
            .into_iter()
            .map(PathBuf::from)
            .collect(),
            slugs: [
                ("/src/goapi", "kickscrew/kxcw-goapi-v2"),
                ("/src/goapi-clone", "kickscrew/kxcw-goapi-v2"),
                ("/src/xinfra", "blackhorseya/x-infra"),
            ]
            .into_iter()
            .map(|(p, s)| (PathBuf::from(p), s.to_string()))
            .collect(),
        }
    }
}

impl IdentityEnv for StubEnv {
    fn var(&self, key: &str) -> Option<String> {
        self.vars.get(key).cloned()
    }
    fn home(&self) -> Option<PathBuf> {
        Some(PathBuf::from("/h"))
    }
    fn is_dir(&self, path: &Path) -> bool {
        self.dirs.contains(path)
    }
    fn source_slug(&self, source_repo: &Path) -> Option<String> {
        self.slugs.get(source_repo).cloned()
    }
}

fn fleet(yaml: &str) -> FleetConfig {
    serde_yaml_ng::from_str(yaml).expect("test fleet.yaml parses")
}

fn dir(p: &str) -> GhIdentity {
    GhIdentity::ConfigDir(PathBuf::from(p))
}

const KXCW_TEAM: &str = r#"
instances:
  kxcw-lead:
    env:
      GH_CONFIG_DIR: ~/.agend/gh/kickscrew
teams:
  kxcw:
    orchestrator: kxcw-lead
    source_repo: /src/goapi
"#;

// --- controls (pass on the Global stub too) --------------------------------

#[test]
fn no_mapping_is_global_178() {
    let cfg = fleet("instances: {}\n");
    let got = resolve_in(&cfg, "8am-tech/site", &StubEnv::new()).unwrap();
    assert_eq!(got, GhIdentity::Global);
}

#[test]
fn unparseable_repo_is_global_178() {
    let cfg = fleet(KXCW_TEAM);
    assert_eq!(
        resolve_in(&cfg, "", &StubEnv::new()).unwrap(),
        GhIdentity::Global
    );
}

#[test]
fn gh_identities_round_trips_through_serde_178() {
    let cfg = fleet("gh_identities:\n  KicksCrew: ~/.agend/gh/kickscrew\n");
    assert_eq!(
        cfg.gh_identities.get("KicksCrew").map(String::as_str),
        Some("~/.agend/gh/kickscrew")
    );
    let out = serde_yaml_ng::to_string(&FleetConfig::default()).unwrap();
    assert!(
        !out.contains("gh_identities"),
        "empty map must not be written back: {out}"
    );
}

// --- A: override table -----------------------------------------------------

#[test]
fn override_owner_key_case_insensitive_178() {
    let cfg = fleet("gh_identities:\n  KicksCrew: ~/.agend/gh/kickscrew\n");
    let got = resolve_in(&cfg, "kickscrew/kxcw-marketplace-goapi", &StubEnv::new()).unwrap();
    assert_eq!(got, dir("/h/.agend/gh/kickscrew"));
}

#[test]
fn override_accepts_github_url_repo_178() {
    let cfg = fleet("gh_identities:\n  kickscrew: /abs/gh\n");
    let got = resolve_in(
        &cfg,
        "https://github.com/KicksCrew/kxcw-infra.git",
        &StubEnv::new(),
    )
    .unwrap();
    assert_eq!(got, dir("/abs/gh"));
}

#[test]
fn override_owner_repo_beats_owner_178() {
    let cfg = fleet(
        "gh_identities:\n  kickscrew: ~/.agend/gh/kickscrew\n  KicksCrew/Public-Thing: /abs/gh\n",
    );
    let env = StubEnv::new();
    assert_eq!(
        resolve_in(&cfg, "kickscrew/public-thing", &env).unwrap(),
        dir("/abs/gh")
    );
    assert_eq!(
        resolve_in(&cfg, "kickscrew/other", &env).unwrap(),
        dir("/h/.agend/gh/kickscrew")
    );
}

#[test]
fn override_missing_dir_fails_closed_178() {
    let cfg = fleet("gh_identities:\n  kickscrew: ~/.agend/gh/nope\n");
    let err = resolve_in(&cfg, "kickscrew/kxcw-goapi-v2", &StubEnv::new()).unwrap_err();
    assert!(err.to_string().starts_with(ERR_PREFIX), "{err}");
    assert!(err.to_string().contains("/h/.agend/gh/nope"), "{err}");
}

#[test]
fn override_beats_team_derivation_178() {
    let cfg = fleet(&format!(
        "gh_identities:\n  kickscrew: /abs/gh\n{KXCW_TEAM}"
    ));
    let got = resolve_in(&cfg, "kickscrew/kxcw-goapi-v2", &StubEnv::new()).unwrap();
    assert_eq!(got, dir("/abs/gh"));
}

// --- B: team derivation ----------------------------------------------------

#[test]
fn team_literal_config_dir_178() {
    let cfg = fleet(KXCW_TEAM);
    let got = resolve_in(&cfg, "KicksCrew/kxcw-goapi-v2", &StubEnv::new()).unwrap();
    assert_eq!(got, dir("/h/.agend/gh/kickscrew"));
}

#[test]
fn team_only_exact_slug_match_178() {
    // Same owner, different repo: B does not generalise to the owner.
    let cfg = fleet(KXCW_TEAM);
    let got = resolve_in(&cfg, "kickscrew/kxcw-infra", &StubEnv::new()).unwrap();
    assert_eq!(got, GhIdentity::Global);
}

#[test]
fn team_from_env_config_dir_178() {
    let cfg = fleet(
        r#"
instances:
  kxcw-lead:
    env:
      GH_CONFIG_DIR: { from_env: KXCW_GH_DIR }
teams:
  kxcw: { orchestrator: kxcw-lead, source_repo: /src/goapi }
"#,
    );
    let mut env = StubEnv::new();
    env.vars
        .insert("KXCW_GH_DIR".into(), "~/.agend/gh/kickscrew".into());
    assert_eq!(
        resolve_in(&cfg, "kickscrew/kxcw-goapi-v2", &env).unwrap(),
        dir("/h/.agend/gh/kickscrew")
    );
    env.vars.clear();
    let err = resolve_in(&cfg, "kickscrew/kxcw-goapi-v2", &env).unwrap_err();
    assert!(err.to_string().contains("KXCW_GH_DIR"), "{err}");
}

#[test]
fn team_dir_missing_fails_closed_178() {
    let cfg = fleet(
        r#"
instances:
  kxcw-lead: { env: { GH_CONFIG_DIR: /gone } }
teams:
  kxcw: { orchestrator: kxcw-lead, source_repo: /src/goapi }
"#,
    );
    let err = resolve_in(&cfg, "kickscrew/kxcw-goapi-v2", &StubEnv::new()).unwrap_err();
    assert!(err.to_string().starts_with(ERR_PREFIX), "{err}");
}

#[test]
fn teams_agreeing_on_dir_resolve_178() {
    let cfg = fleet(
        r#"
instances:
  a-lead: { env: { GH_CONFIG_DIR: ~/.agend/gh/kickscrew } }
  b-lead: { env: { GH_CONFIG_DIR: /h/.agend/gh/kickscrew } }
teams:
  a: { orchestrator: a-lead, source_repo: /src/goapi }
  b: { orchestrator: b-lead, source_repo: /src/goapi-clone }
"#,
    );
    let got = resolve_in(&cfg, "kickscrew/kxcw-goapi-v2", &StubEnv::new()).unwrap();
    assert_eq!(got, dir("/h/.agend/gh/kickscrew"));
}

#[test]
fn teams_disagreeing_fail_closed_178() {
    let cfg = fleet(
        r#"
instances:
  a-lead: { env: { GH_CONFIG_DIR: ~/.agend/gh/kickscrew } }
  b-lead: { env: { GH_CONFIG_DIR: /abs/gh } }
teams:
  a: { orchestrator: a-lead, source_repo: /src/goapi }
  b: { orchestrator: b-lead, source_repo: /src/goapi-clone }
"#,
    );
    let err = resolve_in(&cfg, "kickscrew/kxcw-goapi-v2", &StubEnv::new()).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.starts_with(ERR_PREFIX) && msg.contains("a=") && msg.contains("b="),
        "{msg}"
    );
}

#[test]
fn team_with_and_without_dir_fail_closed_178() {
    let cfg = fleet(
        r#"
instances:
  a-lead: { env: { GH_CONFIG_DIR: ~/.agend/gh/kickscrew } }
  b-lead: {}
teams:
  a: { orchestrator: a-lead, source_repo: /src/goapi }
  b: { orchestrator: b-lead, source_repo: /src/goapi-clone }
"#,
    );
    let err = resolve_in(&cfg, "kickscrew/kxcw-goapi-v2", &StubEnv::new()).unwrap_err();
    assert!(err.to_string().contains("b=<global>"), "{err}");
}

#[test]
fn team_orchestrator_without_key_is_global_and_defaults_env_ignored_178() {
    let cfg = fleet(
        r#"
defaults:
  env:
    GH_CONFIG_DIR: ~/.agend/gh/kickscrew
instances:
  xi-lead: {}
teams:
  x-infra: { orchestrator: xi-lead, source_repo: /src/xinfra }
"#,
    );
    let got = resolve_in(&cfg, "blackhorseya/x-infra", &StubEnv::new()).unwrap();
    assert_eq!(got, GhIdentity::Global);
}
