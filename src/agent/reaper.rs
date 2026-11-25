//! PTY-reaper helpers: generation-scoped registry access for `handle_pty_close`.
//!
//! A restart re-registers a successor handle under the SAME fleet `InstanceId`,
//! so every registry query the reaper makes must be scoped to the
//! `(id, generation)` of the handle it is reaping. Pure code motion out of
//! `agent/mod.rs` (anti-monolith ceiling); behaviour is unchanged.

#[cfg(unix)]
use super::write_actor;
use super::{crash_disposition, lock_registry, AgentRegistry};

/// Sprint 21 F-NEW1: kill the process tree rooted at the agent's child PID,
/// if still registered. Looks up the PID through the registry → child mutex
/// chain, then delegates to [`crate::process::kill_process_tree`] (PR-U #158).
///
/// No-op if the agent is not in the registry (already cleaned up) or if the
/// child has no live PID (already reaped). Idempotent on dead PIDs — safe to
/// call multiple times during the same crash-detection sequence.
///
/// Generation-scoped: only the handle whose `generation` matches is touched. A
/// restart re-registers a successor under the same id, and a stale reaper must
/// never kill the successor's process tree.
pub(super) fn sweep_child_tree(
    id: &crate::types::InstanceId,
    registry: &AgentRegistry,
    generation: crash_disposition::SpawnGeneration,
) {
    let pid: Option<u32> = {
        let reg = lock_registry(registry);
        reg.get(id)
            .filter(|h| h.generation == generation)
            .and_then(|h| h.child.lock().process_id())
    };
    if let Some(pid) = pid {
        crate::process::kill_process_tree(pid);
    }
}

/// Outcome of [`wait_for_process_exit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExitWait {
    /// The reaped handle's child exited with this code.
    Exited(i32),
    /// The child did not exit within the wait window (same semantics as the
    /// former `None`: classified as a signal kill).
    NoExit,
    /// The registry no longer holds THIS `(id, generation)` — it was removed or
    /// replaced by a successor. Someone else owns the entry now, so the reaper
    /// must not sweep, classify, fall back, or clean up.
    Evicted,
}

pub(super) fn wait_for_process_exit(
    name: &str,
    id: &crate::types::InstanceId,
    registry: &AgentRegistry,
    generation: crash_disposition::SpawnGeneration,
) -> ExitWait {
    for _ in 0..20 {
        let reg = lock_registry(registry);
        let Some(handle) = reg.get(id).filter(|h| h.generation == generation) else {
            tracing::debug!(
                agent = name,
                "entry removed or replaced, skipping crash handling"
            );
            return ExitWait::Evicted;
        };
        {
            let mut c = handle.child.lock();
            if let Ok(Some(status)) = c.try_wait() {
                return ExitWait::Exited(status.exit_code() as i32);
            }
        }
        drop(reg);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    ExitWait::NoExit
}

/// Remove `id` from the registry only if it still holds the handle of
/// `generation` (check + remove under one registry lock). Returns whether this
/// call removed it. A successor under the same id is left untouched.
pub(super) fn remove_owned_and_unregister(
    registry: &AgentRegistry,
    id: &crate::types::InstanceId,
    generation: crash_disposition::SpawnGeneration,
) -> bool {
    let removed = {
        let mut reg = lock_registry(registry);
        if reg.get(id).is_some_and(|h| h.generation == generation) {
            reg.remove(id)
        } else {
            None
        }
    };
    #[cfg(unix)]
    if let Some(handle) = &removed {
        write_actor::unregister(&handle.pty_writer);
    }
    removed.is_some()
}

/// Delete the name-keyed IPC port file only while the id is still vacant, with
/// the registry lock held across the check and the unlink. A successor publishes
/// its port only after it is registered (which needs this same lock), so either
/// we observe it and leave its port alone, or we unlink first and it republishes
/// afterwards. The unlink is a leaf syscall; holding the registry lock for it
/// adds no lock edge.
pub(super) fn remove_port_if_vacant(
    name: &str,
    id: &crate::types::InstanceId,
    registry: &AgentRegistry,
    home: &Option<std::path::PathBuf>,
) {
    let Some(ref home) = home else { return };
    let reg = lock_registry(registry);
    if !reg.contains_key(id) {
        crate::ipc::remove_port(&crate::daemon::run_dir(home), name);
    }
}

/// Generation-scoped cleanup. Returns `false` (and touches nothing, including
/// the IPC port file keyed by name) when the entry is no longer ours.
pub(super) fn cleanup_agent(
    name: &str,
    id: &crate::types::InstanceId,
    registry: &AgentRegistry,
    home: &Option<std::path::PathBuf>,
    generation: crash_disposition::SpawnGeneration,
) -> bool {
    if !remove_owned_and_unregister(registry, id, generation) {
        return false;
    }
    remove_port_if_vacant(name, id, registry, home);
    true
}

/// Pure startup-grace decision, split out of [`is_startup_failure`] so it is
/// unit-testable without a live registry / PTY-backed `AgentHandle` (which can't
/// be constructed in a unit test). An exit is a startup FAILURE — meaning NO
/// shell fallback — when the agent has been up < 5s AND no user input arrived
/// since it was spawned. `uptime` is `(elapsed, spawned_at_epoch_ms)` for the
/// agent, or `None` when it is absent from the registry.
pub(super) fn startup_failure_from(
    uptime: Option<(std::time::Duration, u64)>,
    last_input_at_ms: u64,
) -> bool {
    match uptime {
        // Input at/after the spawn epoch is a real user session → not a startup
        // failure. Input timestamped before the spawn epoch is stale (a prior
        // session) and must NOT suppress the startup-failure path.
        Some((elapsed, spawned_at_epoch_ms)) => {
            elapsed < std::time::Duration::from_secs(5) && last_input_at_ms < spawned_at_epoch_ms
        }
        None => false,
    }
}

pub(super) fn is_startup_failure(
    name: &str,
    id: &crate::types::InstanceId,
    registry: &AgentRegistry,
    generation: crash_disposition::SpawnGeneration,
) -> bool {
    let uptime = {
        let reg = lock_registry(registry);
        reg.get(id)
            .filter(|h| h.generation == generation)
            .map(|h| (h.spawned_at.elapsed(), h.spawned_at_epoch_ms))
    };
    let last_input_at_ms = crate::daemon::heartbeat_pair::snapshot_for(name).last_input_at_ms;
    startup_failure_from(uptime, last_input_at_ms)
}
