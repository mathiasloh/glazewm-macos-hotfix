use tokio::sync::mpsc;
use wm_common::{
  ContainerDto, InvokeCommand, WindowMatchConfig, WindowRuleConfig,
};
use wm_platform::{Dispatcher, WindowId};

use super::*;
use crate::{
  commands::container::detach_container,
  models::{SplitContainer, Workspace},
  traits::TilingSizeGetters,
};

/// Creates a root/workspace fixture without a native monitor or OS calls.
fn fixture() -> (WmState, mpsc::UnboundedReceiver<WmEvent>, Workspace) {
  let (events, receiver) = mpsc::unbounded_channel();
  let (exit, _) = mpsc::unbounded_channel();
  let (restart, _) = mpsc::unbounded_channel();
  let mut state = WmState::new(Dispatcher::mock(), events, exit, restart);
  let workspace = Workspace::mock().call();
  attach_container(
    &workspace.clone().into(),
    &state.root_container.clone().into(),
    None,
  )
  .expect("Attach workspace.");
  state.initialize_for_testing();
  (state, receiver, workspace)
}

/// Creates a distinct identity, never a real operator window.
fn native(id: u32) -> NativeWindow {
  NativeWindow::mock_with_id(WindowId(id))
}

/// Attaches a mock without invoking native management or monitor queries.
fn attach(window: &WindowContainer, workspace: &Workspace) {
  attach_container(
    &window.clone().into(),
    &workspace.clone().into(),
    None,
  )
  .expect("Attach window.");
}

/// Removes the fixture before `WmState::drop` can attempt a native redraw.
fn clear(workspace: Workspace) {
  detach_container(workspace.into()).expect("Detach fixture.");
}

/// Adds a safe native monitor boundary for real ordinary admission.
fn admission_fixture() -> (WmState, Workspace, Monitor) {
  let (state, _, workspace) = fixture();
  detach_container(workspace.clone().into()).expect("Reparent workspace.");
  let monitor = Monitor::mock().workspaces(vec![workspace.clone()]).call();
  state.dispatcher.mock_set_nearest_display(monitor.native());
  attach_container(
    &monitor.clone().into(),
    &state.root_container.clone().into(),
    None,
  )
  .expect("Attach monitor.");
  (state, workspace, monitor)
}

/// Ordinary admission must not allocate both an ineligible groupless tab
/// and its newly shown successor while waiting for periodic cleanup.
#[test]
fn ordinary_admission_drops_stale_groupless_allocation_before_sync() {
  let (mut state, workspace, monitor) = admission_fixture();
  let old = NativeWindow::mock_tab(WindowId(1), 7, None);
  let stale: WindowContainer =
    TilingWindow::mock().native(old.clone()).call().into();
  attach(&stale, &workspace);
  for id in [2, 3] {
    attach(
      &TilingWindow::mock()
        .native(NativeWindow::mock_tab(WindowId(id), 8, None))
        .call()
        .into(),
      &workspace,
    );
  }
  set_focused_descendant(&stale.clone().into(), None);
  // AX lifetime survives, but the ordinary cleanup predicate rejects A.
  // No group, owner, title, or frame correlation proves A and B related.
  old.mock_set_valid(false);
  let candidate = NativeWindow::mock_tab(WindowId(4), 7, Some(9));
  state
    .dispatcher
    .mock_set_focused_window(Some(candidate.clone()));
  let mut config = UserConfig::for_testing(vec![]);
  config.value.general.cursor_jump.enabled = false;
  manage_window(candidate.clone(), None, &mut state, &mut config)
    .expect("Admit B through the real ordinary creation path.");
  crate::commands::general::platform_sync(&mut state, &config)
    .expect("Flush the admission redraw without a cleanup timer.");
  let admitted = state.window_from_native(&candidate).is_some();
  let stale_allocated = state.window_from_native(&old).is_some();
  let allocations = workspace
    .tiling_children()
    .map(|window| window.to_rect().expect("Allocated tile."))
    .collect::<Vec<_>>();
  let synced = candidate.mock_operations();
  clear(workspace);
  detach_container(monitor.into()).expect("Detach monitor.");
  assert!(admitted, "The fixture must reach ordinary B admission.");
  assert!(
    synced.iter().any(|operation| operation == "set_frame"),
    "The fixture must reach the native admission redraw."
  );
  assert_eq!(
    allocations.len(),
    3,
    "Single admission/sync allocated a blank fourth tile: {allocations:?}"
  );
  assert!(!stale_allocated, "Ineligible A still owns a layout slot.");
}

