use super::*;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

fn close_test_pane(
    id: crate::types::InstanceId,
    name: &str,
    fleet_name: Option<&str>,
) -> crate::layout::Pane {
    crate::layout::Pane {
        agent_name: name.into(),
        instance_id: id,
        instance_ref: None,
        vterm: crate::vterm::VTerm::new(10, 10),
        rx: crossbeam_channel::bounded(1).1,
        id: 1,
        backend: None,
        working_dir: None,
        display_name: None,
        restart_error: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: fleet_name.map(str::to_string),
        last_input_at: None,
        pending_notification_count: 0,
        pending_decision_count: 0,
        selection: None,
        source: crate::layout::PaneSource::Local,
        offthread: None,
        _fwd_cancel: None,
    }
}

fn confirm_close_tab(
    home: &Path,
    registry: &crate::agent::AgentRegistry,
    pane: crate::layout::Pane,
) -> crate::layout::Layout {
    let mut layout = crate::layout::Layout::new();
    layout.add_tab(crate::layout::Tab::new("close-test".to_string(), pane));
    let (wakeup_tx, _wakeup_rx) = crossbeam_channel::unbounded();
    let mut name_counter = HashMap::new();
    let mut reap_workers = Vec::new();
    let mut overlay = Overlay::ConfirmClose {
        target: CloseTarget::Tab,
    };
    let mut ctx = OverlayCtx {
        layout: &mut layout,
        registry,
        home,
        fleet_path: home,
        wakeup_tx: &wakeup_tx,
        name_counter: &mut name_counter,
        task_rpc_tx: &test_task_channel().0,
        restart_request_tx: None,
        reap_workers: &mut reap_workers,
    };
    handle_key(&mut overlay, press(KeyCode::Char('y')), &mut ctx);
    layout
}

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::empty())
}

/// Drive one key through `handle_key` against an empty layout.
/// Returns (tabs after, overlay after).
fn menu_key(overlay: Overlay, code: KeyCode, tag: &str) -> (usize, Overlay) {
    let home = dec_home(tag);
    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let mut layout = crate::layout::Layout::new();
    let (wakeup_tx, _wakeup_rx) = crossbeam_channel::unbounded();
    let mut name_counter = HashMap::new();
    let mut reap_workers = Vec::new();
    let mut overlay = overlay;
    let mut ctx = OverlayCtx {
        layout: &mut layout,
        registry: &registry,
        home: &home,
        fleet_path: &home,
        wakeup_tx: &wakeup_tx,
        name_counter: &mut name_counter,
        task_rpc_tx: &test_task_channel().0,
        restart_request_tx: None,
        reap_workers: &mut reap_workers,
    };
    handle_key(&mut overlay, press(code), &mut ctx);
    let tabs = layout.tabs.len();
    std::fs::remove_dir_all(home).ok();
    (tabs, overlay)
}

fn backend_item() -> MenuItem {
    MenuItem {
        label: "[backend] claude".to_string(),
        kind: MenuItemKind::Backend(Backend::ClaudeCode),
    }
}

#[test]
fn creating_menu_items_need_confirmation_but_views_do_not() {
    assert!(creates_new_instance(&MenuItemKind::Backend(
        Backend::ClaudeCode
    )));
    assert!(creates_new_instance(&MenuItemKind::Fugu));
    assert!(!creates_new_instance(&MenuItemKind::Shell));
    assert!(!creates_new_instance(&MenuItemKind::FleetInstance(
        "x".to_string()
    )));
}

/// A single Enter on a backend entry must NOT create an instance: it only
/// asks for confirmation (the stray-Enter phantom-create path).
#[test]
fn split_menu_enter_on_backend_asks_instead_of_creating() {
    let menu = Overlay::SplitMenu {
        items: vec![backend_item()],
        selected: 0,
        dir: SplitDir::Vertical,
    };
    let (tabs, overlay) = menu_key(menu, KeyCode::Enter, "split_enter_confirm");
    assert!(
        matches!(overlay, Overlay::ConfirmCreate { .. }),
        "Enter must open the create confirmation"
    );
    assert_eq!(tabs, 0);
}

#[test]
fn new_tab_menu_enter_on_backend_asks_instead_of_creating() {
    let menu = Overlay::NewTabMenu {
        items: vec![backend_item()],
        selected: 0,
    };
    let (_, overlay) = menu_key(menu, KeyCode::Enter, "tab_enter_confirm");
    assert!(matches!(overlay, Overlay::ConfirmCreate { .. }));
}

/// Anything but `y` cancels (Enter included), so a second stray Enter or
/// typed text can never confirm creation.
#[test]
fn confirm_create_cancels_on_enter_and_other_keys() {
    for code in [
        KeyCode::Enter,
        KeyCode::Char('n'),
        KeyCode::Esc,
        KeyCode::Char('j'),
    ] {
        let ov = Overlay::ConfirmCreate {
            item: backend_item(),
            target: MenuTarget::Tab,
        };
        let (tabs, overlay) = menu_key(ov, code, "confirm_create_cancel");
        assert!(matches!(overlay, Overlay::None), "{code:?} must close it");
        assert_eq!(tabs, 0, "{code:?} must not create anything");
    }
}

