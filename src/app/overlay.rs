//! Modal overlays — the non-pane UI elements (menus, rename prompts, help,
//! command palette, etc.). Key input flows through `handle_key` which mutates
//! the overlay and surrounding state via `OverlayCtx`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;
use std::path::Path;

use crate::agent::AgentRegistry;
use crate::backend::Backend;
use crate::layout::organizer::{OrganizerPlan, OrganizerScope};
use crate::layout::{Layout, Pane, SplitDir, Tab};

mod organizer_ui;

/// An item in the new-tab selection menu.
pub struct MenuItem {
    pub label: String,
    pub kind: MenuItemKind,
}

pub enum MenuItemKind {
    Shell,
    Backend(Backend),
    FleetInstance(String),
    /// #2441: one-click Fugu (codex harness + isolated sakana CODEX_HOME).
    /// Only surfaced when `provider_detect` reports the provider available.
    Fugu,
}

pub(super) enum CloseTarget {
    Pane,
    Tab,
}

/// #2305: sub-mode of the pending-decision answer overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionMode {
    /// Navigating the pending-question list (j/k select, Enter answers, q/Esc close).
    Browse,
    /// Answering the selected question. `option` is the highlighted option index;
    /// `free_text` is `Some(buffer)` while typing a free-text answer (only when the
    /// question's `allow_free_text` is set), else `None` (choosing among options).
    Answer {
        option: usize,
        free_text: Option<String>,
    },
}

pub enum TaskBoardMode {
    Board,
    Detail,
    NewTask {
        input: String,
    },
    Assign {
        /// (display_label, assignee_value)
        choices: Vec<(String, String)>,
        selected: usize,
        /// M3: captured task ID to avoid col/row index drift
        task_id: String,
    },
    Help,
}

/// Which view is active in the board overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardView {
    Tasks,
    Fleet,
    Status,
    Monitor,
}

/// Where a menu-selected item should be placed.
#[derive(Clone, Copy)]
pub(super) enum MenuTarget {
    Tab,
    Split(SplitDir),
}

/// Whether choosing this item creates a NEW persistent daemon instance (fleet.yaml
/// entry + backend process). Those need an explicit confirmation; local shells and
/// existing fleet agents are only views and do not.
fn creates_new_instance(kind: &MenuItemKind) -> bool {
    matches!(kind, MenuItemKind::Backend(_) | MenuItemKind::Fugu)
}

pub(super) enum Overlay {
    None,
    /// New tab selection menu.
    NewTabMenu {
        items: Vec<MenuItem>,
        selected: usize,
    },
    /// Split pane selection menu — choose what to run in the new pane.
    SplitMenu {
        items: Vec<MenuItem>,
        selected: usize,
        dir: SplitDir,
    },
    /// `y` confirmation before a menu selection creates a new daemon instance.
    /// Any other key cancels, so stray typing can never spawn one.
    ConfirmCreate {
        item: MenuItem,
        target: MenuTarget,
    },
    RenameTab {
        input: String,
    },
    RenamePane {
        input: String,
    },
    ConfirmClose {
        target: CloseTarget,
    },
    /// Explicit operator restart confirmation. The command is retained until
    /// the operator accepts, so no pane is killed while the prompt is shown.
    ConfirmRestart {
        command: String,
    },
    ConfirmDeleteInstance {
        name: String,
        input: String,
        notice: Option<String>,
    },
    TabList {
        selected: usize,
    },
    /// Move-pane destination picker. Lists all tabs plus a trailing
    /// "[+] New tab" slot. Enter applies `Layout::move_pane_across_tabs`
    /// with the focused pane as source.
    MovePaneTarget {
        /// 0..tabs.len() → move into that tab. == tabs.len() → new tab.
        selected: usize,
        /// Source pane to move. Captured on overlay open so a subsequent
        /// mouse click that changes focus (while the overlay is modal,
        /// this shouldn't happen, but be defensive) can't retarget the move.
        source_pane_id: usize,
        /// Source tab index at overlay-open time. Same defensive capture as
        /// `source_pane_id`; `move_pane_across_tabs` re-verifies existence.
        source_tab_idx: usize,
        /// Target split direction when moving into an existing tab.
        split_dir: crate::layout::SplitDir,
    },
    Help,
    ReconnectNotice {
        message: String,
    },
    /// Keyboard scroll mode (j/k/PgUp/PgDn). Pane's scroll_offset is used directly.
    Scroll,
    /// Command palette (:command input).
    Command {
        input: String,
        /// #t-5 completion: highlighted index into the prefix-matched candidate
        /// list (`commands::matching_specs(input)`). Recomputed on each edit; the
        /// candidate list itself is derived from `input` (not stored) so the enum
        /// holds no borrowed `&'static` specs.
        selected: usize,
    },
    /// #2305: pending-decision answer board (Ctrl+B D). Lists questions awaiting
    /// an operator answer; Browse to pick one, then Answer (select an option or
    /// type free-text). Mirrors the interactive `Tasks` overlay.
    Decisions {
        items: Vec<crate::decisions::Decision>,
        /// Selected pending question (Browse mode).
        selected: usize,
        /// Sub-mode: Browse the list, or Answer the selected question.
        mode: DecisionMode,
    },
    /// Task board overlay — 5-column kanban view with CRUD.
    Tasks {
        items: Vec<crate::tasks::Task>,
        /// Currently focused column (#2306: 0=Todo, 1=Working, 2=Review, 3=Blocked, 4=Done).
        col: usize,
        /// Currently focused row within the column.
        row: usize,
        /// Sub-mode: None=board, Detail, NewTask(input), Assign(selected).
        mode: TaskBoardMode,
        /// Active view: Tasks (kanban) or Fleet (agent dashboard).
        view: BoardView,
        pending: bool,
        notice: Option<String>,
    },
    /// Floating scratch shell (Ctrl+B ~). Esc kills the shell and closes the
    /// overlay. Pane is boxed because it's much larger than any other variant.
    ScratchShell {
        pane: Box<Pane>,
    },
    /// #3631: transactional Team Organizer. `plan` is a pure preview computed
    /// from `scope`; nothing in the layout changes until Enter. `applied` marks a
    /// committed transaction so a failed session save can be retried, and `u`
    /// reverts via the snapshot held on the layout.
    Organizer {
        scope: OrganizerScope,
        scopes: Vec<OrganizerScope>,
        selected: usize,
        plan: OrganizerPlan,
        applied: bool,
        notice: Option<String>,
    },
}