/// Cleanup discovered during admission reserves a positively grouped old
/// binding and defers unproven same-owner activity instead of stealing it.
#[test]
fn ordinary_admission_reserves_grouped_binding_without_unproven_replacement(
) {
  let (mut state, workspace, monitor) = admission_fixture();
  let old = NativeWindow::mock_tab(WindowId(1), 7, Some(9));
  let slot: WindowContainer =
    TilingWindow::mock().native(old.clone()).call().into();
  attach(&slot, &workspace);
  set_focused_descendant(&slot.clone().into(), None);
  old.mock_set_valid(false);
  let candidate = NativeWindow::mock_tab(WindowId(2), 7, Some(10));
  state
    .dispatcher
    .mock_set_focused_window(Some(candidate.clone()));
  manage_window(
    candidate.clone(),
    None,
    &mut state,
    &mut UserConfig::for_testing(vec![]),
  )
  .expect("Unproven admission.");
  let retained = state.window_from_native(&old) == Some(slot.clone());
  let admitted = state.window_from_native(&candidate).is_some();
  let reserved = state.native_binding_reserved(&slot);
  let deferred = state.deferred_native_tab_windows.clone();
  clear(workspace);
  detach_container(monitor.into()).expect("Detach monitor.");
  assert!(
    retained && reserved,
    "Positive group proof must retain its slot."
  );
  assert!(
    !admitted,
    "Unproven same-owner candidate must not steal a slot."
  );
  assert_eq!(deferred, vec![candidate]);
}

/// Ordinary cleanup uses native validity, not minimization or workspace
/// visibility, when admitting an unrelated new window.
#[test]
fn ordinary_admission_retains_valid_minimized_and_offworkspace_windows() {
  let (mut state, workspace, monitor) = admission_fixture();
  let minimized: WindowContainer = NonTilingWindow::mock()
    .native(NativeWindow::mock_tab(WindowId(1), 7, None))
    .state(WindowState::Minimized)
    .call()
    .into();
  attach(&minimized, &workspace);
  let hidden = Workspace::mock().name("2".into()).call();
  attach_container(&hidden.clone().into(), &monitor.clone().into(), None)
    .expect("Attach inactive workspace.");
  let offworkspace: WindowContainer = TilingWindow::mock()
    .native(NativeWindow::mock_tab(WindowId(2), 7, None))
    .call()
    .into();
  attach(&offworkspace, &hidden);
  set_focused_descendant(&workspace.clone().into(), None);
  assert!(
    !hidden.is_displayed(),
    "Fixture must use an inactive workspace."
  );
  let candidate = NativeWindow::mock_tab(WindowId(3), 8, None);
  state
    .dispatcher
    .mock_set_focused_window(Some(candidate.clone()));
  manage_window(
    candidate.clone(),
    None,
    &mut state,
    &mut UserConfig::for_testing(vec![]),
  )
  .expect("Ordinary admission.");
  let minimized_retained = state.window_from_native(&minimized.native())
    == Some(minimized.clone());
  let hidden_retained = state.window_from_native(&offworkspace.native())
    == Some(offworkspace.clone());
  let admitted = state.window_from_native(&candidate).is_some();
  clear(workspace);
  clear(hidden);
  detach_container(monitor.into()).expect("Detach monitor.");
  assert!(admitted, "The new eligible window must still be admitted.");
  assert!(minimized_retained && hidden_retained);
  assert_eq!(minimized.state(), WindowState::Minimized);
}

