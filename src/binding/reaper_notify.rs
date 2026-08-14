//! t-20260813113928166255-89366-26: notification for the daemon-startup
//! orphan reaper (`binding::reconcile_orphans`). Own module so this stays
//! clear of the 2500-LOC anti-monolith ceiling on `binding.rs` — same
//! rationale as `binding::shim_install`.
//!
//! `reconcile_orphans` used to delete a stale binding with only a
//! `tracing::info!` — invisible to the revoked instance and its team.
//! Confirmed incident: a stalled-but-alive worker (heartbeat is a "dead"
//! proxy, not a "stuck" one) lost its binding across a daemon restart and
//! stayed silently blocked for 3 real days, discovering it only when a push
//! was denied. This module turns that into a durable inbox event.
//!
//! d-20260814042446102235-10 scoped this to notification ONLY: the
//! three-condition removal judgment in `reconcile_orphans` is untouched.

use std::path::Path;

/// R1 (PR #1 review, reviewer-caught): attempt the actual `binding.json`
/// removal and only claim it happened — via the `tracing::info!` line AND
/// the revoked-instance notification — when it truly did. The pre-existing
/// `let _ = std::fs::remove_file(...)` this replaces swallowed the error and
/// proceeded unconditionally: on a permission/IO failure it logged "removed"
/// and told the recipient to `bind_self` recover a binding that was still on
/// disk — an actionable-but-false notice, worse than the silent failure this
/// PR set out to fix. A removal failure is instead surfaced via
/// `tracing::warn!` so an unremovable orphan binding is a known state, not a
/// swallowed one.
///
/// Lives here (not inline in `reconcile_orphans`) purely to keep
/// `binding.rs` under the 2500-LOC anti-monolith ceiling — `binding_index()`
/// / `index_key()` are private to `binding.rs` but visible here as this is a
/// child module.
pub(crate) fn remove_and_notify(
    home: &Path,
    agent_name: &str,
    binding_path: &Path,
    v: &serde_json::Value,
) {
    match std::fs::remove_file(binding_path) {
        Ok(()) => {
            if let Ok(mut map) = super::binding_index().write() {
                map.remove(&super::index_key(home, agent_name));
            }
            tracing::info!(
                path = %binding_path.display(),
                "removed orphan binding (>24h old, heartbeat stale)"
            );
            notify_binding_reaped(home, agent_name, v);
        }
        Err(e) => {
            tracing::warn!(
                path = %binding_path.display(),
                error = %e,
                "reconcile_orphans: failed to remove orphan binding.json — \
                 binding still bound, no notification sent"
            );
        }
    }
}

/// Notify the revoked instance AND its team orchestrator (when one resolves
/// and differs from the instance itself) that `reconcile_orphans` deleted
/// their binding. `binding` is the already-parsed `binding.json` body — the
/// caller has it in scope right before deletion, so no extra I/O is needed
/// here.
///
/// Uses [`crate::inbox::notify_system`] — NOT `notify_agent`. `notify_agent`
/// is PTY-inject-only (no durable inbox write) and is NOT observable via
/// `inbox::drain` when the target has no live pane (see binding.rs's #2347
/// test notes) — using it here would silently lose the notification in
/// exactly the no-pane/pre-agent-spawn boot window this fix exists to cover.
/// `notify_system` persists to the recipient's inbox JSONL first
/// (`storage::enqueue_returning_unread_count`, pure `home`-relative file
/// I/O) and only THEN attempts a best-effort live PTY nudge, so the
/// notification survives even though `reconcile_orphans` runs at daemon
/// startup, before the API socket binds and before any fleet agent spawns.
///
/// No fallback to a `general` operator inbox when the instance is teamless
/// (deliberate — d-20260814042446102235-10 follow-up, not an oversight): the
/// revoked instance itself is always notified, so a `general` fallback would
/// only add a recipient who cannot act on this notice (not the binding's
/// owner, doesn't know the worktree, shouldn't decide another team's
/// re-bind) — an unactionable recurring notification is a documented noise
/// source (t-20260814003031365538-20742-0) that erodes attention to the
/// whole notification class over time.
///
/// Best-effort: a failed enqueue (e.g. readonly disk) is logged and does NOT
/// roll back the binding removal that already happened.
fn notify_binding_reaped(home: &Path, agent_name: &str, binding: &serde_json::Value) {
    let task_id = binding["task_id"].as_str().unwrap_or("");
    let branch = binding["branch"].as_str().unwrap_or("");
    let worktree = binding["worktree"].as_str().unwrap_or("");
    let body = notify_body(agent_name, task_id, branch, worktree);
    let task_id_opt = (!task_id.is_empty()).then_some(task_id);

    if let Err(e) = crate::inbox::notify_system(
        home,
        agent_name,
        "system:binding_reaper",
        "binding_reaper_revoked",
        body.clone(),
        None,
        task_id_opt,
    ) {
        tracing::warn!(
            agent = agent_name,
            error = %e,
            "reconcile_orphans: failed to notify revoked instance of binding revoke"
        );
    }

    let Some(recipient) = crate::fleet::team_orchestrator_for(home, agent_name) else {
        return;
    };
    if recipient == agent_name {
        return;
    }
    if let Err(e) = crate::inbox::notify_system(
        home,
        &recipient,
        "system:binding_reaper",
        "binding_reaper_revoked",
        body,
        None,
        task_id_opt,
    ) {
        tracing::warn!(
            agent = agent_name,
            recipient = %recipient,
            error = %e,
            "reconcile_orphans: failed to notify team orchestrator of member binding revoke"
        );
    }
}

fn opt_field(v: &str) -> &str {
    if v.is_empty() {
        "(unknown)"
    } else {
        v
    }
}

fn notify_body(agent_name: &str, task_id: &str, branch: &str, worktree: &str) -> String {
    format!(
        "binding for instance `{agent_name}` was revoked by the daemon-startup orphan reaper \
         (binding issued_at > 24h ago AND heartbeat stale > 1h — #693 protection did not apply). \
         worktree=`{}` branch=`{}` task_id=`{}`. If `{agent_name}` still needs this worktree, \
         recover with `bind_self` (instance=`{agent_name}`, task_id=`{}`, branch=`{}`).",
        opt_field(worktree),
        opt_field(branch),
        opt_field(task_id),
        opt_field(task_id),
        opt_field(branch),
    )
}
