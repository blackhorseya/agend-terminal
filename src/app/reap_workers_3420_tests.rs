//! #3420 lifecycle witnesses for app-owned unmanaged-child reapers.
//!
//! Kept in a sibling `*tests*.rs` file so the production app module remains under
//! the repository's anti-monolith ceiling. `use super::*` reaches the private
//! teardown and close-path helpers owned by `app`.

use super::*;

#[cfg(unix)]
fn tmp_home(suffix: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agend-app-phase2-{}-{}",
        suffix,
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// #3420: a close-pane reap is registered with app-owned teardown state
/// SYNCHRONOUSLY, before the off-thread termination runs —
/// `kill_unmanaged_agents` pushes the reaper's `JoinHandle` into
/// `reap_workers` on the close path, so close-then-quit cannot abandon the
/// child. Written RED while the close path still dropped that handle; it now
/// pins the shipped ownership.
#[test]
fn close_pane_reap_is_registered_for_app_teardown_3420() {
    let source = include_str!("mod.rs");
    let helper_start = source
        .find("fn kill_unmanaged_agents(")
        .expect("unmanaged close reaper helper present");
    let helper_end = source[helper_start..]
        .find("\nfn kill_unmanaged_agent(")
        .map(|end| helper_start + end)
        .expect("single-agent helper follows plural helper");
    let helper = &source[helper_start..helper_end];
    assert!(
        helper.contains("reap_workers"),
        "close-pane reaper must receive app-owned JoinHandle storage"
    );
    assert!(
        helper.contains("reap_workers.push"),
        "close-pane reaper must register its JoinHandle before returning"
    );

    let overlay = include_str!("overlay.rs");
    assert!(
        overlay.contains("nonfleet_agents, &mut *ctx.reap_workers"),
        "ConfirmClose must pass the app-owned reaper collection"
    );
}

/// #3420 correction: the two teardown join groups need SEPARATE budgets.
///
/// One shared deadline made the reap join spend the attach join's window: the
/// reap runs first, `terminate_agents_parallel` carries a 2s grace
/// (`daemon::SHUTDOWN_GRACE`), and `bounded_join_attach_workers` detaches
/// immediately once the deadline has passed. A stubborn child could therefore
/// leave every attach worker detached — the abandonment this work exists to
/// prevent, moved to the other group.
///
/// Deterministic and sleepless: the allocation is a pure function of one start
/// instant, so the property is arithmetic rather than timing.
#[test]
fn teardown_gives_each_join_group_its_own_budget_3420() {
    let start = std::time::Instant::now();
    let (reap_deadline, attach_deadline) = teardown_join_deadlines(start);

    assert_eq!(
        reap_deadline - start,
        REAP_JOIN_BUDGET,
        "the reap join gets its own budget"
    );
    assert_eq!(
        attach_deadline - reap_deadline,
        ATTACH_JOIN_BUDGET,
        "the attach join's budget must sit AFTER the reap budget, not inside it"
    );
}

/// The non-vacuity control the shared-deadline bug would have failed: with the
/// reap budget FULLY exhausted, the attach group must still have its entire
/// window left. Under one shared deadline this difference is zero or negative.
#[test]
fn an_exhausted_reap_budget_leaves_the_attach_budget_intact_3420() {
    let start = std::time::Instant::now();
    let (reap_deadline, attach_deadline) = teardown_join_deadlines(start);

    // Stand at the instant the reap join has consumed every last microsecond.
    let after_exhausted_reap = reap_deadline;
    assert!(
        attach_deadline > after_exhausted_reap,
        "a fully spent reap budget must not leave the attach join with an expired deadline"
    );
    assert_eq!(
        attach_deadline - after_exhausted_reap,
        ATTACH_JOIN_BUDGET,
        "the attach join must still have its WHOLE budget after the reap budget is spent"
    );
}

/// The production body of `app_teardown`, and nothing after it.
///
/// `app_teardown` is the last production item before
/// `fn is_text_composing_input`, so that is where the slice ends. Reading to
/// EOF instead would pull this whole test module in, and every needle below
/// would then match its own source — a guard that can be satisfied by its
/// own text is not reading the production it claims to pin. The two wiring
/// guards below slice through this helper for that reason, and
/// `teardown_wiring_guards_must_not_read_their_own_source_3420` (written RED
/// against the earlier end-of-file slice) keeps the bound honest.
fn teardown_production_body(source: &str) -> &str {
    let start = source
        .find("fn app_teardown(")
        .expect("app_teardown present");
    let rest = &source[start..];
    let end = rest
        .find("\nfn is_text_composing_input(")
        .expect("app_teardown must remain the item before is_text_composing_input");
    &rest[..end]
}

/// The searched call, assembled at runtime so this module's own source never
/// contains it as a literal. Without this, bounding the slice alone would
/// still leave a guard that a future in-module string could satisfy.
fn join_call(group: &str, deadline: &str) -> String {
    format!(
        "{}({group}, {deadline})",
        ["bounded_join", "_attach_workers"].concat()
    )
}

#[test]
fn teardown_wiring_guards_must_not_read_their_own_source_3420() {
    let source = include_str!("mod.rs");
    let body = teardown_production_body(source);
    let cfg_test = ["#[cfg(", "test)]"].concat();
    assert!(
        !body.contains(&cfg_test),
        "the wiring guards' slice reaches this test module, so their needles \
         can be satisfied by their own source instead of by production"
    );
}

/// Both groups are joined against the deadline meant for them, not one of
/// them twice.
#[test]
fn teardown_joins_each_group_against_its_own_deadline_3420() {
    let source = include_str!("mod.rs");
    let body = teardown_production_body(source);
    let reap_call = body
        .find(&join_call("reap_workers", "reap_deadline"))
        .expect("the reap join must use the reap deadline");
    let attach_call = body
        .find(&join_call("attach_workers", "attach_deadline"))
        .expect("the attach join must use the attach deadline");
    assert!(
        reap_call < attach_call,
        "reapers still join first; only their budgets are separated"
    );
}

/// #3420: close-then-quit drains unmanaged reapers BEFORE attach workers,
/// each group against its own deadline (`teardown_join_deadlines`). Written
/// RED while teardown joined only attach workers; it now pins the shipped
/// order, and `teardown_joins_each_group_against_its_own_deadline_3420` pins
/// the budgets.
#[test]
fn app_teardown_joins_reapers_before_attach_workers_3420() {
    let source = include_str!("mod.rs");
    let body = teardown_production_body(source);
    let joiner = ["bounded_join", "_attach_workers"].concat();
    let reap = body
        .find(&format!("{joiner}(reap_workers"))
        .expect("teardown must join unmanaged reaper handles");
    let attach = body
        .find(&format!("{joiner}(attach_workers"))
        .expect("teardown must join attach workers");
    assert!(
        reap < attach,
        "unmanaged child reapers must join before attach workers"
    );
}

/// #3420: exercise the production unmanaged-close path with a real
/// ChildHandle fixture, then quit immediately. Teardown must consume the
/// registered reaper before returning, leaving the child handle reaped.
#[cfg(unix)]
#[test]
fn close_then_quit_reaps_unmanaged_child_before_return_3420() {
    let home = tmp_home("close_then_quit_3420");
    let id = crate::types::InstanceId::new();
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    registry.lock().insert(
        id,
        crate::agent::mk_sigterm_immune_test_handle("scratch-3420", id),
    );
    let child = Arc::clone(&registry.lock().get(&id).expect("fixture registered").child);
    let mut reap_workers = Vec::new();

    // Let the shell install its signal traps before anything signals it. The
    // reaper fires stage-1 SIGTERM within a millisecond of the close, and a
    // signal that lands mid-exec takes the DEFAULT action — the trap has not
    // been installed yet. A real pane has always been running for a while by
    // the time the operator closes it.
    std::thread::sleep(std::time::Duration::from_millis(250));

    kill_unmanaged_agents(&registry, [id], &mut reap_workers);
    assert_eq!(
        reap_workers.len(),
        1,
        "close must register its reaper handle"
    );
    assert!(
        registry.lock().get(&id).is_none(),
        "close must remove the unmanaged registry entry before handoff"
    );

    // NON-VACUITY PRECONDITION. The final assertion below only distinguishes
    // a teardown that JOINED the reaper from one that detached it if the
    // child is still alive when teardown starts. `terminate_agents_parallel`
    // sleeps `SHUTDOWN_GRACE` before it reaps anything, so a child that
    // outlives this window can only be dead afterwards because teardown
    // waited. A child that exits on its OWN is reaped by the final
    // `try_wait` no matter what teardown did, and the test would pass
    // against a detaching teardown.
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        matches!(child.lock().try_wait(), Ok(None)),
        "fixture child must still be running when teardown begins, or the \
         reap assertion below cannot tell a joined teardown from a detached one"
    );

    app_teardown(&home, &Layout::new(), reap_workers, Vec::new());
    assert!(
        matches!(child.lock().try_wait(), Ok(Some(_))),
        "close-then-quit must reap the real unmanaged child before teardown returns"
    );
    std::fs::remove_dir_all(home).ok();
}