/// Tests nested 70/30 layout retention, including sibling and focus order.
#[test]
fn native_tab_replacement_preserves_nested_layout_and_native_lookup() {
  let (state, mut events, workspace) = fixture();
  let old = TilingWindow::mock()
    .native(native(1))
    .process_name("Finder".into())
    .call();
  let sibling = TilingWindow::mock().native(native(2)).call();
  let split = SplitContainer::mock()
    .tiling_containers(vec![sibling.clone().into(), old.clone().into()])
    .call();
  let outside = TilingWindow::mock().native(native(3)).call();
  attach_container(&split.clone().into(), &workspace.clone().into(), None)
    .expect("Attach split.");
  attach(&outside.clone().into(), &workspace);
  old.set_tiling_size(0.7);
  sibling.set_tiling_size(0.3);
  set_focused_descendant(&old.clone().into(), None);
  let order = split.children();
  let focus = split.borrow_child_focus_order().clone();
  let outer_order = workspace.children();
  let old_id = old.id();
  let candidate = native(4);
  let properties = NativeWindowProperties::mock()
    .title("New tab".into())
    .process_name("Finder".into())
    .call();
  let config = UserConfig::for_testing(vec![]);

  let replaced = replace_native_tab(
    &candidate,
    &properties,
    &state,
    &config,
    true,
    |window| window.id() != WindowId(1),
    |window| window.id() == WindowId(1),
  )
  .expect("Unique inactive tab.");

  assert_eq!(replaced.id(), old_id);
  assert_eq!(old.parent(), Some(split.clone().into()));
  assert_eq!(old.index(), 1);
  assert_eq!(split.children(), order);
  assert_eq!(*split.borrow_child_focus_order(), focus);
  assert_eq!(workspace.children(), outer_order);
  assert_eq!(old.tiling_size(), 0.7);
  assert_eq!(sibling.tiling_size(), 0.3);
  assert_eq!(state.windows().len(), 3);
  assert_eq!(
    state
      .windows()
      .iter()
      .filter(|w| w.native_properties().process_name == "Finder")
      .count(),
    1
  );
  assert!(state.window_from_native(&native(1)).is_none());
  assert_eq!(state.window_from_native(&candidate), Some(replaced));
  assert_eq!(old.native_properties().title, "New tab");
  assert!(events.try_recv().is_err());
  clear(workspace);
}

/// Publishes the new identity only after replacing the old entry in place.
#[test]
fn native_tab_replacement_emits_one_managed_dto_and_repeated_admission_is_noop(
) {
  let (mut state, mut events, workspace) = fixture();
  let old: WindowContainer = NonTilingWindow::mock()
    .native(native(1))
    .process_name("Finder".into())
    .call()
    .into();
  attach(&old, &workspace);
  let id = old.id();
  let candidate = native(2);
  let properties = NativeWindowProperties::mock()
    .title("Active tab".into())
    .process_name("Finder".into())
    .call();
  let mut config = UserConfig::for_testing(vec![]);
  let window = replace_native_tab(
    &candidate,
    &properties,
    &state,
    &config,
    true,
    |_| false,
    |_| true,
  )
  .expect("Replace tab.");
  complete_window_management(window, &mut state, &mut config)
    .expect("Publish admission.");
  assert_eq!(state.windows().len(), 1);
  let WmEvent::WindowManaged {
    managed_window: ContainerDto::Window(dto),
  } = events.try_recv().expect("Managed event.")
  else {
    panic!("Expected window DTO.");
  };
  assert_eq!(dto.id, id);
  assert_eq!(dto.handle, 2);
  assert_eq!(dto.title, "Active tab");
  assert_eq!(dto.process_name, "Finder");
  assert!(replace_native_tab(
    &candidate,
    &properties,
    &state,
    &config,
    true,
    |_| false,
    |_| true
  )
  .is_none());
  assert_eq!(state.windows().len(), 1);
  assert!(events.try_recv().is_err());
  clear(workspace);
}

/// Missing/different groups, eligible old windows, and non-active
/// candidates must leave the existing layout and identity for normal
/// admission.
#[test]
fn native_tab_replacement_falls_back_without_proven_unique_inactive_group()
{
  for (active, old_valid, same_group) in [
    (true, false, false),
    (true, true, true),
    (false, false, true),
  ] {
    let (state, mut events, workspace) = fixture();
    let old: WindowContainer =
      TilingWindow::mock().native(native(1)).call().into();
    attach(&old, &workspace);
    let config = UserConfig::for_testing(vec![]);
    let properties = NativeWindowProperties::mock().call();
    assert!(replace_native_tab(
      &native(2),
      &properties,
      &state,
      &config,
      active,
      |_| old_valid,
      |_| same_group
    )
    .is_none());
    assert_eq!(old.native().id(), WindowId(1));
    assert_eq!(workspace.child_count(), 1);
    assert!(events.try_recv().is_err());
    clear(workspace);
  }
}

