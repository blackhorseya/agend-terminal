//! #178 — pick the `gh` identity (`GH_CONFIG_DIR`) per repo for the daemon's
//! own GitHub calls.
//!
//! Instance `env.GH_CONFIG_DIR` only reaches agent PTYs; the daemon itself ran
//! every `gh` / REST call as the global gh account, so a repo that account
//! cannot see (a KicksCrew private repo while the global account is
//! blackhorseya) failed with `Could not resolve to a Repository`. Global
//! account switching (`gh auth switch`) is forbidden — the identity is chosen
//! per call instead.
//!
//! Resolution order for a repo slug (Sean 2026-10-23, d-20261023025838276661-3):
//! 1. **A — override table**: fleet.yaml top-level `gh_identities`, key
//!    `owner/repo` or `owner` (case-insensitive, `owner/repo` beats `owner`),
//!    value = the `GH_CONFIG_DIR` (`~` expanded).
//! 2. **B — team derivation**: every team whose `source_repo` resolves to
//!    exactly this slug contributes its orchestrator's own
//!    `env.GH_CONFIG_DIR` (`defaults.env` is NOT read). Teams that disagree —
//!    including one with a dir and one without — fail closed.
//! 3. **Global**: no mapping → the daemon's global gh account (pre-#178
//!    behaviour), logged once per slug at info.
//!
//! Fail closed (`Err`, message prefixed `gh identity:`) when the chosen dir
//! does not exist, a `from_env` source is unset, or B is ambiguous — never a
//! silent fallback to Global. `GH_TOKEN` / `GITHUB_TOKEN` in the daemon env
//! still win over the config dir (gh's own precedence); that is warned, not
//! blocked ([`warn_if_env_token_overrides`]).

// RED (#178): `resolve_in`, `IdentityEnv` and `ConfigDir` are exercised only by
// the tests until GREEN wires them; GREEN removes this allow.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use crate::fleet::FleetConfig;

/// Error-message prefix for every fail-closed resolution (greppable in logs).
pub(crate) const ERR_PREFIX: &str = "gh identity:";

/// Which gh account a call runs as.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum GhIdentity {
    /// The daemon's global gh account (no `GH_CONFIG_DIR` override).
    Global,
    /// Run with `GH_CONFIG_DIR` set to this directory.
    ConfigDir(PathBuf),
}

/// Injectable side effects of [`resolve_in`] so the core is pure in tests.
pub(crate) trait IdentityEnv {
    /// Daemon env var (trimmed; blank = unset).
    fn var(&self, key: &str) -> Option<String>;
    /// User home for `~` expansion.
    fn home(&self) -> Option<PathBuf>;
    fn is_dir(&self, path: &Path) -> bool;
    /// `source_repo` → canonical lowercase `owner/repo` (may run git).
    fn source_slug(&self, source_repo: &Path) -> Option<String>;
}

/// Pure resolution of `repo` (slug or GitHub URL) against `cfg`.
/// An unparseable `repo` has no owner to map → [`GhIdentity::Global`].
///
/// RED stub (#178): pre-#178 behaviour — every repo runs as the global account.
pub(crate) fn resolve_in(
    cfg: &FleetConfig,
    repo: &str,
    env: &dyn IdentityEnv,
) -> anyhow::Result<GhIdentity> {
    let _ = (cfg, repo, env);
    Ok(GhIdentity::Global)
}

/// Resolve the identity for `repo`; when `repo` is not a slug (site 10 passes
/// `""` and runs gh inside a checkout) the slug comes from `cwd`'s origin.
/// Missing fleet.yaml → Global; unreadable/unparsable → fail closed.
///
/// RED stub (#178): pre-#178 behaviour, no I/O.
pub(crate) fn resolve(home: &Path, repo: &str, cwd: Option<&Path>) -> anyhow::Result<GhIdentity> {
    let _ = (home, repo, cwd);
    Ok(GhIdentity::Global)
}

#[cfg(test)]
#[path = "gh_identity_tests.rs"]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