/// Unmanaged-close respawn witness: closing a scratch/overlay shell must NOT
/// leave the PTY-close reaper free to run its clean-exit shell fallback.
///
/// `kill_unmanaged_agents` evicts the exact registry entry and then
/// terminates the child, but (unlike the daemon DELETE path, which stores
/// `deleted = true` before it kills) it never marks the handle deleted. The
/// reaper thread `spawn_agent` started for the shell then sees: shutdown flag
/// false, `wait_for_process_exit` -> entry gone -> `Some(0)` (UserExit),
/// `is_startup_failure` -> false, and `on_clean_exit_shell_fallback` spawns a
/// brand-new same-name shell (home=None mints a fresh UUID) that no pane owns.
///
/// The fixture mirrors `pane_factory`'s `UnmanagedLocalShell`: home=None,
/// crash_tx=None, shutdown flag present and false (a private flag, so a
/// sibling test's `app_teardown` cannot flip it and make this vacuous).
#[cfg(unix)]
#[test]
fn closing_unmanaged_shell_does_not_respawn_orphan_shell() {
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let name = "scratch-unmanaged-respawn";
    let id = agent::spawn_agent(
        &agent::SpawnConfig {
            name,
            backend: None,
            backend_command: crate::default_shell(),
            args: &[],
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 80,
            rows: 24,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: None,
            crash_tx: None,
            shutdown: Some(Arc::clone(&shutdown)),
        },
        &registry,
    )
    .expect("spawn unmanaged shell fixture");

    // A real pane has been running for a while when the operator closes it.
    std::thread::sleep(std::time::Duration::from_millis(500));

    let mut reap_workers = Vec::new();
    kill_unmanaged_agents(&registry, [id], &mut reap_workers);

    // Poll for a same-name entry under a DIFFERENT id (the orphan fallback).
    let mut orphan_ids = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        orphan_ids = agent::lock_registry(&registry)
            .iter()
            .filter(|(k, h)| **k != id && &*h.name == name)
            .map(|(k, _)| *k)
            .collect();
        if !orphan_ids.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // Cleanup FIRST (before asserting) so a failing run leaks no child: mark
    // every same-name handle deleted so no further fallback chain starts,
    // then evict and terminate them.
    let leftovers: Vec<_> = agent::lock_registry(&registry)
        .iter()
        .filter(|(_, h)| &*h.name == name)
        .map(|(k, h)| {
            h.deleted.store(true, std::sync::atomic::Ordering::SeqCst);
            *k
        })
        .collect();
    kill_unmanaged_agents(&registry, leftovers, &mut reap_workers);
    for worker in reap_workers {
        worker.join().ok();
    }

    assert!(
        orphan_ids.is_empty(),
        "closing an unmanaged shell must not let the reaper respawn a same-name \
         orphan shell; found orphan registry ids: {orphan_ids:?}"
    );
}