/// Two cached old matches are insufficient proof of a replacement target.
#[test]
fn native_tab_replacement_rejects_ambiguous_group() {
  let (state, _, workspace) = fixture();
  for id in [1, 2] {
    attach(
      &TilingWindow::mock().native(native(id)).call().into(),
      &workspace,
    );
  }
  let config = UserConfig::for_testing(vec![]);
  assert!(replace_native_tab(
    &native(3),
    &NativeWindowProperties::mock().call(),
    &state,
    &config,
    true,
    |_| false,
    |_| true
  )
  .is_none());
  assert_eq!(
    state
      .windows()
      .iter()
      .map(|w| w.native().id())
      .collect::<Vec<_>>(),
    vec![WindowId(1), WindowId(2)]
  );
  clear(workspace);
}

/// Both explicit ignores and matching Manage Ignore rules exclude swaps.
#[test]
fn native_tab_replacement_does_not_steal_slot_for_ignored_candidate() {
  for rule_ignore in [false, true] {
    let (mut state, mut events, workspace) = fixture();
    let old: WindowContainer =
      TilingWindow::mock().native(native(1)).call().into();
    attach(&old, &workspace);
    let candidate = native(2);
    let rules = if rule_ignore {
      vec![WindowRuleConfig {
        commands: vec![InvokeCommand::Ignore],
        match_window: vec![WindowMatchConfig::default()],
        on: vec![WindowRuleEvent::Manage],
        run_once: true,
      }]
    } else {
      state.ignored_windows.push(candidate.clone());
      vec![]
    };
    let config = UserConfig::for_testing(rules);
    assert!(replace_native_tab(
      &candidate,
      &NativeWindowProperties::mock().call(),
      &state,
      &config,
      true,
      |_| false,
      |_| true
    )
    .is_none());
    assert_eq!(old.native().id(), WindowId(1));
    assert_eq!(workspace.child_count(), 1);
    assert!(events.try_recv().is_err());
    clear(workspace);
  }
}

/// Deferred inactive admission publishes once without replacing newer WM
/// focus.
#[test]
fn deferred_admission_completion_preserves_newer_focus() {
  let (mut state, mut events, workspace) = fixture();
  let focused: WindowContainer =
    NonTilingWindow::mock().native(native(1)).call().into();
  let deferred: WindowContainer =
    NonTilingWindow::mock().native(native(2)).call().into();
  attach(&focused, &workspace);
  attach(&deferred, &workspace);
  set_focused_descendant(&focused.clone().into(), None);
  let focus_order = workspace.borrow_child_focus_order().clone();
  let mut config = UserConfig::for_testing(vec![]);
  complete_window_management_with_focus(
    deferred.clone(),
    &mut state,
    &mut config,
    false,
  )
  .expect("Deferred completion.");
  assert!(focused.has_focus(None));
  assert_eq!(*workspace.borrow_child_focus_order(), focus_order);
  assert!(!state.pending_sync.needs_focus_update());
  assert!(!state.pending_sync.needs_focused_effect_update());
  assert!(
    matches!(events.try_recv(), Ok(WmEvent::WindowManaged { managed_window: ContainerDto::Window(dto) }) if dto.id == deferred.id())
  );
  assert!(events.try_recv().is_err());
  clear(workspace);
}

/// Manage rules match refreshed properties and fresh run-once history.
#[test]
fn native_tab_replacement_runs_manage_rules_for_new_properties() {
  let (mut state, _, workspace) = fixture();
  let old: WindowContainer =
    NonTilingWindow::mock().native(native(1)).call().into();
  attach(&old, &workspace);
  let rule = WindowRuleConfig {
    commands: vec![],
    match_window: vec![WindowMatchConfig {
      window_title: Some(wm_common::MatchType::Equals {
        equals: "New".into(),
      }),
      ..Default::default()
    }],
    on: vec![WindowRuleEvent::Manage],
    run_once: true,
  };
  old.set_done_window_rules(vec![rule.clone()]);
  let mut config = UserConfig::for_testing(vec![rule.clone()]);
  let properties = NativeWindowProperties::mock()
    .title("New".into())
    .process_name("Finder".into())
    .call();
  let window = replace_native_tab(
    &native(2),
    &properties,
    &state,
    &config,
    true,
    |_| false,
    |_| true,
  )
  .expect("Replace tab.");
  assert!(window.done_window_rules().is_empty());
  complete_window_management(window.clone(), &mut state, &mut config)
    .expect("Run admission rules.");
  assert_eq!(window.done_window_rules(), vec![rule]);
  clear(workspace);
}