/// #2305: record the operator's answer to a pending decision and reload the
/// pending list so the answered item drops out. `answered_by` is fixed
/// `"operator"` — the TUI is the trusted local operator. Best-effort: on a
/// backend error the list simply doesn't change (the item stays pending), so the
/// operator can retry; the overlay never surfaces a hard failure mid-key.
fn submit_decision_answer(
    home: &Path,
    id: &str,
    answer: String,
    items: &mut Vec<crate::decisions::Decision>,
) {
    let _ = crate::decisions::answer(
        home,
        "operator",
        &serde_json::json!({"id": id, "answer": answer}),
    );
    *items = crate::decisions::list_pending(home);
}

/// Bundle of mutable references passed to `handle_key` so overlay handlers can
/// affect layout / registry / telegram state / name counter without the event
/// loop needing a dedicated arg per overlay variant.
pub(super) struct OverlayCtx<'a> {
    pub layout: &'a mut Layout,
    pub registry: &'a AgentRegistry,
    pub home: &'a Path,
    pub fleet_path: &'a Path,
    pub wakeup_tx: &'a crossbeam_channel::Sender<usize>,
    pub name_counter: &'a mut HashMap<String, usize>,
    pub task_rpc_tx: &'a crossbeam_channel::Sender<super::rpc::TaskRequest>,
    pub restart_request_tx:
        Option<&'a crossbeam_channel::Sender<super::commands::RemoteRestartRequest>>,
    pub reap_workers: &'a mut Vec<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
pub(super) struct OverlayOutcome {
    /// Layout changed in a way that requires a resize pass before next draw.
    pub needs_resize: bool,
}

/// A menu item was chosen. Creating a NEW daemon instance asks for `y` first
/// (a stray Enter must not spawn a billable agent); everything else runs now.
fn choose_menu_item(
    overlay: &mut Overlay,
    item: MenuItem,
    target: MenuTarget,
    ctx: &mut OverlayCtx<'_>,
    outcome: &mut OverlayOutcome,
) {
    if creates_new_instance(&item.kind) {
        *overlay = Overlay::ConfirmCreate { item, target };
    } else {
        spawn_menu_item(item, target, ctx, outcome);
    }
}

/// Build the pane for a chosen menu item and place it as a new tab or a split.
fn spawn_menu_item(
    item: MenuItem,
    target: MenuTarget,
    ctx: &mut OverlayCtx<'_>,
    outcome: &mut OverlayOutcome,
) {
    let (c, r) = crossterm::terminal::size().unwrap_or((120, 40));
    let content_rows = ctx.layout.pane_content_rows(r);
    let (pc, pr) = match target {
        MenuTarget::Tab => (c.saturating_sub(2), content_rows),
        MenuTarget::Split(SplitDir::Vertical) => (c.saturating_sub(2) / 2, content_rows),
        MenuTarget::Split(SplitDir::Horizontal) => (c.saturating_sub(2), content_rows / 2),
    };
    match super::pane_from_menu_item(
        item,
        ctx.fleet_path,
        ctx.layout,
        ctx.registry,
        ctx.home,
        pc,
        pr,
        ctx.wakeup_tx,
        ctx.name_counter,
    ) {
        Ok(pane) => {
            super::telegram_hooks::maybe_create_telegram_topic(ctx.registry, ctx.home, &pane);
            super::discord_hooks::maybe_create_discord_binding(ctx.registry, &pane);
            match target {
                MenuTarget::Tab => {
                    let tab_name = pane.agent_name.clone();
                    ctx.layout.add_tab(Tab::new(tab_name.to_string(), pane));
                }
                MenuTarget::Split(dir) => {
                    if let Some(tab) = ctx.layout.active_tab_mut() {
                        tab.split_focused(dir, pane);
                    }
                }
            }
            outcome.needs_resize = true;
        }
        // #t-98760-1: surface a failed spawn instead of silently swallowing the
        // Err — a swallowed Err looked to the operator like Ctrl+B c "did nothing".
        Err(e) => match target {
            MenuTarget::Tab => tracing::error!(error = %e, "new-tab spawn failed"),
            MenuTarget::Split(_) => tracing::error!(error = %e, "split failed"),
        },
    }
}

