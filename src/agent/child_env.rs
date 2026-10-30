//! Env assembly shared by every path that launches a backend process: the PTY
//! spawn in `build_command` and the Codex managed app-server launch. The
//! app-server is the process that actually runs Codex's tool shells, so it must
//! see the same isolation, identity pins and fleet.yaml `env:` as a PTY backend.

use super::{
    env_isolation_enabled, env_key_in, is_sensitive_env_key, resolve_child_env,
    warn_env_isolation_disabled_once,
};
use crate::backend::Backend;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;

/// Env to apply to a child process: optionally clear the inherited env first,
/// then set `vars` in order (later entries win).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ChildEnv {
    pub(crate) clear: bool,
    pub(crate) vars: Vec<(String, OsString)>,
}

impl ChildEnv {
    /// Apply to a `std::process::Command` (the CommandBuilder path applies the
    /// same fields itself).
    pub(crate) fn apply_to_command(&self, command: &mut std::process::Command) {
        if self.clear {
            command.env_clear();
        }
        for (k, v) in &self.vars {
            command.env(k, v);
        }
    }
}

/// #1440 env isolation plus the AgEnD identity pins, in apply order.
///
/// Isolation is default off (phased rollout) — when off, behavior is unchanged
/// except a one-time warn listing the inherited keys that isolation would drop.
/// The pins are written after the isolation vars, so they win over anything the
/// plan injected.
pub(crate) fn base_child_env(
    name: &str,
    backend: Option<&Backend>,
    home: Option<&Path>,
) -> ChildEnv {
    let mut out = ChildEnv::default();
    let source_env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    let passthrough = home
        .and_then(|h| crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(h)).ok())
        .map(|f| f.resolve_passthrough_env(name))
        .unwrap_or_default();
    let plan = resolve_child_env(backend, &passthrough, &source_env);
    if env_isolation_enabled() {
        out.clear = true;
        out.vars
            .extend(plan.injected.into_iter().map(|(k, v)| (k, v.into())));
    } else {
        warn_env_isolation_disabled_once(&plan.dropped);
    }

    let mut pin = |k: &str, v: OsString| out.vars.push((k.to_string(), v));
    pin("AGEND_INSTANCE_NAME", name.into());
    // #t-…777-1: hand the spawned agent the daemon's own pid so the kill-family shim
    // (kill_guard.rs) can DENY `kill <daemon-pid>` (a host-tier blast an agent has no legit
    // reason to issue). This runs in the daemon process, so process::id() IS the daemon.
    pin("AGEND_DAEMON_PID", std::process::id().to_string().into());
    // MED-5: AGEND_HOME is on the env-isolation SENSITIVE deny-list (so
    // `env_clear` drops it and the fleet passthrough loop never re-adds it), but
    // the spawned agent's in-pane `agend-terminal` subcommands need it to resolve
    // the daemon's home — without it they fall back to the default
    // `~/.agend-terminal`, pointing at the wrong daemon. Re-inject unconditionally
    // after the clear, exactly like AGEND_INSTANCE_NAME above.
    if let Some(h) = home {
        pin("AGEND_HOME", h.into());
        // #1504 family (t-…-65): the agentic-git shim on the agent's PATH derives
        // its own self-exclusion dir from AGENTIC_GIT_HOME, falling back to
        // AGEND_HOME. Tests that scope AGEND_HOME to a temp dir (ScopedAgendHome)
        // and then spawn `git` otherwise make the shim resolve to itself and trip
        // its recursion guard. Pin the PRIMARY name to the daemon home so the shim
        // keeps knowing where its own binary lives whatever the crate's home is
        // scoped to. Same deny-list class as AGEND_HOME, and nothing can redirect
        // it: `resolve_child_env` drops sensitive keys when it builds the isolation
        // plan, so the clear + injected vars never carry one in; this pin is
        // written after them, so by ordering it wins over anything the plan
        // injected; and `fleet_env_overrides` skips sensitive keys unless they are
        // the backend's own credential keys (#2106), which this is not.
        pin("AGENTIC_GIT_HOME", h.into());
    }
    out
}

/// User env from fleet.yaml. Drop entries on the sensitive-env deny-list
/// so a hostile template cannot override ANTHROPIC_API_KEY, LD_PRELOAD,
/// AGEND_HOME, etc. with attacker-controlled values inherited by the
/// spawned agent process.
///
/// #2106: EXCEPTION — a sensitive key is allowed through when it is a
/// credential the detected backend declares (`credential_env_keys`, the same
/// per-backend override the #1440 isolation path applies in
/// `resolve_child_env`). Operators legitimately point an instance at a
/// third-party provider (e.g. an `ANTHROPIC_BASE_URL` proxy +
/// `ANTHROPIC_AUTH_TOKEN`), and the operator's fleet.yaml is a TRUSTED source
/// under the single-machine single-user threat model (anyone who can write
/// fleet.yaml can already run arbitrary code as the user). This stays NARROW:
/// only THIS backend's own credential keys pass — linker-injection
/// (`LD_*`/`DYLD_*`), home/identity redirect (`AGEND_HOME`,
/// `AGEND_INSTANCE_NAME`), and foreign credentials (`AWS_*`, `GITHUB_TOKEN`,
/// another backend's key) are NOT in this backend's credential set, so they
/// are still dropped even from fleet.yaml. `is_sensitive_env_key` / the
/// deny-list itself are unchanged, so every OTHER enforcement point (the
/// isolation passthrough, etc.) keeps its protection.
pub(crate) fn fleet_env_overrides(
    name: &str,
    backend: Option<&Backend>,
    env: Option<&HashMap<String, String>>,
) -> Vec<(String, String)> {
    let backend_creds: &[&str] = backend.map(|b| b.credential_env_keys()).unwrap_or(&[]);
    let mut out = Vec::new();
    let Some(env_map) = env else {
        return out;
    };
    for (k, v) in env_map {
        if is_sensitive_env_key(k) {
            if env_key_in(k, backend_creds) {
                // Audit the override (key NAME only — never the secret value).
                // One line per spawn of an instance that sets a per-instance
                // credential; not a per-tick flood.
                tracing::info!(
                    instance = %name,
                    key = %k,
                    "#2106: injecting operator fleet.yaml credential override (backend-declared credential key)"
                );
            } else {
                tracing::warn!(
                    instance = %name,
                    key = %k,
                    "dropping fleet.yaml env override for sensitive key"
                );
                continue;
            }
        }
        out.push((k.clone(), v.clone()));
    }
    out
}
