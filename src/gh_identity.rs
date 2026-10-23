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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::fleet::{FleetConfig, FleetEnvValue};

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

fn expand_tilde(raw: &str, env: &dyn IdentityEnv) -> PathBuf {
    let raw = raw.trim();
    match (raw, raw.strip_prefix("~/")) {
        ("~", _) => env.home().unwrap_or_else(|| PathBuf::from(raw)),
        (_, Some(rest)) => env
            .home()
            .map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(raw)),
        _ => PathBuf::from(raw),
    }
}

fn existing_dir(dir: PathBuf, what: &str, env: &dyn IdentityEnv) -> anyhow::Result<GhIdentity> {
    if !env.is_dir(&dir) {
        anyhow::bail!(
            "{ERR_PREFIX} {what} → GH_CONFIG_DIR {} does not exist",
            dir.display()
        );
    }
    Ok(GhIdentity::ConfigDir(dir))
}

/// Pure resolution of `repo` (slug or GitHub URL) against `cfg`.
/// An unparseable `repo` has no owner to map → [`GhIdentity::Global`].
pub(crate) fn resolve_in(
    cfg: &FleetConfig,
    repo: &str,
    env: &dyn IdentityEnv,
) -> anyhow::Result<GhIdentity> {
    let Some(slug) = crate::mcp::handlers::dispatch_hook::canonicalize_repo_slug(repo) else {
        return Ok(GhIdentity::Global);
    };
    let owner = slug.split('/').next().unwrap_or_default();

    // A — override table: exact `owner/repo` first, then `owner`.
    let overrides: HashMap<String, &String> = cfg
        .gh_identities
        .iter()
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v))
        .collect();
    for key in [slug.as_str(), owner] {
        if let Some(dir) = overrides.get(key) {
            return existing_dir(
                expand_tilde(dir, env),
                &format!("gh_identities[{key}] for {slug}"),
                env,
            );
        }
    }

    // B — team derivation (exact source_repo slug match only).
    let mut team_names: Vec<&String> = cfg.teams.keys().collect();
    team_names.sort();
    let mut votes: Vec<(&str, Option<PathBuf>)> = Vec::new();
    for name in team_names {
        let team = &cfg.teams[name];
        let Some(src) = team.source_repo.as_deref() else {
            continue;
        };
        if env.source_slug(src).as_deref() != Some(slug.as_str()) {
            continue;
        }
        let configured = team
            .orchestrator
            .as_deref()
            .and_then(|o| cfg.instances.get(o))
            .and_then(|inst| inst.env.get("GH_CONFIG_DIR"));
        let dir = match configured {
            None => None,
            Some(FleetEnvValue::Literal(v)) => Some(expand_tilde(v, env)),
            Some(FleetEnvValue::FromEnv { from_env }) => match env.var(from_env) {
                Some(v) => Some(expand_tilde(&v, env)),
                None => anyhow::bail!(
                    "{ERR_PREFIX} team {name} orchestrator env.GH_CONFIG_DIR from_env \
                     {from_env} is unset in the daemon env (repo {slug})"
                ),
            },
        };
        votes.push((name.as_str(), dir));
    }
    let distinct: HashSet<&Option<PathBuf>> = votes.iter().map(|(_, d)| d).collect();
    if distinct.len() > 1 {
        let detail: Vec<String> = votes
            .iter()
            .map(|(t, d)| match d {
                Some(d) => format!("{t}={}", d.display()),
                None => format!("{t}=<global>"),
            })
            .collect();
        anyhow::bail!(
            "{ERR_PREFIX} teams sharing source_repo {slug} disagree on the orchestrator \
             GH_CONFIG_DIR ({}); add a gh_identities entry to disambiguate",
            detail.join(", ")
        );
    }
    match votes.into_iter().next() {
        Some((team, Some(dir))) => existing_dir(dir, &format!("team {team} for {slug}"), env),
        _ => Ok(GhIdentity::Global),
    }
}

// ---------------------------------------------------------------------------
// Production entry — fleet.yaml read per call (plain file read + parse, like
// `AuthorIndex::build`; `FleetConfig::load` may backfill-write). The parsed
// config and the source_repo → slug memo are kept until the fleet.yaml bytes
// change, so a steady-state call costs one small file read and no git.
// ---------------------------------------------------------------------------