fn execute_command(command: &str, ctx: &mut OverlayCtx<'_>, outcome: &mut OverlayOutcome) {
    let mut cctx = super::commands::CommandCtx {
        layout: &mut *ctx.layout,
        registry: ctx.registry,
        home: ctx.home,
        wakeup_tx: ctx.wakeup_tx,
        name_counter: &mut *ctx.name_counter,
        restart_tx: ctx.restart_request_tx,
    };
    if super::commands::execute(command, &mut cctx) {
        outcome.needs_resize = true;
    }
}

/// Dispatch a key press to the currently-active overlay. Mutates `*overlay`
/// (including replacing it with `Overlay::None` to close) and may mutate
/// layout / registry state via `ctx`. Caller is responsible for only invoking
/// this when `!matches!(overlay, Overlay::None)`.
pub(super) fn handle_key(
    overlay: &mut Overlay,
    key: KeyEvent,
    ctx: &mut OverlayCtx<'_>,
) -> OverlayOutcome {
    let mut outcome = OverlayOutcome::default();
    match overlay {
        Overlay::NewTabMenu {
            items,
            ref mut selected,
        } => match key.code {
            KeyCode::Up | KeyCode::Char('k') if *selected > 0 => {
                *selected -= 1;
            }
            KeyCode::Down | KeyCode::Char('j') if *selected + 1 < items.len() => {
                *selected += 1;
            }
            KeyCode::Enter => {
                let sel = *selected;
                if let Overlay::NewTabMenu { items, .. } = std::mem::replace(overlay, Overlay::None)
                {
                    if let Some(item) = items.into_iter().nth(sel) {
                        choose_menu_item(overlay, item, MenuTarget::Tab, ctx, &mut outcome);
                    }
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                *overlay = Overlay::None;
            }
            _ => {}
        },
        Overlay::SplitMenu {
            items,
            ref mut selected,
            dir,
        } => {
            let split_dir = *dir;
            match key.code {
                KeyCode::Up | KeyCode::Char('k') if *selected > 0 => {
                    *selected -= 1;
                }
                KeyCode::Down | KeyCode::Char('j') if *selected + 1 < items.len() => {
                    *selected += 1;
                }
                KeyCode::Enter => {
                    let sel = *selected;
                    if let Overlay::SplitMenu { items, .. } =
                        std::mem::replace(overlay, Overlay::None)
                    {
                        if let Some(item) = items.into_iter().nth(sel) {
                            choose_menu_item(
                                overlay,
                                item,
                                MenuTarget::Split(split_dir),
                                ctx,
                                &mut outcome,
                            );
                        }
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    *overlay = Overlay::None;
                }
                _ => {}
            }
        }
        Overlay::ConfirmCreate { .. } => {
            if let Overlay::ConfirmCreate { item, target } =
                std::mem::replace(overlay, Overlay::None)
            {
                // Only a bare lowercase `y`: Ctrl/Alt/Super+Y, Shift+Y and `Y`
                // all cancel, matching the prompt text ("y = create").
                if key.code == KeyCode::Char('y') && key.modifiers.is_empty() {
                    spawn_menu_item(item, target, ctx, &mut outcome);
                }
            }
        }
        Overlay::RenameTab { ref mut input } => match key.code {
            KeyCode::Enter => {
                let new_name = input.clone();
                if !new_name.is_empty() {
                    if let Some(tab) = ctx.layout.active_tab_mut() {
                        tab.name = new_name;
                    }
                }
                *overlay = Overlay::None;
            }
            KeyCode::Esc => {
                *overlay = Overlay::None;
            }
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c) => {
                input.push(c);
            }
            _ => {}
        },
        Overlay::RenamePane { ref mut input } => match key.code {
            KeyCode::Enter => {
                let new_name = input.clone();
                if let Some(tab) = ctx.layout.active_tab_mut() {
                    let fid = tab.focus_id;
                    if let Some(pane) = tab.root_mut().find_pane_mut(fid) {
                        pane.display_name = if new_name.is_empty() {
                            None // clear → revert to agent_name
                        } else {
                            Some(new_name)
                        };
                    }
                }
                *overlay = Overlay::None;
            }
            KeyCode::Esc => {
                *overlay = Overlay::None;
            }
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c) => {
                input.push(c);
            }
            _ => {}
        },
        Overlay::ConfirmClose { target } => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let is_tab = matches!(target, CloseTarget::Tab);
                *overlay = Overlay::None;
                // Pane/tab close is a VIEW operation. Fleet panes are remote
                // views onto daemon-owned instances, so dropping one must never
                // call the destructive instance lifecycle (fleet/team removal,
                // worktree/workspace cleanup, topic/watch deletion). Permanent
                // deletion remains behind the explicit `delete_instance`
                // control surface. Dropping the Pane closes its bridge client.
                //
                // Also capture each closed pane's authoritative registry id so
                // a non-fleet local shell still has its PTY child reaped. These
                // shells are intentionally absent from fleet.yaml, so name-based
                // managed deletion cannot resolve them.
                let nonfleet_agents: Vec<crate::types::InstanceId>;
                if is_tab {
                    let idx = ctx.layout.active;
                    let panes: Vec<(crate::types::InstanceId, Option<String>)> = ctx
                        .layout
                        .tabs
                        .get(idx)
                        .into_iter()
                        .flat_map(|t| {
                            t.root().pane_ids().into_iter().filter_map(|id| {
                                t.root()
                                    .find_pane(id)
                                    .map(|p| (p.instance_id, p.fleet_instance_name.clone()))
                            })
                        })
                        .collect();
                    nonfleet_agents = panes
                        .into_iter()
                        .filter(|(_, f)| f.is_none())
                        .map(|(a, _)| a)
                        .collect();
                    let _ = ctx.layout.close_tab(idx);
                    outcome.needs_resize = true;
                } else if let Some(tab) = ctx.layout.active_tab_mut() {
                    let fid = tab.focus_id;
                    let (instance_id, fleet_name): (
                        Option<crate::types::InstanceId>,
                        Option<String>,
                    ) = tab
                        .root()
                        .find_pane(fid)
                        .map(|p| (Some(p.instance_id), p.fleet_instance_name.clone()))
                        .unwrap_or((None, None));
                    if tab.close_focused().is_some() {
                        outcome.needs_resize = true;
                    }
                    nonfleet_agents = match (instance_id, fleet_name) {
                        (Some(a), None) => vec![a],
                        _ => Vec::new(),
                    };
                } else {
                    nonfleet_agents = Vec::new();
                }
                // Reap non-fleet shell panes by UUID, mirroring ScratchShell Esc.
                super::kill_unmanaged_agents(ctx.registry, nonfleet_agents, &mut *ctx.reap_workers);
            }
            _ => {
                *overlay = Overlay::None;
            }
        },
        Overlay::ConfirmRestart { command } => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let command = command.clone();
                *overlay = Overlay::None;
                execute_command(&command, ctx, &mut outcome);
            }
            _ => {
                *overlay = Overlay::None;
            }
        },
        Overlay::ConfirmDeleteInstance {
            name,
            input,
            notice,
        } => match key.code {
            KeyCode::Enter => {
                if input == name {
                    if crate::daemon::find_active_run_dir(ctx.home).is_none() {
                        *notice = Some("delete refused: no active daemon".to_string());
                    } else {
                        // Capture the exact view identity before the daemon
                        // delete. A name-only pane (legacy session) is not
                        // safe to remove after a same-name replacement.
                        let expected_ref = authoritative_instance_ref_for_delete(ctx, name);
                        match crate::mcp::handlers::instance_state::lifecycle::full_delete_instance_with_runtime(
                            ctx.home,
                            name,
                            None,
                        ) {
                            Ok(()) => {
                                match finish_successful_delete(ctx, name, expected_ref) {
                                    DeleteCompletion::Close => {
                                        *overlay = Overlay::None;
                                        outcome.needs_resize = true;
                                    }
                                    DeleteCompletion::Keep { notice: message } => {
                                        *notice = Some(message);
                                    }
                                }
                            }
                            Err(error) => {
                                *notice = Some(error);
                            }
                        }
                    }
                } else {
                    *notice = Some(format!("type exact name to delete '{name}'"));
                }
            }
            KeyCode::Esc => {
                *overlay = Overlay::None;
            }
            KeyCode::Backspace => {
                input.pop();
                *notice = None;
            }
            KeyCode::Char(c) => {
                input.push(c);
                *notice = None;
            }
            _ => {}
        },
        Overlay::TabList { ref mut selected } => match key.code {
            KeyCode::Up | KeyCode::Char('k') if *selected > 0 => {
                *selected -= 1;
            }
            KeyCode::Down | KeyCode::Char('j') if *selected + 1 < ctx.layout.tabs.len() => {
                *selected += 1;
            }
            KeyCode::Enter => {
                ctx.layout.goto_tab(*selected);
                *overlay = Overlay::None;
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                *overlay = Overlay::None;
            }
            _ => {}
        },
        Overlay::MovePaneTarget {
            ref mut selected,
            ref mut split_dir,
            source_pane_id,
            source_tab_idx,
        } => {
            // List length = tabs.len() + 1 (trailing "New tab" entry). The
            // "New tab" slot is always valid; tab slots other than source
            // are valid move targets. `selected` can legally equal tabs.len().
            let list_len = ctx.layout.tabs.len() + 1;
            match key.code {
                KeyCode::Tab | KeyCode::Char('s') => {
                    *split_dir = match split_dir {
                        crate::layout::SplitDir::Horizontal => crate::layout::SplitDir::Vertical,
                        crate::layout::SplitDir::Vertical => crate::layout::SplitDir::Horizontal,
                    };
                }
                KeyCode::Up | KeyCode::Char('k') if *selected > 0 => {
                    *selected -= 1;
                }
                KeyCode::Down | KeyCode::Char('j') if *selected + 1 < list_len => {
                    *selected += 1;
                }
                KeyCode::Enter => {
                    let src_pane = *source_pane_id;
                    let src_tab = *source_tab_idx;
                    let sel = *selected;
                    let dir = *split_dir;
                    let tabs_count = ctx.layout.tabs.len();
                    // H5: overlay clear moved to after move logic (was before — fragile)
                    if sel == tabs_count {
                        // New tab: name after the pane's agent for a sensible default.
                        let name = ctx
                            .layout
                            .tabs
                            .get(src_tab)
                            .and_then(|t| t.root().find_pane(src_pane))
                            .map(|p| p.agent_name.clone())
                            .unwrap_or_else(|| "new".into());
                        if ctx
                            .layout
                            .move_pane_across_tabs(
                                src_tab,
                                src_pane,
                                crate::layout::MovePlacement::NewTab {
                                    name: name.to_string(),
                                },
                            )
                            .is_some()
                        {
                            outcome.needs_resize = true;
                        }
                    } else if sel != src_tab {
                        if let Some(new_idx) = ctx.layout.move_pane_across_tabs(
                            src_tab,
                            src_pane,
                            crate::layout::MovePlacement::SplitFocused { to_tab: sel, dir },
                        ) {
                            // Follow the pane — the user just pointed at this tab.
                            ctx.layout.goto_tab(new_idx);
                            outcome.needs_resize = true;
                        }
                    }
                    // sel == src_tab: no-op (already in that tab).
                    *overlay = Overlay::None;
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    *overlay = Overlay::None;
                }
                _ => {}
            }
        }
        Overlay::Help => {
            *overlay = Overlay::None;
        }
        Overlay::ReconnectNotice { .. } => {
            *overlay = Overlay::None;
        }
        Overlay::Scroll => match key.code {
            KeyCode::Up | KeyCode::Char('k') => super::scroll_focused(ctx.layout, 1),
            KeyCode::Down | KeyCode::Char('j') => super::scroll_focused(ctx.layout, -1),
            KeyCode::PageUp => super::scroll_focused(ctx.layout, 20),
            KeyCode::PageDown => super::scroll_focused(ctx.layout, -20),
            KeyCode::Char('q') | KeyCode::Esc => {
                *overlay = Overlay::None;
            }
            _ => {}
        },
        Overlay::Command {
            ref mut input,
            ref mut selected,
        } => match key.code {
            // Enter: execute the CURRENT input — semantics unchanged from before
            // completion existed (completion is opt-in via Tab; Enter never picks a
            // candidate, so muscle memory / scripts behave identically).
            KeyCode::Enter => {
                let cmd = input.clone();
                if super::commands::is_arrange_command(&cmd) {
                    *overlay = organizer_ui::open_arrange_overlay(&cmd, ctx, &mut outcome)
                        .unwrap_or(Overlay::None);
                } else if super::commands::is_restart_command(&cmd) {
                    *overlay = Overlay::ConfirmRestart { command: cmd };
                } else {
                    *overlay = Overlay::None;
                    execute_command(&cmd, ctx, &mut outcome);
                }
            }
            KeyCode::Esc => {
                *overlay = Overlay::None;
            }
            // #t-5 completion nav: move the highlight within the candidate list
            // (keywords while typing the command, argument values once past it).
            KeyCode::Up => {
                *selected = selected.saturating_sub(1);
            }
            KeyCode::Down => {
                // Bound to the VISIBLE window (capped at MAX_PALETTE_ROWS), not
                // the full count — otherwise `selected` could run past the last
                // rendered row and Tab would complete an off-screen candidate.
                let n = super::commands::palette_completion(input, ctx.registry).visible_count();
                if *selected + 1 < n {
                    *selected += 1;
                }
            }
            // Tab: complete the token under the cursor to the highlighted
            // candidate — the command keyword while typing it, or an argument
            // value once the keyword is complete — keeping the palette open with
            // a trailing space for the next argument. No-op when there's nothing
            // to complete (the clamp + empty handling live in `tab_complete`).
            // Computed from the SAME `palette_completion` the renderer drew so the
            // completed candidate matches the on-screen highlight.
            KeyCode::Tab => {
                let completion = super::commands::palette_completion(input, ctx.registry);
                if let Some(new_input) =
                    super::commands::tab_complete(input, &completion, *selected)
                {
                    *input = new_input;
                    *selected = 0;
                }
            }
            KeyCode::Backspace => {
                input.pop();
                *selected = 0; // candidate set changed → reset highlight
            }
            KeyCode::Char(c) => {
                input.push(c);
                *selected = 0; // candidate set changed → reset highlight
            }
            _ => {}
        },
        Overlay::Organizer { .. } => {
            let org = organizer_ui::handle_organizer_key(overlay, key, ctx);
            outcome.needs_resize = outcome.needs_resize || org.needs_resize;
        }
        Overlay::Decisions {
            ref mut items,
            ref mut selected,
            ref mut mode,
        } => {
            match mode {
                DecisionMode::Browse => match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        *selected = selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') if *selected + 1 < items.len() => {
                        *selected += 1;
                    }
                    // Enter answers the selected question. Open free-text directly
                    // when there are no options to choose (and free text is allowed).
                    KeyCode::Enter => {
                        if let Some(d) = items.get(*selected) {
                            let free_text = if d.options.is_empty() && d.allow_free_text {
                                Some(String::new())
                            } else {
                                None
                            };
                            // Only enter Answer mode if there's actually a way to
                            // answer (some options, or free text allowed).
                            if !d.options.is_empty() || d.allow_free_text {
                                *mode = DecisionMode::Answer {
                                    option: 0,
                                    free_text,
                                };
                            }
                        }
                    }
                    KeyCode::Esc | KeyCode::Char('q') => {
                        *overlay = Overlay::None;
                    }
                    _ => {}
                },
                DecisionMode::Answer {
                    ref mut option,
                    ref mut free_text,
                } => {
                    // Snapshot the bits we need as OWNED values so the submit path
                    // can take `&mut items` without an outstanding borrow. If the
                    // selected decision vanished (race), bail back to Browse.
                    let Some((id, opt_labels, allow_free)) = items.get(*selected).map(|d| {
                        (
                            d.id.clone(),
                            d.options
                                .iter()
                                .map(|o| o.label.clone())
                                .collect::<Vec<_>>(),
                            d.allow_free_text,
                        )
                    }) else {
                        *mode = DecisionMode::Browse;
                        return outcome;
                    };
                    if let Some(buf) = free_text {
                        // Free-text input sub-mode.
                        match key.code {
                            KeyCode::Enter if !buf.is_empty() => {
                                submit_decision_answer(ctx.home, &id, buf.clone(), items);
                                *selected = (*selected).min(items.len().saturating_sub(1));
                                *mode = DecisionMode::Browse;
                            }
                            KeyCode::Char(c) => buf.push(c),
                            KeyCode::Backspace => {
                                buf.pop();
                            }
                            // Esc backs out of free-text to option select.
                            KeyCode::Esc => *free_text = None,
                            _ => {}
                        }
                    } else {
                        // Option-select sub-mode.
                        match key.code {
                            KeyCode::Up | KeyCode::Char('k') => {
                                *option = option.saturating_sub(1);
                            }
                            KeyCode::Down | KeyCode::Char('j')
                                if *option + 1 < opt_labels.len() =>
                            {
                                *option += 1;
                            }
                            KeyCode::Enter => {
                                if let Some(label) = opt_labels.get(*option) {
                                    submit_decision_answer(ctx.home, &id, label.clone(), items);
                                    *selected = (*selected).min(items.len().saturating_sub(1));
                                    *mode = DecisionMode::Browse;
                                }
                            }
                            // 'f' / 'e' switch to free-text entry when allowed.
                            KeyCode::Char('f') | KeyCode::Char('e') if allow_free => {
                                *free_text = Some(String::new());
                            }
                            KeyCode::Esc => *mode = DecisionMode::Browse,
                            _ => {}
                        }
                    }
                }
            }
        }
        Overlay::Tasks {
            ref mut items,
            ref mut col,
            ref mut row,
            ref mut mode,
            ref mut view,
            ref mut pending,
            ref mut notice,
        } => {
            // Tab switches between Tasks and Fleet views
            if key.code == KeyCode::Tab && matches!(mode, TaskBoardMode::Board) {
                *view = match view {
                    BoardView::Tasks => BoardView::Fleet,
                    BoardView::Fleet => BoardView::Status,
                    BoardView::Status => BoardView::Monitor,
                    BoardView::Monitor => BoardView::Tasks,
                };
                return outcome;
            }
            match mode {
                TaskBoardMode::Detail => {
                    if matches!(
                        key.code,
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace
                    ) {
                        *mode = TaskBoardMode::Board;
                    }
                }
                TaskBoardMode::NewTask { ref mut input } => match key.code {
                    KeyCode::Enter if !input.is_empty() => {
                        if submit_task_request(
                            ctx.task_rpc_tx,
                            serde_json::json!({
                                "action": "create",
                                "title": input.as_str(),
                                "priority": "normal",
                            }),
                            pending,
                            notice,
                        ) {
                            *mode = TaskBoardMode::Board;
                        }
                    }
                    KeyCode::Char(c) => input.push(c),
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Esc => {
                        *mode = TaskBoardMode::Board;
                    }
                    _ => {}
                },
                TaskBoardMode::Assign {
                    ref choices,
                    ref mut selected,
                    ref task_id,
                } => match key.code {
                    KeyCode::Up | KeyCode::Char('k') if *selected > 0 => {
                        *selected -= 1;
                    }
                    KeyCode::Down | KeyCode::Char('j') if *selected + 1 < choices.len() => {
                        *selected += 1;
                    }
                    KeyCode::Enter if !choices.is_empty() => {
                        // M3: use captured task_id instead of col/row indices
                        let assignee = &choices[*selected].1;
                        if submit_task_request(
                            ctx.task_rpc_tx,
                            serde_json::json!({
                                "action": "update",
                                "id": task_id,
                                "assignee": assignee,
                            }),
                            pending,
                            notice,
                        ) {
                            *mode = TaskBoardMode::Board;
                        }
                    }
                    KeyCode::Esc => {
                        *mode = TaskBoardMode::Board;
                    }
                    _ => {}
                },
                TaskBoardMode::Help => match key.code {
                    KeyCode::Esc | KeyCode::Char('?') => {
                        *mode = TaskBoardMode::Board;
                    }
                    _ => {}
                },
                TaskBoardMode::Board => {
                    let columns = crate::render::task_board_columns(items);
                    match key.code {
                        KeyCode::Left | KeyCode::Char('h')
                            if !key.modifiers.contains(KeyModifiers::SHIFT) && *col > 0 =>
                        {
                            *col -= 1;
                            *row = (*row).min(
                                crate::render::selectable_len(&columns, *col).saturating_sub(1),
                            );
                        }
                        KeyCode::Right | KeyCode::Char('l')
                            if !key.modifiers.contains(KeyModifiers::SHIFT) && *col < 4 =>
                        {
                            *col += 1;
                            *row = (*row).min(
                                crate::render::selectable_len(&columns, *col).saturating_sub(1),
                            );
                        }
                        KeyCode::Up | KeyCode::Char('k') if *row > 0 => {
                            *row -= 1;
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            let col_len = crate::render::selectable_len(&columns, *col);
                            if *row + 1 < col_len {
                                *row += 1;
                            }
                        }
                        KeyCode::Enter if !columns[*col].is_empty() => {
                            *mode = TaskBoardMode::Detail;
                        }
                        // n — new task
                        KeyCode::Char('n') => {
                            *mode = TaskBoardMode::NewTask {
                                input: String::new(),
                            };
                        }
                        // d — cancel task
                        KeyCode::Char('d')
                            if !key.modifiers.contains(KeyModifiers::SHIFT)
                                && !columns[*col].is_empty() =>
                        {
                            if let Some(task) = columns[*col].get(*row) {
                                submit_task_request(
                                    ctx.task_rpc_tx,
                                    serde_json::json!({
                                        "action": "update",
                                        "id": task.id,
                                        "status": "cancelled",
                                    }),
                                    pending,
                                    notice,
                                );
                            }
                        }
                        // D (Shift+D) — mark task done from any column
                        // Match 'D' (legacy terminals) and 'd'+SHIFT (Kitty protocol)
                        KeyCode::Char('D') | KeyCode::Char('d') if !columns[*col].is_empty() => {
                            if let Some(task) = columns[*col].get(*row) {
                                submit_task_request(
                                    ctx.task_rpc_tx,
                                    serde_json::json!({
                                        "action": "done",
                                        "id": task.id,
                                    }),
                                    pending,
                                    notice,
                                );
                            }
                        }
                        // a — assign
                        KeyCode::Char('a') if !columns[*col].is_empty() => {
                            // M3: capture task_id now to avoid col/row drift
                            let target_task_id = columns[*col]
                                .get(*row)
                                .map(|t| t.id.clone())
                                .unwrap_or_default();
                            let mut choices: Vec<(String, String)> = Vec::new();
                            let mut seen = std::collections::HashSet::new();
                            // Teams
                            let team_list = crate::teams::list(ctx.home);
                            if let Some(teams) = team_list["teams"].as_array() {
                                for t in teams {
                                    if let Some(name) = t["name"].as_str() {
                                        choices.push((format!("🏷 {name}"), name.to_string()));
                                        seen.insert(name.to_string());
                                    }
                                }
                            }
                            // All instances (from metadata dir)
                            let meta_dir = ctx.home.join("metadata");
                            if let Ok(entries) = std::fs::read_dir(&meta_dir) {
                                for entry in entries.flatten() {
                                    if let Some(name) =
                                        entry.path().file_stem().and_then(|s| s.to_str())
                                    {
                                        if !seen.contains(name) {
                                            choices.push((name.to_string(), name.to_string()));
                                            seen.insert(name.to_string());
                                        }
                                    }
                                }
                            }
                            if choices.is_empty() {
                                choices.push(("(no agents/teams)".to_string(), String::new()));
                            }
                            *mode = TaskBoardMode::Assign {
                                choices,
                                selected: 0,
                                task_id: target_task_id,
                            };
                        }
                        // Shift+← / Shift+→ — move task status
                        // Match 'H' (legacy) and 'h'+SHIFT (Kitty protocol)
                        KeyCode::Char('H') | KeyCode::Char('h')
                            if !columns[*col].is_empty() && *col > 0 =>
                        {
                            if let Some(task) = columns[*col].get(*row) {
                                // #2306: regress along the progress flow
                                // Todo←Working←Review←Done; a Blocked task
                                // un-blocks back to Working (in_progress). Pure
                                // canonical-status updates — no enum/automation change.
                                let new_status = match *col {
                                    1 => Some("open"),        // Working → Todo
                                    2 => Some("in_progress"), // Review → Working
                                    3 => Some("in_progress"), // Blocked → Working (unblock)
                                    4 => Some("in_review"),   // Done → Review
                                    _ => None,
                                };
                                if let Some(val) = new_status {
                                    submit_task_request(
                                        ctx.task_rpc_tx,
                                        serde_json::json!({"action": "update", "id": task.id, "status": val}),
                                        pending,
                                        notice,
                                    );
                                    // Follow the task to whichever column its NEW
                                    // status lands in (column order != status order,
                                    // so a fixed ±1 would mis-track — e.g. Blocked).
                                    *col = crate::render::board_column_for_status(val);
                                }
                            }
                        }
                        // Match 'L' (legacy) and 'l'+SHIFT (Kitty protocol)
                        KeyCode::Char('L') | KeyCode::Char('l')
                            if !columns[*col].is_empty() && *col < 4 =>
                        {
                            if let Some(task) = columns[*col].get(*row) {
                                // #2306: advance along the progress flow
                                // Todo→Working→Review→Done; a Blocked task
                                // un-blocks back to Working (in_progress).
                                let new_status = match *col {
                                    0 => Some("in_progress"), // Todo → Working
                                    1 => Some("in_review"),   // Working → Review
                                    2 => Some("done"),        // Review → Done
                                    3 => Some("in_progress"), // Blocked → Working (unblock)
                                    _ => None,
                                };
                                if let Some(val) = new_status {
                                    submit_task_request(
                                        ctx.task_rpc_tx,
                                        serde_json::json!({"action": "update", "id": task.id, "status": val}),
                                        pending,
                                        notice,
                                    );
                                    *col = crate::render::board_column_for_status(val);
                                }
                            }
                        }
                        KeyCode::Char('?') => {
                            *mode = TaskBoardMode::Help;
                        }
                        // #1202: t/f/s/m switch between views within the overlay
                        KeyCode::Char('t') => {
                            *view = BoardView::Tasks;
                        }
                        KeyCode::Char('f') | KeyCode::Char('F') => {
                            *view = BoardView::Fleet;
                        }
                        KeyCode::Char('s') => {
                            *view = BoardView::Status;
                        }
                        KeyCode::Char('m') | KeyCode::Char('M') => {
                            *view = BoardView::Monitor;
                        }
                        KeyCode::Esc | KeyCode::Char('q') => {
                            *overlay = Overlay::None;
                        }
                        _ => {}
                    }
                }
            }
        }
        Overlay::ScratchShell { pane } => match key.code {
            KeyCode::Esc => {
                // Capture the registry key before dropping the pane so we can
                // kill the shell process. The registry owns the PTY master;
                // removing it drops the handle, the forwarder sees rx close,
                // and exits. The pane (and its subscriber rx) drop with the
                // overlay.
                let instance_id = pane.instance_id;
                *overlay = Overlay::None;
                super::kill_unmanaged_agent(ctx.registry, instance_id, &mut *ctx.reap_workers);
            }
            _ => {
                let bytes = crate::tui::key_to_bytes(key.code, key.modifiers);
                if !bytes.is_empty() {
                    pane.write_input(ctx.registry, &bytes);
                }
            }
        },
        Overlay::None => {}
    }
    outcome
}