/// How many times `create_instance` RPC was entered for each key, sent
/// to a fresh `ConfirmCreate` (real `handle_key`).
fn create_calls_for(events: &[KeyEvent], tag: &str) -> Vec<usize> {
    let home = dec_home(tag);
    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let mut layout = crate::layout::Layout::new();
    let (wakeup_tx, _wakeup_rx) = crossbeam_channel::unbounded();
    let mut name_counter = HashMap::new();
    let mut reap_workers = Vec::new();
    let mut ctx = OverlayCtx {
        layout: &mut layout,
        registry: &registry,
        home: &home,
        fleet_path: &home,
        wakeup_tx: &wakeup_tx,
        name_counter: &mut name_counter,
        task_rpc_tx: &test_task_channel().0,
        restart_request_tx: None,
        reap_workers: &mut reap_workers,
    };
    let mut observed = Vec::new();
    for event in events {
        super::super::rpc::CREATE_INSTANCE_CALLS.with(|c| c.set(0));
        let mut overlay = Overlay::ConfirmCreate {
            item: backend_item(),
            target: MenuTarget::Tab,
        };
        handle_key(&mut overlay, *event, &mut ctx);
        assert!(matches!(overlay, Overlay::None), "{event:?} must close it");
        observed.push(super::super::rpc::CREATE_INSTANCE_CALLS.with(|c| c.get()));
    }
    std::fs::remove_dir_all(&home).ok();
    observed
}

/// Only an UNMODIFIED `y` reaches the `create_instance` RPC. Ctrl/Alt/Super+Y
/// (and Enter) cancel. Shift+Y / `Y` also cancel: the prompt asks for `y`.
#[test]
fn confirm_create_only_plain_y_calls_create_instance() {
    let ev = |code, m| KeyEvent::new(code, m);
    let events = [
        ev(KeyCode::Char('y'), KeyModifiers::CONTROL),
        ev(KeyCode::Char('y'), KeyModifiers::ALT),
        ev(KeyCode::Char('y'), KeyModifiers::SUPER),
        ev(KeyCode::Char('y'), KeyModifiers::SHIFT),
        ev(KeyCode::Char('Y'), KeyModifiers::empty()),
        ev(KeyCode::Char('y'), KeyModifiers::empty()),
        ev(KeyCode::Enter, KeyModifiers::empty()),
    ];
    assert_eq!(
        create_calls_for(&events, "confirm_create_modifiers"),
        [0, 0, 0, 0, 0, 1, 0],
        "only plain `y` may call create_instance"
    );
}

