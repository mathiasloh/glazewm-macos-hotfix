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