fn authoritative_instance_ref_for_delete(
    ctx: &OverlayCtx<'_>,
    name: &str,
) -> Option<crate::types::InstanceRef> {
    crate::agent::instance_ref_for_name(ctx.registry, ctx.home, name)
        .or_else(|| ctx.layout.unique_instance_ref_for_agent(name))
}

enum DeleteCompletion {
    Close,
    Keep { notice: String },
}

fn finish_successful_delete(
    ctx: &mut OverlayCtx<'_>,
    name: &str,
    expected_ref: Option<crate::types::InstanceRef>,
) -> DeleteCompletion {
    let Some(expected_ref) = expected_ref else {
        return DeleteCompletion::Keep {
            notice: "delete completed but instance identity is unavailable; view retained"
                .to_string(),
        };
    };

    ctx.layout.remove_fleet_instance_views_exact(expected_ref);
    if !super::session::save_session(ctx.home, ctx.layout)
        && !super::session::record_retired_ref(ctx.home, expected_ref)
    {
        tracing::error!(
            name = %name,
            "delete completed but session retirement is pending retry"
        );
        return DeleteCompletion::Keep {
            notice: "delete completed; session retirement pending retry".to_string(),
        };
    }
    DeleteCompletion::Close
}

fn submit_task_request(
    tx: &crossbeam_channel::Sender<super::rpc::TaskRequest>,
    arguments: serde_json::Value,
    pending: &mut bool,
    notice: &mut Option<String>,
) -> bool {
    if *pending {
        *notice = Some("task request already pending".to_string());
        return false;
    }
    match tx.try_send(super::rpc::TaskRequest::Mutate(arguments)) {
        Ok(()) => {
            *pending = true;
            *notice = None;
            true
        }
        Err(crossbeam_channel::TrySendError::Full(_)) => {
            *notice = Some("task request already pending".to_string());
            false
        }
        Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
            *notice = Some("task RPC worker stopped".to_string());
            false
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod organizer_tests;

#[cfg(test)]
mod review_repro_app_tui;