#[test]
fn confirm_close_reaps_unmanaged_shell_by_instance_id() {
    let home = dec_home("confirm_close_unmanaged");
    let id = crate::types::InstanceId::new();
    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    registry
        .lock()
        .insert(id, crate::agent::mk_test_handle("scratch", id));

    let started = std::time::Instant::now();
    let layout = confirm_close_tab(&home, &registry, close_test_pane(id, "scratch", None));

    assert!(layout.tabs.is_empty(), "the tab view should close");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "closing a shell view must not wait for the 2s child-termination grace window"
    );
    assert!(
        !registry.lock().contains_key(&id),
        "the unmanaged registry entry must be removed and reaped"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn confirm_close_preserves_managed_fleet_state() {
    let home = dec_home("confirm_close_fleet");
    let id = crate::types::InstanceId::new();
    let fleet_path = crate::fleet::fleet_yaml_path(&home);
    let fleet_bytes = format!(
        "instances:\n  managed:\n    id: '{}'\n    backend: claude\n",
        id
    );
    std::fs::write(&fleet_path, &fleet_bytes).expect("write fleet fixture");
    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    registry
        .lock()
        .insert(id, crate::agent::mk_test_handle("managed", id));

    let layout = confirm_close_tab(
        &home,
        &registry,
        close_test_pane(id, "managed", Some("managed")),
    );

    assert!(layout.tabs.is_empty(), "the tab view should close");
    // The incident path used a fire-and-forget delete thread. Poll across
    // a bounded window so an asynchronous regression cannot pass merely
    // because the assertions raced ahead of the destructive worker.
    for _ in 0..20 {
        assert!(
            registry.lock().contains_key(&id),
            "closing a fleet view must not remove its managed process"
        );
        assert_eq!(
            std::fs::read(&fleet_path).expect("fleet still exists"),
            fleet_bytes.as_bytes(),
            "closing a fleet view must not mutate fleet.yaml"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let handle = crate::agent::remove_and_unregister(&registry, &id).expect("cleanup handle");
    crate::daemon::terminate_agents_parallel(vec![("managed".to_string(), handle.child)]);
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn delete_instance_requires_exact_name_before_lifecycle() {
    let home = dec_home("delete_exact_name");
    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (wakeup_tx, _wakeup_rx) = crossbeam_channel::unbounded();
    let mut layout = crate::layout::Layout::new();
    layout.add_tab(crate::layout::Tab::new(
        "managed".to_string(),
        close_test_pane(
            crate::types::InstanceId::default(),
            "managed",
            Some("managed"),
        ),
    ));
    let mut name_counter = HashMap::new();
    let mut reap_workers = Vec::new();
    let mut overlay = Overlay::ConfirmDeleteInstance {
        name: "managed".to_string(),
        input: "manage".to_string(),
        notice: None,
    };
    let task_rpc_tx = test_task_channel().0.clone();
    let mut ctx = OverlayCtx {
        layout: &mut layout,
        registry: &registry,
        home: &home,
        fleet_path: &home,
        wakeup_tx: &wakeup_tx,
        name_counter: &mut name_counter,
        task_rpc_tx: &task_rpc_tx,
        restart_request_tx: None,
        reap_workers: &mut reap_workers,
    };

    handle_key(&mut overlay, press(KeyCode::Enter), &mut ctx);
    assert!(matches!(
        overlay,
        Overlay::ConfirmDeleteInstance {
            name,
            input,
            notice: Some(notice),
        } if name == "managed"
            && input == "manage"
            && notice.contains("type exact name")
    ));
    assert_eq!(layout.tabs.len(), 1, "mismatch must preserve all views");
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn delete_persistence_failure_keeps_explicit_retry_state() {
    let home = dec_home("delete_persistence_retry");
    let expected_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 91);
    let mut pane = close_test_pane(crate::types::InstanceId::new(), "managed", Some("managed"));
    pane.instance_ref = Some(expected_ref);
    let mut layout = crate::layout::Layout::new();
    layout.add_tab(crate::layout::Tab::new("managed".to_string(), pane));
    assert!(super::super::session::save_session(&home, &layout));
    assert!(layout.remove_fleet_instance_views_exact(expected_ref));
    crate::store::fail_next_atomic_write_for_test(&home.join("session.json"));
    crate::store::fail_next_atomic_write_for_test(&home.join("session.retired.json"));

    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (wakeup_tx, _wakeup_rx) = crossbeam_channel::unbounded();
    let mut name_counter = HashMap::new();
    let mut reap_workers = Vec::new();
    let task_rpc_tx = test_task_channel().0.clone();
    let mut ctx = OverlayCtx {
        layout: &mut layout,
        registry: &registry,
        home: &home,
        fleet_path: &home,
        wakeup_tx: &wakeup_tx,
        name_counter: &mut name_counter,
        task_rpc_tx: &task_rpc_tx,
        restart_request_tx: None,
        reap_workers: &mut reap_workers,
    };
    let overlay = Overlay::ConfirmDeleteInstance {
        name: "managed".to_string(),
        input: "managed".to_string(),
        notice: None,
    };
    let completion = finish_successful_delete(&mut ctx, "managed", Some(expected_ref));

    assert!(
        matches!(completion, DeleteCompletion::Keep { notice } if notice.contains("pending")),
        "failed persistence must leave an explicit retry state"
    );
    assert!(matches!(overlay, Overlay::ConfirmDeleteInstance { .. }));
    assert!(home.join("session.json").exists());
    crate::store::fail_next_atomic_write_for_test(&home.join("session.json"));
    assert!(!super::super::session::save_session(&home, ctx.layout));
    let retired: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.join("session.retired.json"))
            .expect("retry must persist retired identity"),
    )
    .expect("valid retired session JSON");
    assert_eq!(
        retired["instance_refs"]
            .as_array()
            .expect("instance_refs array")
            .len(),
        1
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn delete_without_authoritative_identity_fails_closed() {
    let home = dec_home("delete_missing_identity");
    let sentinel = br#"{"active_tab":0,"tabs":[]}"#;
    std::fs::write(home.join("session.json"), sentinel).expect("write session sentinel");
    let mut layout = crate::layout::Layout::new();
    layout.add_tab(crate::layout::Tab::new(
        "managed".to_string(),
        close_test_pane(crate::types::InstanceId::new(), "managed", Some("managed")),
    ));
    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (wakeup_tx, _wakeup_rx) = crossbeam_channel::unbounded();
    let mut name_counter = HashMap::new();
    let mut reap_workers = Vec::new();
    let task_rpc_tx = test_task_channel().0.clone();
    let mut ctx = OverlayCtx {
        layout: &mut layout,
        registry: &registry,
        home: &home,
        fleet_path: &home,
        wakeup_tx: &wakeup_tx,
        name_counter: &mut name_counter,
        task_rpc_tx: &task_rpc_tx,
        restart_request_tx: None,
        reap_workers: &mut reap_workers,
    };
    let overlay = Overlay::ConfirmDeleteInstance {
        name: "managed".to_string(),
        input: "managed".to_string(),
        notice: None,
    };
    let completion = finish_successful_delete(&mut ctx, "managed", None);

    assert!(matches!(
        completion,
        DeleteCompletion::Keep { notice } if notice.contains("identity")
    ));
    assert!(matches!(overlay, Overlay::ConfirmDeleteInstance { .. }));
    assert_eq!(
        std::fs::read(home.join("session.json")).expect("session sentinel"),
        sentinel
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn delete_instance_without_active_daemon_is_fail_closed() {
    let home = dec_home("delete_no_daemon");
    let id = crate::types::InstanceId::new();
    let fleet_path = crate::fleet::fleet_yaml_path(&home);
    let fleet_bytes = format!(
        "instances:\n  managed:\n    id: '{}'\n    backend: claude\n",
        id
    );
    std::fs::write(&fleet_path, &fleet_bytes).expect("write fleet fixture");
    let workspace = crate::paths::workspace_dir(&home).join("managed");
    std::fs::create_dir_all(&workspace).expect("create workspace fixture");
    std::fs::write(workspace.join("keep.txt"), b"keep").expect("write workspace fixture");
    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (wakeup_tx, _wakeup_rx) = crossbeam_channel::unbounded();
    let mut layout = crate::layout::Layout::new();
    layout.add_tab(Tab::new(
        "managed".to_string(),
        close_test_pane(id, "managed", Some("managed")),
    ));
    let mut name_counter = HashMap::new();
    let mut reap_workers = Vec::new();
    let task_rpc_tx = test_task_channel().0.clone();
    let mut overlay = Overlay::ConfirmDeleteInstance {
        name: "managed".to_string(),
        input: "managed".to_string(),
        notice: None,
    };
    let mut ctx = OverlayCtx {
        layout: &mut layout,
        registry: &registry,
        home: &home,
        fleet_path: &home,
        wakeup_tx: &wakeup_tx,
        name_counter: &mut name_counter,
        task_rpc_tx: &task_rpc_tx,
        restart_request_tx: None,
        reap_workers: &mut reap_workers,
    };

    handle_key(&mut overlay, press(KeyCode::Enter), &mut ctx);
    assert!(matches!(
        overlay,
        Overlay::ConfirmDeleteInstance {
            notice: Some(notice), ..
        } if notice.contains("active daemon")
    ));
    assert_eq!(
        std::fs::read(&fleet_path).expect("fleet remains"),
        fleet_bytes.as_bytes()
    );
    assert!(workspace.join("keep.txt").exists(), "workspace remains");
    assert_eq!(layout.tabs.len(), 1, "view remains");
    assert!(
        layout.tabs[0]
            .root()
            .first_pane()
            .fleet_instance_name
            .as_deref()
            == Some("managed")
    );

    std::fs::remove_dir_all(home).ok();
}

#[test]
fn delete_instance_error_preserves_every_exact_name_view() {
    let home = dec_home("delete_error_preserves_views");
    let id = crate::types::InstanceId::new();
    let run = home.join("run").join(std::process::id().to_string());
    std::fs::create_dir_all(&run).expect("create active-run fixture");
    crate::daemon::write_daemon_id(&run);
    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (wakeup_tx, _wakeup_rx) = crossbeam_channel::unbounded();
    let mut layout = crate::layout::Layout::new();
    layout.add_tab(Tab::new(
        "first".to_string(),
        close_test_pane(id, "managed", Some("managed")),
    ));
    layout.add_tab(Tab::new(
        "second".to_string(),
        close_test_pane(id, "managed", Some("managed")),
    ));
    let mut name_counter = HashMap::new();
    let mut reap_workers = Vec::new();
    let task_rpc_tx = test_task_channel().0.clone();
    let mut overlay = Overlay::ConfirmDeleteInstance {
        name: "managed".to_string(),
        input: "managed".to_string(),
        notice: None,
    };
    let mut ctx = OverlayCtx {
        layout: &mut layout,
        registry: &registry,
        home: &home,
        fleet_path: &home,
        wakeup_tx: &wakeup_tx,
        name_counter: &mut name_counter,
        task_rpc_tx: &task_rpc_tx,
        restart_request_tx: None,
        reap_workers: &mut reap_workers,
    };

    handle_key(&mut overlay, press(KeyCode::Enter), &mut ctx);
    assert!(matches!(
        overlay,
        Overlay::ConfirmDeleteInstance {
            notice: Some(_),
            ..
        }
    ));
    assert_eq!(layout.tabs.len(), 2, "deletion error preserves every tab");
    assert_eq!(
        layout
            .tabs
            .iter()
            .flat_map(|tab| {
                tab.root()
                    .pane_ids()
                    .into_iter()
                    .filter_map(|id| tab.root().find_pane(id))
            })
            .filter_map(|pane| pane.fleet_instance_name.as_deref())
            .filter(|name| *name == "managed")
            .count(),
        2
    );

    std::fs::remove_dir_all(home).ok();
}

#[test]
fn delete_instance_removes_current_ref_when_old_name_only_view_precedes_it_3625() {
    let home = dec_home("delete_replacement_view");
    let current_id = crate::types::InstanceId::new();
    let current_ref = crate::types::InstanceRef::new(current_id, 77);

    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (wakeup_tx, _wakeup_rx) = crossbeam_channel::unbounded();
    let mut old = close_test_pane(crate::types::InstanceId::new(), "managed", Some("managed"));
    old.instance_ref = None;
    let mut replacement = close_test_pane(current_id, "managed", Some("managed"));
    replacement.id = 2;
    replacement.instance_ref = Some(current_ref);
    let mut layout = crate::layout::Layout::new();
    layout.add_tab(Tab::new("old".to_string(), old));
    layout.add_tab(Tab::new("replacement".to_string(), replacement));
    let mut name_counter = HashMap::new();
    let mut reap_workers = Vec::new();
    let task_rpc_tx = test_task_channel().0.clone();
    let ctx = OverlayCtx {
        layout: &mut layout,
        registry: &registry,
        home: &home,
        fleet_path: &home,
        wakeup_tx: &wakeup_tx,
        name_counter: &mut name_counter,
        task_rpc_tx: &task_rpc_tx,
        restart_request_tx: None,
        reap_workers: &mut reap_workers,
    };

    let expected = authoritative_instance_ref_for_delete(&ctx, "managed");
    assert_eq!(expected, Some(current_ref));
    let expected = match expected {
        Some(expected) => expected,
        None => panic!("current instance ref should be authoritative"),
    };
    assert!(ctx.layout.remove_fleet_instance_views_exact(expected));
    assert!(
        ctx.layout.find_pane_mut(1).is_some(),
        "old scrollback remains"
    );
    assert!(
        ctx.layout.find_pane_mut(2).is_none(),
        "current view removed"
    );
    std::fs::remove_dir_all(home).ok();
}

fn task_overlay() -> Overlay {
    Overlay::Tasks {
        items: Vec::new(),
        col: 0,
        row: 0,
        mode: TaskBoardMode::Board,
        view: BoardView::Tasks,
        pending: false,
        notice: None,
    }
}

fn test_task_channel() -> &'static (
    crossbeam_channel::Sender<crate::app::rpc::TaskRequest>,
    crossbeam_channel::Receiver<crate::app::rpc::TaskRequest>,
) {
    static CHANNEL: std::sync::OnceLock<(
        crossbeam_channel::Sender<crate::app::rpc::TaskRequest>,
        crossbeam_channel::Receiver<crate::app::rpc::TaskRequest>,
    )> = std::sync::OnceLock::new();
    CHANNEL.get_or_init(crossbeam_channel::unbounded)
}

fn get_mode(overlay: &Overlay) -> &TaskBoardMode {
    match overlay {
        Overlay::Tasks { mode, .. } => mode,
        _ => panic!("expected Tasks overlay"),
    }
}

#[test]
fn task_write_failure_is_visible_and_does_not_mark_pending() {
    let (tx, rx) = crossbeam_channel::bounded(1);
    drop(rx);
    let mut pending = false;
    let mut notice = None;

    assert!(!submit_task_request(
        &tx,
        serde_json::json!({"action": "done", "id": "t-1"}),
        &mut pending,
        &mut notice,
    ));
    assert!(!pending);
    assert_eq!(notice.as_deref(), Some("task RPC worker stopped"));
}

/// #2305: a per-test temp home (process+tag unique — no env, no cross-test
/// collision; decisions are posted with an explicit `home`).
fn dec_home(tag: &str) -> std::path::PathBuf {
    let h = std::env::temp_dir().join(format!("overlay_dec_{}_{}", std::process::id(), tag));
    std::fs::create_dir_all(&h).ok();
    h
}

/// #2305: drive a sequence of keys through `handle_key` against a fresh ctx
/// bound to `home` (so the Decisions overlay's `decisions::answer` writes land
/// in this temp home).
fn run_keys(home: &Path, overlay: &mut Overlay, keys: &[KeyCode]) {
    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut name_counter = HashMap::new();
    let mut layout = crate::layout::Layout::new();
    let mut reap_workers = Vec::new();
    let mut ctx = OverlayCtx {
        layout: &mut layout,
        registry: &registry,
        home,
        fleet_path: home,
        wakeup_tx: &tx,
        name_counter: &mut name_counter,
        task_rpc_tx: &test_task_channel().0,
        restart_request_tx: None,
        reap_workers: &mut reap_workers,
    };
    for k in keys {
        handle_key(overlay, press(*k), &mut ctx);
    }
}

#[test]
fn task_board_question_mark_shows_help() {
    let home = std::env::temp_dir().join("overlay_test_help_show");
    std::fs::create_dir_all(&home).ok();
    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut name_counter = HashMap::new();
    let mut layout = crate::layout::Layout::new();
    let mut reap_workers = Vec::new();
    let mut ctx = OverlayCtx {
        layout: &mut layout,
        registry: &registry,
        home: &home,
        fleet_path: &home,
        wakeup_tx: &tx,
        name_counter: &mut name_counter,
        task_rpc_tx: &test_task_channel().0,
        restart_request_tx: None,
        reap_workers: &mut reap_workers,
    };
    let mut overlay = task_overlay();
    handle_key(&mut overlay, press(KeyCode::Char('?')), &mut ctx);
    assert!(matches!(get_mode(&overlay), TaskBoardMode::Help));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn task_board_help_esc_returns_to_board() {
    let home = std::env::temp_dir().join("overlay_test_help_esc");
    std::fs::create_dir_all(&home).ok();
    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut name_counter = HashMap::new();
    let mut layout = crate::layout::Layout::new();
    let mut reap_workers = Vec::new();
    let mut ctx = OverlayCtx {
        layout: &mut layout,
        registry: &registry,
        home: &home,
        fleet_path: &home,
        wakeup_tx: &tx,
        name_counter: &mut name_counter,
        task_rpc_tx: &test_task_channel().0,
        restart_request_tx: None,
        reap_workers: &mut reap_workers,
    };
    let mut overlay = Overlay::Tasks {
        items: Vec::new(),
        col: 0,
        row: 0,
        mode: TaskBoardMode::Help,
        view: BoardView::Tasks,
        pending: false,
        notice: None,
    };
    handle_key(&mut overlay, press(KeyCode::Esc), &mut ctx);
    assert!(matches!(get_mode(&overlay), TaskBoardMode::Board));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn task_board_help_question_mark_toggles() {
    let home = std::env::temp_dir().join("overlay_test_help_toggle");
    std::fs::create_dir_all(&home).ok();
    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut name_counter = HashMap::new();
    let mut layout = crate::layout::Layout::new();
    let mut reap_workers = Vec::new();
    let mut ctx = OverlayCtx {
        layout: &mut layout,
        registry: &registry,
        home: &home,
        fleet_path: &home,
        wakeup_tx: &tx,
        name_counter: &mut name_counter,
        task_rpc_tx: &test_task_channel().0,
        restart_request_tx: None,
        reap_workers: &mut reap_workers,
    };
    let mut overlay = Overlay::Tasks {
        items: Vec::new(),
        col: 0,
        row: 0,
        mode: TaskBoardMode::Help,
        view: BoardView::Tasks,
        pending: false,
        notice: None,
    };
    handle_key(&mut overlay, press(KeyCode::Char('?')), &mut ctx);
    assert!(matches!(get_mode(&overlay), TaskBoardMode::Board));
    std::fs::remove_dir_all(&home).ok();
}

fn shift(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::SHIFT)
}

fn tmp_home(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-overlay-test-{}-{}-{}",
        std::process::id(),
        name,
        id
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok();
    dir
}

fn make_ctx<'a>(
    home: &'a std::path::Path,
    layout: &'a mut crate::layout::Layout,
    registry: &'a crate::agent::AgentRegistry,
    tx: &'a crossbeam_channel::Sender<usize>,
    task_rpc_tx: &'a crossbeam_channel::Sender<crate::app::rpc::TaskRequest>,
    name_counter: &'a mut HashMap<String, usize>,
    reap_workers: &'a mut Vec<std::thread::JoinHandle<()>>,
) -> OverlayCtx<'a> {
    OverlayCtx {
        layout,
        registry,
        home,
        fleet_path: home,
        wakeup_tx: tx,
        name_counter,
        task_rpc_tx,
        restart_request_tx: None,
        reap_workers,
    }
}

/// Regression: Shift+L (Kitty protocol: 'l'+SHIFT) must request the
/// canonical status update through the daemon RPC seam.
#[test]
fn task_board_l_requests_status_update_via_rpc() {
    let home = tmp_home("l_persist");
    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (tx, _rx) = crossbeam_channel::unbounded();
    let (task_tx, task_rx) = crossbeam_channel::unbounded();
    let mut name_counter = HashMap::new();
    let mut layout = crate::layout::Layout::new();
    let mut reap_workers = Vec::new();

    // Create a task (status=open) — #2306: open lives in the Todo column (0).
    crate::tasks::handle(
        &home,
        "user",
        &serde_json::json!({"action": "create", "title": "test", "priority": "normal"}),
    );
    let items = crate::tasks::list_all(&home);
    let task_id = items[0].id.clone();

    let mut overlay = Overlay::Tasks {
        items,
        col: 0,
        row: 0,
        mode: TaskBoardMode::Board,
        view: BoardView::Tasks,
        pending: false,
        notice: None,
    };

    // Kitty protocol: Shift+L → KeyCode::Char('l') + SHIFT
    let mut ctx = make_ctx(
        &home,
        &mut layout,
        &registry,
        &tx,
        &task_tx,
        &mut name_counter,
        &mut reap_workers,
    );
    handle_key(&mut overlay, shift(KeyCode::Char('l')), &mut ctx);

    let crate::app::rpc::TaskRequest::Mutate(arguments) =
        task_rx.recv().expect("task mutation should be queued")
    else {
        panic!("expected task mutation")
    };
    assert_eq!(arguments["action"], "update");
    assert_eq!(arguments["id"], task_id);
    assert_eq!(arguments["status"], "in_progress");

    std::fs::remove_dir_all(&home).ok();
}

/// Regression: Shift+D must mark task done from any column.
/// Tests both Kitty protocol ('d'+SHIFT) and legacy ('D') key events.
#[test]
fn task_board_shift_d_marks_done_from_any_column() {
    for (label, key_event) in [
        ("kitty", shift(KeyCode::Char('d'))),
        ("legacy", press(KeyCode::Char('D'))),
    ] {
        let home = tmp_home(&format!("shift_d_{label}"));
        let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (tx, _rx) = crossbeam_channel::unbounded();
        let (task_tx, task_rx) = crossbeam_channel::unbounded();
        let mut name_counter = HashMap::new();
        let mut layout = crate::layout::Layout::new();
        let mut reap_workers = Vec::new();

        crate::tasks::handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "t", "priority": "normal"}),
        );
        let items = crate::tasks::list_all(&home);
        let task_id = items[0].id.clone();

        // #2306: the open task lives in the Todo column (0). Shift+D marks
        // done from ANY column regardless.
        let mut overlay = Overlay::Tasks {
            items,
            col: 0,
            row: 0,
            mode: TaskBoardMode::Board,
            view: BoardView::Tasks,
            pending: false,
            notice: None,
        };

        let mut ctx = make_ctx(
            &home,
            &mut layout,
            &registry,
            &tx,
            &task_tx,
            &mut name_counter,
            &mut reap_workers,
        );
        handle_key(&mut overlay, key_event, &mut ctx);

        let crate::app::rpc::TaskRequest::Mutate(arguments) =
            task_rx.recv().expect("task mutation should be queued")
        else {
            panic!("expected task mutation")
        };
        assert_eq!(arguments["action"], "done", "Shift+D ({label}) action");
        assert_eq!(arguments["id"], task_id, "Shift+D ({label}) task");

        std::fs::remove_dir_all(&home).ok();
    }
}

/// Regression: cursor index must resolve the correct task after
/// filter/sort. Shift+L on row N must move the Nth task in the
/// column, not a different one.
#[test]
fn task_board_cursor_resolves_correct_task_index() {
    let home = tmp_home("cursor_resolve");
    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (tx, _rx) = crossbeam_channel::unbounded();
    let (task_tx, task_rx) = crossbeam_channel::unbounded();
    let mut name_counter = HashMap::new();
    let mut layout = crate::layout::Layout::new();
    let mut reap_workers = Vec::new();

    // Create two tasks in Open column with different priorities
    crate::tasks::handle(
        &home,
        "user",
        &serde_json::json!({"action": "create", "title": "low-pri", "priority": "normal"}),
    );
    // Sleep >1s to guarantee distinct task IDs (second-level precision)
    std::thread::sleep(std::time::Duration::from_millis(1100));
    crate::tasks::handle(
        &home,
        "user",
        &serde_json::json!({"action": "create", "title": "high-pri", "priority": "high"}),
    );

    let items = crate::tasks::list_all(&home);
    assert_eq!(items.len(), 2);

    // After sort: high-pri is row 0, low-pri is row 1. #2306: open tasks live
    // in the Todo column (0).
    let cols = crate::render::task_board_columns(&items);
    assert_eq!(cols[0][0].title, "high-pri");
    assert_eq!(cols[0][1].title, "low-pri");
    let high_id = cols[0][0].id.clone();

    // Move cursor to row 0 (high-pri), press Shift+L
    let mut overlay = Overlay::Tasks {
        items,
        col: 0,
        row: 0,
        mode: TaskBoardMode::Board,
        view: BoardView::Tasks,
        pending: false,
        notice: None,
    };

    let mut ctx = make_ctx(
        &home,
        &mut layout,
        &registry,
        &tx,
        &task_tx,
        &mut name_counter,
        &mut reap_workers,
    );
    handle_key(&mut overlay, shift(KeyCode::Char('l')), &mut ctx);

    let crate::app::rpc::TaskRequest::Mutate(arguments) =
        task_rx.recv().expect("task mutation should be queued")
    else {
        panic!("expected task mutation")
    };
    assert_eq!(arguments["action"], "update");
    assert_eq!(arguments["id"], high_id, "cursor must target high-pri");
    assert_eq!(arguments["status"], "in_progress");

    std::fs::remove_dir_all(&home).ok();
}

// --- Sprint 8 PR-N: Tab routing ---

#[test]
fn test_board_overlay_tab_toggles_view() {
    let home = std::env::temp_dir().join("overlay_test_tab_toggle");
    std::fs::create_dir_all(&home).ok();
    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut name_counter = HashMap::new();
    let mut layout = crate::layout::Layout::new();
    let mut reap_workers = Vec::new();
    let mut ctx = OverlayCtx {
        layout: &mut layout,
        registry: &registry,
        home: &home,
        fleet_path: &home,
        wakeup_tx: &tx,
        name_counter: &mut name_counter,
        task_rpc_tx: &test_task_channel().0,
        restart_request_tx: None,
        reap_workers: &mut reap_workers,
    };
    let mut overlay = Overlay::Tasks {
        items: Vec::new(),
        col: 0,
        row: 0,
        mode: TaskBoardMode::Board,
        view: BoardView::Tasks,
        pending: false,
        notice: None,
    };
    handle_key(&mut overlay, press(KeyCode::Tab), &mut ctx);
    if let Overlay::Tasks { view, .. } = &overlay {
        assert_eq!(*view, BoardView::Fleet, "Tab must switch to Fleet");
    } else {
        panic!("overlay must still be Tasks");
    }
    handle_key(&mut overlay, press(KeyCode::Tab), &mut ctx);
    if let Overlay::Tasks { view, .. } = &overlay {
        assert_eq!(*view, BoardView::Status, "Tab must switch to Status");
    }
    handle_key(&mut overlay, press(KeyCode::Tab), &mut ctx);
    if let Overlay::Tasks { view, .. } = &overlay {
        assert_eq!(*view, BoardView::Monitor, "Tab must switch to Monitor");
    }
    handle_key(&mut overlay, press(KeyCode::Tab), &mut ctx);
    if let Overlay::Tasks { view, .. } = &overlay {
        assert_eq!(*view, BoardView::Tasks, "Tab must cycle back to Tasks");
    }
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_non_board_mode_ignores_tab() {
    let home = std::env::temp_dir().join("overlay_test_tab_ignore");
    std::fs::create_dir_all(&home).ok();
    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut name_counter = HashMap::new();
    let mut layout = crate::layout::Layout::new();
    let mut reap_workers = Vec::new();
    let mut ctx = OverlayCtx {
        layout: &mut layout,
        registry: &registry,
        home: &home,
        fleet_path: &home,
        wakeup_tx: &tx,
        name_counter: &mut name_counter,
        task_rpc_tx: &test_task_channel().0,
        restart_request_tx: None,
        reap_workers: &mut reap_workers,
    };
    let mut overlay = Overlay::Tasks {
        items: Vec::new(),
        col: 0,
        row: 0,
        mode: TaskBoardMode::Help,
        view: BoardView::Tasks,
        pending: false,
        notice: None,
    };
    handle_key(&mut overlay, press(KeyCode::Tab), &mut ctx);
    if let Overlay::Tasks { view, .. } = &overlay {
        assert_eq!(
            *view,
            BoardView::Tasks,
            "Tab in Help mode must not switch view"
        );
    }
    std::fs::remove_dir_all(&home).ok();
}

// #2305: the read-only `handle_list_scroll` helper was removed when the
// Decisions overlay became interactive (it was its only prod caller); its
// navigation is now covered by the Decisions answer-overlay tests below.

// ─── #2305 interactive decision answer overlay ───

fn open_decisions(home: &Path) -> Overlay {
    Overlay::Decisions {
        items: crate::decisions::list_pending(home),
        selected: 0,
        mode: DecisionMode::Browse,
    }
}

#[test]
fn decisions_option_answer_submits_and_reloads() {
    let home = dec_home("opt");
    let _ = crate::decisions::post(
        &home,
        "lead",
        &serde_json::json!({
            "title": "Deploy?", "content": "ship v2?", "needs_answer": true,
            "options": [{"label": "yes", "recommended": true}, "no"],
        }),
    );
    let mut overlay = open_decisions(&home);
    // Enter opens Answer (option-select, since options exist).
    run_keys(&home, &mut overlay, &[KeyCode::Enter]);
    assert!(matches!(
        &overlay,
        Overlay::Decisions {
            mode: DecisionMode::Answer {
                option: 0,
                free_text: None
            },
            ..
        }
    ));
    // Down→'no', Up→'yes', Enter submits 'yes'.
    run_keys(
        &home,
        &mut overlay,
        &[KeyCode::Down, KeyCode::Up, KeyCode::Enter],
    );
    match &overlay {
        Overlay::Decisions { items, mode, .. } => {
            assert!(
                matches!(mode, DecisionMode::Browse),
                "back to Browse after submit"
            );
            assert!(
                items.is_empty(),
                "answered question dropped from pending list"
            );
        }
        _ => panic!("expected Decisions overlay after submit"),
    }
    let d = crate::decisions::list_all(&home)
        .into_iter()
        .next()
        .expect("decision present");
    assert_eq!(d.answer.as_deref(), Some("yes"));
    assert_eq!(d.answered_by.as_deref(), Some("operator"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn decisions_free_text_answer_submits() {
    let home = dec_home("free");
    let _ = crate::decisions::post(
        &home,
        "lead",
        &serde_json::json!({
            "title": "Why?", "content": "explain", "needs_answer": true, "allow_free_text": true,
        }),
    );
    let mut overlay = open_decisions(&home);
    // No options + free text allowed → Enter goes straight to free-text entry.
    run_keys(&home, &mut overlay, &[KeyCode::Enter]);
    assert!(matches!(
        &overlay,
        Overlay::Decisions {
            mode: DecisionMode::Answer {
                free_text: Some(_),
                ..
            },
            ..
        }
    ));
    run_keys(
        &home,
        &mut overlay,
        &[KeyCode::Char('h'), KeyCode::Char('i'), KeyCode::Enter],
    );
    let d = crate::decisions::list_all(&home)
        .into_iter()
        .next()
        .expect("decision present");
    assert_eq!(d.answer.as_deref(), Some("hi"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn decisions_esc_backs_out_then_closes() {
    let home = dec_home("esc");
    let _ = crate::decisions::post(
        &home,
        "lead",
        &serde_json::json!({
            "title": "Q", "content": "?", "needs_answer": true,
            "options": ["a"], "allow_free_text": true,
        }),
    );
    let mut overlay = open_decisions(&home);
    // Enter→Answer(option), f→free-text.
    run_keys(&home, &mut overlay, &[KeyCode::Enter, KeyCode::Char('f')]);
    assert!(matches!(
        &overlay,
        Overlay::Decisions {
            mode: DecisionMode::Answer {
                free_text: Some(_),
                ..
            },
            ..
        }
    ));
    // Esc backs free-text → option-select.
    run_keys(&home, &mut overlay, &[KeyCode::Esc]);
    assert!(matches!(
        &overlay,
        Overlay::Decisions {
            mode: DecisionMode::Answer {
                free_text: None,
                ..
            },
            ..
        }
    ));
    // Esc backs option-select → Browse.
    run_keys(&home, &mut overlay, &[KeyCode::Esc]);
    assert!(matches!(
        &overlay,
        Overlay::Decisions {
            mode: DecisionMode::Browse,
            ..
        }
    ));
    // Esc in Browse closes the overlay.
    run_keys(&home, &mut overlay, &[KeyCode::Esc]);
    assert!(matches!(&overlay, Overlay::None));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn decisions_browse_jk_navigates() {
    let home = dec_home("nav");
    for t in ["q1", "q2", "q3"] {
        let _ = crate::decisions::post(
            &home,
            "lead",
            &serde_json::json!({"title": t, "content": "?", "needs_answer": true, "allow_free_text": true}),
        );
    }
    let mut overlay = open_decisions(&home);
    run_keys(
        &home,
        &mut overlay,
        &[KeyCode::Char('j'), KeyCode::Char('j')],
    );
    match &overlay {
        Overlay::Decisions {
            selected, items, ..
        } => {
            assert_eq!(items.len(), 3);
            assert_eq!(*selected, 2, "j×2 moves to last");
        }
        _ => panic!("expected Decisions overlay"),
    }
    run_keys(&home, &mut overlay, &[KeyCode::Char('k')]);
    match &overlay {
        Overlay::Decisions { selected, .. } => assert_eq!(*selected, 1, "k moves up"),
        _ => panic!("expected Decisions overlay"),
    }
    std::fs::remove_dir_all(&home).ok();
}

/// #t-5 BLOCKER 2 (r6): a paste can leave `selected` past the now-shorter
/// candidate list; Tab must still complete the highlighted keyword instead of
/// silently no-op'ing on an out-of-range index. Drive the exact stale state
/// (selected=5, input "co" → only `config` matches) through `handle_key`.
#[test]
fn tab_completes_keyword_when_selected_is_stale() {
    let home = dec_home("cmd_tab_stale");
    let mut overlay = Overlay::Command {
        input: "co".to_string(),
        selected: 5,
    };
    run_keys(&home, &mut overlay, &[KeyCode::Tab]);
    match overlay {
        Overlay::Command { input, selected } => {
            assert_eq!(
                input, "config ",
                "Tab completes the matched keyword + trailing space"
            );
            assert_eq!(selected, 0, "Tab resets the highlight");
        }
        _ => panic!("palette must stay open after Tab"),
    }
    std::fs::remove_dir_all(&home).ok();
}

/// #3662 RED: `:restart` must ask for confirmation before the command
/// dispatcher can kill or replace a pane.
#[test]
fn restart_command_confirms_before_execution_3662() {
    let source = include_str!("../overlay.rs").replace("\r\n", "\n");
    assert_restart_confirmation_order(&source);
    let crlf_source = source.replace('\n', "\r\n");
    assert_restart_confirmation_order(&crlf_source);
}

fn assert_restart_confirmation_order(source: &str) {
    let source = source.replace("\r\n", "\n");
    let start = source
        .find("Overlay::Command {\n            ref mut input")
        .expect("command overlay handler must exist");
    let end = source[start..]
        .find("Overlay::Decisions {")
        .map(|offset| start + offset)
        .expect("command overlay handler end marker");
    let region = &source[start..end];
    let confirm = region
        .find("ConfirmRestart")
        .expect("restart Enter must open a confirmation overlay");
    let execute = region
        .find("execute_command")
        .expect("command overlay must retain the execution path");
    assert!(
        confirm < execute,
        "restart confirmation must be established before command execution"
    );
}