struct Snapshot {
    fingerprint: u64,
    cfg: Arc<FleetConfig>,
    slugs: HashMap<PathBuf, Option<String>>,
}

fn snapshot_cell() -> &'static Mutex<Option<Snapshot>> {
    static CELL: OnceLock<Mutex<Option<Snapshot>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

fn fingerprint(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

struct DaemonEnv<'a> {
    slugs: std::cell::RefCell<&'a mut HashMap<PathBuf, Option<String>>>,
}

impl IdentityEnv for DaemonEnv<'_> {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
    fn home(&self) -> Option<PathBuf> {
        dirs::home_dir()
    }
    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }
    fn source_slug(&self, source_repo: &Path) -> Option<String> {
        let mut slugs = self.slugs.borrow_mut();
        slugs
            .entry(source_repo.to_path_buf())
            .or_insert_with(|| crate::daemon::pr_state::gh_poll::slug_for_source(source_repo))
            .clone()
    }
}

/// Resolve the identity for `repo`; when `repo` is not a slug (site 10 passes
/// `""` and runs gh inside a checkout) the slug comes from `cwd`'s origin.
/// Missing fleet.yaml → Global; unreadable/unparsable → fail closed.
pub(crate) fn resolve(home: &Path, repo: &str, cwd: Option<&Path>) -> anyhow::Result<GhIdentity> {
    let derived;
    let repo = match (
        crate::mcp::handlers::dispatch_hook::canonicalize_repo_slug(repo),
        cwd,
    ) {
        (None, Some(dir)) => {
            derived = crate::mcp::handlers::dispatch_hook::derive_repo_from_remote_pub(dir)
                .unwrap_or_default();
            derived.as_str()
        }
        _ => repo,
    };
    let path = crate::fleet::fleet_yaml_path(home);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(GhIdentity::Global),
        Err(e) => anyhow::bail!("{ERR_PREFIX} cannot read {}: {e}", path.display()),
    };
    let fp = fingerprint(&bytes);
    let mut guard = snapshot_cell().lock().unwrap_or_else(|e| e.into_inner());
    if guard.as_ref().map(|s| s.fingerprint) != Some(fp) {
        let cfg: FleetConfig = std::str::from_utf8(&bytes)
            .map_err(anyhow::Error::from)
            .and_then(|y| serde_yaml_ng::from_str(y).map_err(anyhow::Error::from))
            .map_err(|e| anyhow::anyhow!("{ERR_PREFIX} cannot parse {}: {e}", path.display()))?;
        *guard = Some(Snapshot {
            fingerprint: fp,
            cfg: Arc::new(cfg),
            slugs: HashMap::new(),
        });
    }
    let Some(snap) = guard.as_mut() else {
        return Ok(GhIdentity::Global);
    };
    let cfg = Arc::clone(&snap.cfg);
    let env = DaemonEnv {
        slugs: std::cell::RefCell::new(&mut snap.slugs),
    };
    let identity = resolve_in(&cfg, repo, &env)?;
    if identity == GhIdentity::Global {
        log_global_once(repo);
    }
    Ok(identity)
}

fn log_global_once(repo: &str) {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let mut seen = SEEN
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if seen.insert(repo.to_ascii_lowercase()) {
        tracing::info!(
            repo,
            "#178 gh identity: no gh_identities / team mapping — using the daemon's global gh account"
        );
    }
}

/// Q4: `GH_TOKEN` / `GITHUB_TOKEN` in the daemon env override `GH_CONFIG_DIR`
/// inside gh (and win in REST token discovery). Allowed, but warned once per
/// config dir so the operator knows the mapping is not in effect.
pub(crate) fn warn_if_env_token_overrides(dir: &Path) {
    let set: Vec<&str> = ["GH_TOKEN", "GITHUB_TOKEN"]
        .into_iter()
        .filter(|k| std::env::var(k).is_ok_and(|v| !v.trim().is_empty()))
        .collect();
    if set.is_empty() {
        return;
    }
    static WARNED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let mut warned = WARNED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if warned.insert(dir.to_path_buf()) {
        tracing::warn!(
            config_dir = %dir.display(),
            env = ?set,
            "#178 gh identity: daemon env token overrides the per-repo GH_CONFIG_DIR"
        );
    }
}

#[cfg(test)]
#[path = "gh_identity_tests.rs"]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
