use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use wm_common::{
  ContainerDto, InvokeCommand, WindowMatchConfig, WindowRuleConfig,
};
use wm_platform::{
  Dispatcher, NativeWindow, WindowEvent, WindowEventNotification, WindowId,
};

use super::*;
use crate::{
  commands::{
    container::{
      attach_container, detach_container, set_focused_descendant,
    },
    general::platform_sync,
  },
  models::{Monitor, NonTilingWindow, Workspace},
  traits::PositionGetters,
  wm::WindowManager,
};

/// Builds a root/workspace fixture without native monitors or operator
/// windows. Nested tiling retention is covered separately by
/// `manage_window_tab_tests`.
fn fixture() -> (
  WindowManager,
  UserConfig,
  Workspace,
  WindowContainer,
  WindowContainer,
) {
  let (tx, mut rx) = mpsc::unbounded_channel();
  let (exit, exit_rx) = mpsc::unbounded_channel();
  let (restart, restart_keybinding_rx) = mpsc::unbounded_channel();
  let mut state = WmState::new(Dispatcher::mock(), tx, exit, restart);
  let old = NativeWindow::mock_tab(WindowId(1), 7, Some(9));
  state.dispatcher.mock_set_focused_window(Some(old.clone()));
  let inactive = NativeWindow::mock_tab(WindowId(9), 7, Some(9));
  inactive.mock_set_valid(false);
  let slot: WindowContainer =
    NonTilingWindow::mock().native(inactive).call().into();
  let sibling: WindowContainer = NonTilingWindow::mock()
    .native(NativeWindow::mock_tab(WindowId(2), 8, None))
    .call()
    .into();
  let workspace = Workspace::mock().call();
  attach_container(
    &workspace.clone().into(),
    &state.root_container.clone().into(),
    None,
  )
  .expect("Workspace.");
  attach_container(
    &sibling.clone().into(),
    &workspace.clone().into(),
    None,
  )
  .expect("Sibling.");
  attach_container(&slot.clone().into(), &workspace.clone().into(), None)
    .expect("Slot.");
  set_focused_descendant(&slot.clone().into(), None);
  state.initialize_for_testing();
  // Event assertions use live bookkeeping. Tests opt into pause
  // explicitly.
  let mut config = UserConfig::for_testing(vec![]);
  let id = slot.id();
  crate::commands::window::manage_window(
    old,
    None,
    &mut state,
    &mut config,
  )
  .expect("A to B in U.");
  assert_eq!(slot.id(), id);
  assert_eq!(slot.native().id(), WindowId(1));
  state.pending_sync.clear();
  while rx.try_recv().is_ok() {}
  (
    WindowManager {
      state,
      event_rx: rx,
      exit_rx,
      restart_keybinding_rx,
    },
    config,
    workspace,
    slot,
    sibling,
  )
}

/// Runs destruction without a successor event. Unpaused fixtures stop at
/// the real handler before monitor-dependent sync; paused fixtures
/// exercise the complete event wrapper without disabling bookkeeping.
fn destroy(wm: &mut WindowManager, config: &mut UserConfig) {
  if wm.state.is_paused {
    wm.process_event(
      wm_platform::PlatformEvent::Window(WindowEvent::Destroyed {
        window_id: WindowId(1),
        notification: WindowEventNotification(None),
      }),
      config,
    )
  } else {
    super::destroyed(WindowId(1), &mut wm.state, config)
  }
  .expect("Destruction.");
}

/// Requires a matching replacement DTO and no premature unmanaged event.
fn assert_replacement_events(
  wm: &mut WindowManager,
  slot: &WindowContainer,
) {
  assert!(
    !wm.state.is_paused,
    "Event assertions require enabled emission."
  );
  let mut managed = 0;
  while let Ok(event) = wm.event_rx.try_recv() {
    match event {
      WmEvent::WindowUnmanaged { .. } => {
        panic!("Replacement emitted Unmanaged.")
      }
      WmEvent::WindowManaged {
        managed_window: ContainerDto::Window(dto),
      } => {
        assert_eq!(dto.id, slot.id());
        assert_eq!(dto.handle, slot.native().id().0 as isize);
        managed += 1;
      }
      _ => {}
    }
  }
  assert_eq!(managed, 1);
}

/// Clears fixtures before shutdown restoration touches native APIs.
fn clear(workspace: Workspace) {
  detach_container(workspace.into()).expect("Detach workspace.");
}

/// Fresh group lookup is separate from the identity retained while
/// inactive.
#[test]
fn native_group_query_changes_only_the_eligible_binding() {
  let window = NativeWindow::mock_tab(WindowId(1), 7, Some(9));
  let peer = NativeWindow::mock_tab(WindowId(2), 7, Some(9));
  window.mock_set_valid(false);
  window.mock_set_queried_tab_group(None);
  window.refresh_tab_group();
  assert!(window.has_same_tab_group(&peer));
  window.mock_set_valid(true);
  window.refresh_tab_group();
  assert!(!window.has_same_tab_group(&peer));
  window.mock_set_queried_tab_group(Some(10));
  window.refresh_tab_group();
  let detached = NativeWindow::mock_tab(WindowId(3), 7, Some(10));
  assert!(window.has_same_tab_group(&detached));
  assert!(!window.has_same_tab_group(&peer));
}

/// Closing B after A/B switches retains A's slot without a fresh tab
/// group. Destruction may precede A's notification, or A may emit no
/// notification.
#[test]
fn finder_last_tab_close_retains_visited_survivor_without_fresh_group() {
  for (immediate, successor_event) in
    [(true, false), (true, true), (false, false), (false, true)]
  {
    let (mut wm, mut config, workspace, slot, _) = fixture();
    let b = slot.native().clone();
    let a = NativeWindow::mock_tab(WindowId(9), 7, Some(9));
    let id = slot.id();
    let parent = slot.parent();
    let order = workspace.children();
    // The fixture has already admitted B into A's slot. Switch B -> A ->
    // B.
    for (inactive, active) in [(&b, &a), (&a, &b)] {
      inactive.mock_set_valid(false);
      active.mock_set_valid(true);
      wm.state
        .dispatcher
        .mock_set_focused_window(Some(active.clone()));
      crate::commands::window::manage_window(
        active.clone(),
        None,
        &mut wm.state,
        &mut config,
      )
      .expect("Switch an existing Finder tab.");
      assert_eq!(slot.id(), id);
      assert_eq!(slot.native().id(), active.id());
    }
    wm.state.pending_sync.clear();
    while wm.event_rx.try_recv().is_ok() {}
    b.mock_set_valid(false);
    a.mock_set_valid(true);
    a.mock_set_queried_tab_group(None);
    b.mock_set_owner_windows(if immediate {
      vec![a.clone()]
    } else {
      vec![]
    });
    wm.state.dispatcher.mock_set_focused_window(Some(a.clone()));
    super::destroyed(b.id(), &mut wm.state, &mut config)
      .expect("Close only B.");
    b.mock_set_owner_windows(vec![a.clone()]);
    if successor_event {
      // Pausing avoids monitor-dependent sync, not event bookkeeping.
      wm.state.is_paused = true;
      wm.process_event(
        wm_platform::PlatformEvent::Window(WindowEvent::Shown {
          window: a.clone(),
          notification: WindowEventNotification(None),
        }),
        &mut config,
      )
      .expect("Survivor activity after destruction.");
      wm.state.is_paused = false;
    }
    reconcile_due(
      &mut wm.state,
      &mut config,
      Instant::now() + Duration::from_millis(500),
    )
    .expect("Resolve close without another native event.");
    let retained = wm.state.container_by_id(id);
    // Detach before assertions so a failing regression cannot touch OS
    // APIs.
    let actual_order = workspace.children();
    let actual_parent = slot.parent();
    let windows = wm.state.windows();
    let retained_focus = slot.has_focus(None);
    clear(workspace);
    assert!(retained.is_some(), "Surviving Finder lost its layout slot.");
    assert_eq!(slot.native().id(), a.id());
    assert_eq!(actual_parent, parent);
    assert_eq!(actual_order, order);
    assert_eq!(windows.len(), 2);
    assert!(retained_focus, "Closing a tab moved focus to its sibling.");
    assert!(!wm.state.pending_sync.needs_focus_update());
    assert_replacement_events(&mut wm, &slot);
  }
}

/// Survivor activity can recover its visited slot before late destruction.
#[test]
fn groupless_survivor_activity_before_destroy_retains_visited_slot() {
  let (mut wm, mut config, workspace, slot, _) = fixture();
  let b = slot.native().clone();
  let a = NativeWindow::mock_tab(WindowId(9), 7, None);
  let id = slot.id();
  let order = workspace.children();
  b.mock_set_valid(false);
  wm.state.dispatcher.mock_set_focused_window(Some(a.clone()));
  crate::commands::window::manage_window(
    a.clone(),
    None,
    &mut wm.state,
    &mut config,
  )
  .expect("A activity precedes destruction.");
  super::destroyed(b.id(), &mut wm.state, &mut config)
    .expect("Late B destruction.");
  let count = wm.state.windows().len();
  let actual_order = workspace.children();
  clear(workspace);
  assert_eq!(slot.id(), id);
  assert_eq!(slot.native().id(), a.id());
  assert_eq!(actual_order, order);
  assert_eq!(count, 2);
}

/// Visit proof never authorizes a different group, owner, or unknown
/// window.
#[test]
fn visited_tab_recovery_rejects_unrelated_and_ambiguous_survivors() {
  for case in [
    "unknown",
    "owner",
    "group",
    "ignored",
    "ambiguous",
    "closed",
  ] {
    let (mut wm, mut config, workspace, slot, _) = fixture();
    let b = slot.native().clone();
    let visited = NativeWindow::mock_tab(WindowId(9), 7, Some(9));
    let candidate = NativeWindow::mock_tab(
      WindowId(if case == "unknown" { 10 } else { 9 }),
      if case == "owner" { 8 } else { 7 },
      if case == "group" { Some(10) } else { None },
    );
    if case == "ignored" {
      wm.state.ignored_windows.push(candidate.clone());
    }
    let mut inventory = vec![candidate];
    if case == "ambiguous" {
      // Both native IDs have positively occupied this same group.
      b.mock_set_valid(false);
      wm.state
        .dispatcher
        .mock_set_focused_window(Some(visited.clone()));
      crate::commands::window::manage_window(
        visited.clone(),
        None,
        &mut wm.state,
        &mut config,
      )
      .expect("Visit A.");
      visited.mock_set_valid(false);
      b.mock_set_valid(true);
      wm.state.dispatcher.mock_set_focused_window(Some(b.clone()));
      crate::commands::window::manage_window(
        b.clone(),
        None,
        &mut wm.state,
        &mut config,
      )
      .expect("Return to B.");
      inventory.push(NativeWindow::mock_tab(WindowId(11), 7, Some(9)));
    }
    if case == "closed" {
      inventory.clear();
    }
    b.mock_set_valid(false);
    b.mock_set_owner_windows(inventory);
    super::destroyed(b.id(), &mut wm.state, &mut config)
      .expect("Close B.");
    reconcile_due(
      &mut wm.state,
      &mut config,
      Instant::now() + Duration::from_millis(500),
    )
    .expect("Resolve rejected inventory.");
    let count = wm.state.windows().len();
    let pending = wm.state.pending_native_tabs.len();
    clear(workspace);
    assert_eq!(count, 1, "Unexpected successor for {case}.");
    assert_eq!(pending, 0);
  }
}

/// Eligible detachment or regrouping invalidates previously visited
/// identities.
#[test]
fn refreshed_group_change_discards_visited_native_identity() {
  for group in [None, Some(10)] {
    let current = NativeWindow::mock_tab(WindowId(1), 7, Some(9));
    let previous = NativeWindow::mock_tab(WindowId(9), 7, Some(9));
    current.remember_tab_group_member(&previous);
    previous.mock_set_tab_group(None);
    assert!(current.has_visited_tab(&previous));
    current.mock_set_queried_tab_group(group);
    current.refresh_tab_group();
    assert!(!current.has_visited_tab(&previous));
    // Rejoining the original group cannot resurrect discarded membership.
    current.mock_set_queried_tab_group(Some(9));
    current.refresh_tab_group();
    assert!(!current.has_visited_tab(&previous));
  }
}

/// A recovered last tab has no group reservation when its final window
/// closes.
#[test]
fn recovered_groupless_tab_final_close_removes_slot() {
  let (mut wm, mut config, workspace, slot, _) = fixture();
  let b = slot.native().clone();
  let a = NativeWindow::mock_tab(WindowId(9), 7, None);
  b.mock_set_valid(false);
  b.mock_set_owner_windows(vec![a.clone()]);
  super::destroyed(b.id(), &mut wm.state, &mut config).expect("Close B.");
  let recovered = slot.native().id();
  a.mock_set_valid(false);
  super::destroyed(a.id(), &mut wm.state, &mut config).expect("Close A.");
  let count = wm.state.windows().len();
  let pending = wm.state.pending_native_tabs.len();
  clear(workspace);
  assert_eq!(recovered, a.id());
  assert_eq!(count, 1);
  assert_eq!(pending, 0);
}

/// Owner inventory replaces in place even with stale global focus and no A
/// event.
#[test]
fn destruction_inventory_replaces_without_successor_event_or_focus_theft()
{
  for immediate in [false, true] {
    let (mut wm, mut config, workspace, slot, sibling) = fixture();
    let old = slot.native().clone();
    let candidate = NativeWindow::mock_tab(WindowId(3), 7, Some(9));
    let id = slot.id();
    let parent = slot.parent();
    let order = parent.as_ref().expect("Parent.").children();
    old.mock_set_valid(false);
    old.mock_set_owner_windows(if immediate {
      vec![candidate.clone()]
    } else {
      vec![]
    });
    destroy(&mut wm, &mut config);
    assert!(
      wm.state.container_by_id(id).is_some(),
      "Destroying the active tab removed the surviving tab's layout slot."
    );
    if !immediate {
      let started = wm.state.pending_native_tabs[0].started;
      destroy(&mut wm, &mut config);
      assert_eq!(wm.state.pending_native_tabs[0].started, started);
      // Cleanup and duplicate destruction must not remove or renew the
      // slot.
      wm.state
        .cleanup_invalid_windows_with(
          |window| window.native().id() != WindowId(1),
          |_| true,
        )
        .expect("Cleanup.");
      assert_eq!(slot.id(), id);
      // Execute the real command dispatcher before monitor-dependent sync.
      WindowManager::run_commands(
        &vec![InvokeCommand::WmDisableBindingMode {
          name: "unused-test-mode".into(),
        }],
        sibling.clone().into(),
        &mut wm.state,
        &mut config,
      )
      .expect("Unrelated command.");
      assert!(matches!(
        wm.event_rx.try_recv(),
        Ok(WmEvent::BindingModesChanged { .. })
      ));
      wm.state
        .dispatcher
        .mock_set_focused_window(Some(sibling.native().clone()));
      crate::events::handle_window_focused(
        &sibling.native(),
        &mut wm.state,
        &mut config,
      )
      .expect("Unrelated focus before sync.");
      old.mock_set_owner_windows(vec![candidate.clone()]);
      reconcile_due(
        &mut wm.state,
        &mut config,
        started + Duration::from_millis(500),
      )
      .expect("Deadline.");
      assert!(sibling.has_focus(None));
    }
    assert!(wm.state.pending_native_tabs.is_empty());
    assert_eq!(slot.id(), id);
    assert_eq!(slot.native().id(), candidate.id());
    assert_eq!(slot.native_properties().title, "Tab 3");
    assert_eq!(slot.native_properties().process_name, "Finder");
    assert_eq!(slot.parent(), parent);
    assert_eq!(slot.parent().expect("Parent.").children(), order);
    assert!(!wm.state.pending_sync.needs_focus_update());
    // A late destruction of B cannot remove its successor's U binding.
    destroy(&mut wm, &mut config);
    assert_eq!(slot.native().id(), candidate.id());
    assert_eq!(wm.state.windows().len(), 2);
    assert_replacement_events(&mut wm, &slot);
    clear(workspace);
  }
}

/// Cleanup racing ahead of Destroyed must run the same immediate
/// replacement.
#[test]
fn cleanup_before_destroy_preserves_queryable_successor() {
  for paused in [false, true] {
    let (mut wm, mut config, workspace, slot, _) = fixture();
    wm.state.is_paused = paused;
    let id = slot.id();
    slot.native().mock_set_valid(false);
    let candidate = NativeWindow::mock_tab(WindowId(3), 7, Some(9));
    slot
      .native()
      .mock_set_owner_windows(vec![candidate.clone()]);
    if paused {
      wm.cleanup_invalid_windows(&mut config)
        .expect("Paused cleanup recovery.");
    } else {
      // The same cleanup/probe sequence, stopping before monitor IO.
      wm.state.cleanup_invalid_windows().expect("Cleanup.");
      probe_new(&mut wm.state, &mut config, &[])
        .expect("Cleanup recovery.");
    }
    assert_eq!(slot.id(), id);
    assert_eq!(slot.native().id(), candidate.id());
    assert!(wm.state.pending_native_tabs.is_empty());
    destroy(&mut wm, &mut config);
    assert_eq!(wm.state.windows().len(), 2);
    if paused {
      assert!(
        wm.event_rx.try_recv().is_err(),
        "Pause suppresses IPC events."
      );
    } else {
      assert_replacement_events(&mut wm, &slot);
    }
    clear(workspace);
  }
}

/// Reserved dead windows cannot receive focus, cursor, redraw, or effects.
#[test]
fn reserved_window_is_skipped_by_platform_sync_and_cleanup() {
  let (mut wm, mut config, workspace, slot, sibling) = fixture();
  wm.state.is_paused = true;
  slot.native().mock_set_valid(false);
  destroy(&mut wm, &mut config);
  for event in [
    WindowEvent::Hidden {
      window: slot.native().clone(),
      notification: WindowEventNotification(None),
    },
    WindowEvent::Minimized {
      window: slot.native().clone(),
      notification: WindowEventNotification(None),
    },
    WindowEvent::MinimizeEnded {
      window: slot.native().clone(),
      notification: WindowEventNotification(None),
    },
    WindowEvent::MovedOrResized {
      window: slot.native().clone(),
      is_interactive_start: false,
      is_interactive_end: false,
      notification: WindowEventNotification(None),
    },
  ] {
    wm.process_event(
      wm_platform::PlatformEvent::Window(event),
      &mut config,
    )
    .expect("Late B activity.");
    assert_eq!(wm.state.pending_native_tabs.len(), 1);
    assert_eq!(wm.state.windows().len(), 2);
  }
  wm.state.pending_sync.queue_focus_change();
  wm.state.pending_sync.queue_cursor_jump();
  wm.state.pending_sync.queue_all_effects_update();
  wm.state
    .pending_sync
    .queue_container_to_redraw(slot.clone());
  config.value.general.cursor_jump.enabled = true;
  platform_sync(&mut wm.state, &config).expect("Reserved sync.");
  assert_eq!(slot.native().mock_operations(), Vec::<String>::new());
  assert_eq!(wm.state.dispatcher.mock_operations(), Vec::<String>::new());
  assert!(!wm.state.pending_sync.has_changes());
  set_focused_descendant(&sibling.clone().into(), None);
  wm.state.pending_sync.queue_all_effects_update();
  platform_sync(&mut wm.state, &config)
    .expect("Unfocused reserved effects.");
  assert_eq!(slot.native().mock_operations(), Vec::<String>::new());
  assert_eq!(wm.state.windows().len(), 2);
  clear(workspace);
}

/// Exercises the same native binding returning after cleanup reserved it.
/// Other owner activity cannot replace that recovered binding.
fn assert_recovered_original_retains_slot(
  activity: bool,
  candidate_groups: &[Option<u32>],
) {
  let (mut wm, mut config, workspace, slot, sibling) = fixture();
  detach_container(workspace.clone().into()).expect("Reparent workspace.");
  let monitor = Monitor::mock().workspaces(vec![workspace.clone()]).call();
  attach_container(
    &monitor.clone().into(),
    &wm.state.root_container.clone().into(),
    None,
  )
  .expect("Attach mock monitor for recovery redraw.");
  config.value.general.cursor_jump.enabled = false;
  let original = slot.native().clone();
  let id = slot.id();
  let parent = slot.parent();
  let order = workspace.children();
  let focus_order = workspace.descendant_focus_order().collect::<Vec<_>>();
  let rect = slot.to_rect().expect("Original allocation.");
  let sibling_rect = sibling.to_rect().expect("Sibling allocation.");
  original.mock_set_valid(false);
  wm.state
    .cleanup_invalid_windows()
    .expect("Reserve inactive original.");
  assert!(wm.state.native_binding_reserved(&slot));
  let started = wm.state.pending_native_tabs[0].started;
  probe_new(&mut wm.state, &mut config, &[])
    .expect("Immediate probe has no successor.");

  let candidates = candidate_groups
    .iter()
    .enumerate()
    .map(|(index, group)| {
      NativeWindow::mock_tab(WindowId(30 + index as u32), 7, *group)
    })
    .collect::<Vec<_>>();
  original.mock_set_owner_windows(candidates.clone());
  for candidate in &candidates {
    assert!(!original.has_visited_tab(candidate));
    wm.process_event(
      wm_platform::PlatformEvent::Window(WindowEvent::Shown {
        window: candidate.clone(),
        notification: WindowEventNotification(None),
      }),
      &mut config,
    )
    .expect("Hold transient owner activity.");
  }
  assert_eq!(wm.state.deferred_native_tab_windows.len(), candidates.len());

  // No replacement ID or destruction event: the original itself returns.
  original.mock_set_valid(true);
  original.mock_set_queried_tab_group(None);
  if activity {
    // Do not mistake legitimate ordinary replay for stealing the slot.
    // Without activity, candidates stay valid through the deadline probe.
    for candidate in &candidates {
      candidate.mock_set_valid(false);
    }
    wm.process_event(
      wm_platform::PlatformEvent::Window(WindowEvent::Shown {
        window: original.clone(),
        notification: WindowEventNotification(None),
      }),
      &mut config,
    )
    .expect("Original becomes shown again.");
  }
  let released_on_activity = !wm.state.native_binding_reserved(&slot);
  reconcile_due(
    &mut wm.state,
    &mut config,
    started + Duration::from_millis(500),
  )
  .expect("Resolve original recovery at deadline.");

  // Transient candidates disappear before ordinary replay. Without return
  // activity, they were still valid at the deadline reconciliation.
  for candidate in &candidates {
    candidate.mock_set_valid(false);
  }
  replay_ready(&mut wm.state, &mut config);
  let retained =
    wm.state.window_from_native(&original) == Some(slot.clone());
  let actual_id = slot.id();
  let actual_native_id = slot.native().id();
  let actual_parent = slot.parent();
  let actual_order = workspace.children();
  let actual_focus_order =
    workspace.descendant_focus_order().collect::<Vec<_>>();
  let actual_rect = slot.to_rect().expect("Recovered allocation.");
  let actual_sibling_rect =
    sibling.to_rect().expect("Sibling allocation.");
  let count = wm.state.windows().len();
  let reserved = wm.state.native_binding_reserved(&slot);
  let settled = deadline(&wm.state).is_none();
  let focused = slot.has_focus(None);
  let focus_requested = wm.state.pending_sync.needs_focus_update();
  let effects_requested =
    wm.state.pending_sync.needs_focused_effect_update()
      || wm.state.prev_effects_window.as_ref() == Some(&slot);
  platform_sync(&mut wm.state, &config).expect("Resume recovery redraw.");
  let operations = original.mock_operations();
  let unmanaged = std::iter::from_fn(|| wm.event_rx.try_recv().ok())
    .filter(|event| matches!(event, WmEvent::WindowUnmanaged { .. }))
    .count();
  let groupless = !original.has_same_tab_group(&original);

  // A later real final close must not inherit the old group reservation.
  original.mock_set_valid(false);
  original.mock_set_owner_windows(vec![]);
  super::destroyed(original.id(), &mut wm.state, &mut config)
    .expect("Close recovered original.");
  let final_count = wm.state.windows().len();
  let final_settled = deadline(&wm.state).is_none();
  let final_unmanaged = std::iter::from_fn(|| wm.event_rx.try_recv().ok())
    .filter(|event| matches!(event, WmEvent::WindowUnmanaged { .. }))
    .count();
  clear(workspace);
  detach_container(monitor.into()).expect("Detach mock monitor.");

  assert!(retained, "Recovered original lost its existing slot.");
  assert_eq!(actual_id, id);
  assert_eq!(actual_native_id, original.id());
  assert_eq!(actual_parent, parent);
  assert_eq!(actual_order, order);
  assert_eq!(actual_focus_order, focus_order);
  assert_eq!(actual_rect, rect);
  assert_eq!(actual_sibling_rect, sibling_rect);
  assert_eq!(count, 2);
  assert!(
    !reserved && settled,
    "Recovery left a reservation or deferred work."
  );
  assert!(
    !activity || released_on_activity,
    "Shown original stayed reserved."
  );
  assert!(focused && !focus_requested, "Recovery shifted focus.");
  assert_eq!(unmanaged, 0, "Recovery published an ordinary close.");
  assert!(
    effects_requested,
    "Recovery did not resume focused effects."
  );
  assert!(operations.iter().any(|operation| operation == "set_frame"));
  assert!(!operations.iter().any(|operation| operation == "focus"));
  assert!(groupless, "Recovery retained stale native group identity.");
  assert_eq!(final_count, 1);
  assert!(final_settled);
  assert_eq!(
    final_unmanaged, 1,
    "Final close must unmanage exactly once."
  );
}

/// Shown activity releases the recovered original before its old deadline.
#[test]
fn recovered_original_activity_retains_slot() {
  assert_recovered_original_retains_slot(true, &[]);
}

/// Recovery must not depend on delivery of another native notification.
#[test]
fn recovered_original_without_activity_retains_slot() {
  assert_recovered_original_retains_slot(false, &[]);
}

/// An unvisited groupless candidate cannot steal the returning original.
#[test]
fn recovered_original_rejects_unvisited_transient_candidate() {
  for activity in [false, true] {
    assert_recovered_original_retains_slot(activity, &[None]);
  }
}

/// Ambiguous same-group candidates cannot displace a recovered original.
#[test]
fn recovered_original_rejects_ambiguous_transient_candidates() {
  for activity in [false, true] {
    assert_recovered_original_retains_slot(activity, &[Some(9), Some(9)]);
  }
}

/// Recovery preserves newer focus unless the native focus still confirms
/// the recovered window. Focus rules record any forwarded notification.
fn assert_recovered_focus_uses_current_native_focus(
  current_focus: WindowId,
) {
  let (mut wm, _, workspace, slot, sibling) = fixture();
  detach_container(workspace.clone().into()).expect("Reparent workspace.");
  let monitor = Monitor::mock().workspaces(vec![workspace.clone()]).call();
  attach_container(
    &monitor.clone().into(),
    &wm.state.root_container.clone().into(),
    None,
  )
  .expect("Attach mock monitor for recovery sync.");
  let mut config = UserConfig::for_testing(vec![WindowRuleConfig {
    commands: vec![InvokeCommand::WmDisableBindingMode {
      name: "focus-rule-marker".into(),
    }],
    match_window: vec![WindowMatchConfig::default()],
    on: vec![WindowRuleEvent::Focus],
    run_once: false,
  }]);
  config.value.general.cursor_jump.enabled = false;
  let original = slot.native().clone();
  original.mock_set_valid(false);
  wm.state
    .cleanup_invalid_windows()
    .expect("Reserve original.");
  assert!(wm.state.native_binding_reserved(&slot));

  // A newer WM selection has already superseded the queued notification.
  set_focused_descendant(&sibling.clone().into(), None);
  let native_focus = if current_focus == original.id() {
    original.clone()
  } else if current_focus == sibling.native().id() {
    sibling.native().clone()
  } else {
    // An unmanaged external app can own native focus without replacing
    // the last selected WM container.
    NativeWindow::mock_tab(current_focus, 99, None)
  };
  wm.state
    .dispatcher
    .mock_set_focused_window(Some(native_focus));
  wm.state.pending_sync.clear();
  while wm.event_rx.try_recv().is_ok() {}
  let order = workspace.children();
  let focus_order = workspace.descendant_focus_order().collect::<Vec<_>>();
  let id = slot.id();
  original.mock_set_valid(true);
  original.mock_set_queried_tab_group(None);
  wm.process_event(
    wm_platform::PlatformEvent::Window(WindowEvent::Focused {
      window: original.clone(),
      notification: WindowEventNotification(None),
    }),
    &mut config,
  )
  .expect("Deliver recovered window's delayed focus notification.");

  let retained =
    wm.state.window_from_native(&original) == Some(slot.clone());
  let reserved = wm.state.native_binding_reserved(&slot);
  let settled = deadline(&wm.state).is_none();
  let sibling_focused = sibling.has_focus(None);
  let original_focused = slot.has_focus(None);
  let actual_order = workspace.children();
  let actual_focus_order =
    workspace.descendant_focus_order().collect::<Vec<_>>();
  let mut focus_events = Vec::new();
  let mut focus_rule_runs = 0;
  let mut unmanaged = 0;
  while let Ok(event) = wm.event_rx.try_recv() {
    match event {
      WmEvent::FocusChanged {
        focused_container: ContainerDto::Window(dto),
      } => {
        focus_events.push(dto.id);
      }
      WmEvent::BindingModesChanged { .. } => focus_rule_runs += 1,
      WmEvent::WindowUnmanaged { .. } => unmanaged += 1,
      _ => {}
    }
  }
  let operations = original.mock_operations();
  clear(workspace);
  detach_container(monitor.into()).expect("Detach mock monitor.");

  assert!(
    retained && !reserved && settled,
    "Original did not recover."
  );
  assert_eq!(actual_order, order);
  assert_eq!(unmanaged, 0);
  if current_focus == original.id() {
    assert!(original_focused && !sibling_focused);
    assert_eq!(focus_events, vec![id]);
    assert_eq!(focus_rule_runs, 1);
  } else {
    assert!(
      sibling_focused && !original_focused,
      "Stale recovery stole WM focus."
    );
    assert_eq!(actual_focus_order, focus_order);
    assert!(
      focus_events.is_empty(),
      "Stale notification published focus."
    );
    assert_eq!(focus_rule_runs, 0, "Stale notification ran Focus rules.");
    assert!(!operations.iter().any(|operation| operation == "focus"));
  }
}

/// A newer managed sibling's native focus overrides a queued notification.
#[test]
fn recovered_focused_notification_preserves_newer_sibling_focus() {
  assert_recovered_focus_uses_current_native_focus(WindowId(2));
}

/// An external application's current native focus is not recovery
/// authority.
#[test]
fn recovered_focused_notification_preserves_external_native_focus() {
  assert_recovered_focus_uses_current_native_focus(WindowId(77));
}

/// A current native focus notification must still reach ordinary Focus
/// rules.
#[test]
fn recovered_focused_notification_accepts_current_native_focus() {
  assert_recovered_focus_uses_current_native_focus(WindowId(1));
}

/// One inconclusive deadline performs ordinary close once, even paused.
#[test]
fn unresolved_deadline_closes_once_and_cleanup_can_reserve_first() {
  for paused in [false, true] {
    let (mut wm, mut config, workspace, slot, sibling) = fixture();
    wm.state.is_paused = paused;
    slot.native().mock_set_valid(false);
    wm.state
      .cleanup_invalid_windows_with(
        |window| window.native().id() != WindowId(1),
        |_| true,
      )
      .expect("Cleanup first.");
    assert_eq!(wm.state.pending_native_tabs.len(), 1);
    let started = wm.state.pending_native_tabs[0].started;
    destroy(&mut wm, &mut config);
    assert_eq!(wm.state.pending_native_tabs[0].started, started);
    reconcile_due(
      &mut wm.state,
      &mut config,
      started + Duration::from_millis(499),
    )
    .expect("Before deadline.");
    assert_eq!(wm.state.windows().len(), 2);
    set_focused_descendant(&sibling.clone().into(), None);
    reconcile_due(
      &mut wm.state,
      &mut config,
      started + Duration::from_millis(500),
    )
    .expect("Close.");
    reconcile_due(
      &mut wm.state,
      &mut config,
      Instant::now() + Duration::from_secs(10),
    )
    .expect("No second close.");
    assert!(sibling.has_focus(None));
    assert!(!wm.state.pending_sync.needs_focus_update());
    assert_eq!(wm.state.windows().len(), 1);
    assert!(wm.state.pending_native_tabs.is_empty());
    assert_eq!(
      std::iter::from_fn(|| wm.event_rx.try_recv().ok())
        .filter(|event| matches!(event, WmEvent::WindowUnmanaged { .. }))
        .count(),
      usize::from(!paused)
    );
    clear(workspace);
  }
}

/// Activity arriving before the deadline must not create a second
/// container.
#[test]
fn successor_activity_waits_for_reserved_transaction() {
  for group in [Some(9), None] {
    let (mut wm, mut config, workspace, slot, _) = fixture();
    wm.state.is_paused = true;
    let old = slot.native().clone();
    old.mock_set_valid(false);
    destroy(&mut wm, &mut config);
    let started = wm.state.pending_native_tabs[0].started;
    let candidate = NativeWindow::mock_tab(WindowId(3), 7, group);
    old.mock_set_owner_windows(vec![candidate.clone()]);
    wm.state
      .dispatcher
      .mock_set_focused_window(Some(candidate.clone()));
    for _ in 0..2 {
      wm.process_event(
        wm_platform::PlatformEvent::Window(WindowEvent::Shown {
          window: candidate.clone(),
          notification: WindowEventNotification(None),
        }),
        &mut config,
      )
      .expect("Successor activity.");
    }
    assert_eq!(wm.state.windows().len(), 2);
    assert_eq!(slot.native().id(), WindowId(1));
    assert!(!wm.state.pending_sync.needs_focus_update());
    assert_eq!(wm.state.deferred_native_tab_windows.len(), 1);
    assert_eq!(
      deadline(&wm.state),
      Some(started + Duration::from_millis(500))
    );
    candidate.mock_set_tab_group(Some(9));
    // Age the real record rather than sleeping or dispatching an A event.
    wm.state.pending_native_tabs[0].started =
      Instant::now() - Duration::from_millis(500);
    wm.reconcile_native_tabs(&mut config)
      .expect("Paused deadline and replay.");
    assert_eq!(slot.native().id(), candidate.id());
    assert_eq!(wm.state.windows().len(), 2);
    assert!(wm.state.deferred_native_tab_windows.is_empty());
    assert!(!wm.state.pending_sync.needs_focus_update());
    clear(workspace);
  }
}

/// Deferred owner identities survive cancellation and failed queries
/// exactly once. This tests replay selection, not monitor-dependent
/// new-window creation.
#[test]
fn unrelated_same_owner_activity_drains_once_after_resolution_or_cancellation(
) {
  for cancellation in ["expiry", "ignore", "detach"] {
    let (mut wm, mut config, workspace, slot, sibling) = fixture();
    wm.state.is_paused = true;
    let old = slot.native().clone();
    old.mock_set_valid(false);
    destroy(&mut wm, &mut config);
    old.mock_fail_owner_enumeration();
    let candidate = NativeWindow::mock_tab(WindowId(3), 7, Some(10));
    for _ in 0..2 {
      wm.process_event(
        wm_platform::PlatformEvent::Window(WindowEvent::Shown {
          window: candidate.clone(),
          notification: WindowEventNotification(None),
        }),
        &mut config,
      )
      .expect("Unrelated owner activity.");
    }
    assert_eq!(wm.state.deferred_native_tab_windows.len(), 1);
    set_focused_descendant(&sibling.clone().into(), None);
    match cancellation {
      "ignore" => {
        crate::commands::window::ignore_window(slot.clone(), &mut wm.state)
          .expect("Ignore.")
      }
      "detach" => detach_container(slot.clone().into()).expect("Detach."),
      _ => {
        let started = wm.state.pending_native_tabs[0].started;
        reconcile_due(
          &mut wm.state,
          &mut config,
          started + Duration::from_millis(500),
        )
        .expect("Query failure expiry.");
      }
    }
    if cancellation == "detach" {
      assert!(deadline(&wm.state)
        .is_some_and(|deadline| deadline <= Instant::now()));
      reconcile_due(&mut wm.state, &mut config, Instant::now())
        .expect("Prune detached record.");
    }
    assert!(wm.state.pending_native_tabs.is_empty());
    assert!(deadline(&wm.state).is_some());
    assert_eq!(take_ready(&mut wm.state), vec![candidate]);
    assert!(take_ready(&mut wm.state).is_empty());
    assert!(deadline(&wm.state).is_none());
    assert!(sibling.has_focus(None));
    assert!(!wm.state.pending_sync.needs_focus_update());
    clear(workspace);
  }
}

/// Replay drops identities that were closed, ignored, invalidated, or
/// already admitted.
#[test]
fn deferred_replay_discards_ineligible_or_already_managed_identities() {
  for disposition in ["closed", "ignored", "invalid", "managed"] {
    let (mut wm, mut config, workspace, slot, _) = fixture();
    wm.state.is_paused = true;
    slot.native().mock_set_valid(false);
    destroy(&mut wm, &mut config);
    let candidate = NativeWindow::mock_tab(WindowId(3), 7, Some(10));
    wm.process_event(
      wm_platform::PlatformEvent::Window(WindowEvent::Shown {
        window: candidate.clone(),
        notification: WindowEventNotification(None),
      }),
      &mut config,
    )
    .expect("Deferred activity.");
    match disposition {
      "closed" => wm
        .process_event(
          wm_platform::PlatformEvent::Window(WindowEvent::Destroyed {
            window_id: candidate.id(),
            notification: WindowEventNotification(None),
          }),
          &mut config,
        )
        .expect("Closed candidate."),
      "ignored" => wm.state.ignored_windows.push(candidate.clone()),
      "invalid" => candidate.mock_set_valid(false),
      _ => {
        let represented: WindowContainer =
          NonTilingWindow::mock().native(candidate).call().into();
        attach_container(
          &represented.into(),
          &workspace.clone().into(),
          None,
        )
        .expect("Already represented.");
      }
    }
    let started = wm.state.pending_native_tabs[0].started;
    reconcile_due(
      &mut wm.state,
      &mut config,
      started + Duration::from_millis(500),
    )
    .expect("Expiry.");
    assert!(take_ready(&mut wm.state).is_empty());
    assert!(deadline(&wm.state).is_none());
    clear(workspace);
  }
}

/// A slot without group identity follows ordinary close without
/// reservation.
#[test]
fn missing_group_closes_immediately_and_ignore_cancels_reservation() {
  for has_group in [false, true] {
    let (mut wm, mut config, workspace, slot, _) = fixture();
    if !has_group {
      slot.replace_native(
        NativeWindow::mock_tab(WindowId(1), 7, None),
        slot.native_properties(),
      );
    }
    slot.native().mock_set_valid(false);
    destroy(&mut wm, &mut config);
    if has_group {
      assert_eq!(wm.state.pending_native_tabs.len(), 1);
      crate::commands::window::ignore_window(slot, &mut wm.state)
        .expect("Ignore reserved slot.");
    }
    assert!(wm.state.pending_native_tabs.is_empty());
    assert_eq!(wm.state.windows().len(), 1);
    clear(workspace);
  }
}

/// Ignore, different-owner/group, and ambiguous candidates are not
/// replacements.
#[test]
fn destruction_does_not_admit_unproven_candidates_as_new_windows() {
  for case in 0..8 {
    let (mut wm, mut config, workspace, slot, _) = fixture();
    let old = slot.native().clone();
    old.mock_set_valid(false);
    let candidate = NativeWindow::mock_tab(
      WindowId(3),
      if case == 0 { 8 } else { 7 },
      if case == 7 {
        None
      } else {
        Some(if case == 1 { 10 } else { 9 })
      },
    );
    if case == 2 {
      wm.state.ignored_windows.push(candidate.clone());
    }
    if case == 3 {
      config = UserConfig::for_testing(vec![WindowRuleConfig {
        commands: vec![InvokeCommand::Ignore],
        match_window: vec![WindowMatchConfig::default()],
        on: vec![WindowRuleEvent::Manage],
        run_once: true,
      }]);
    }
    if case == 5 {
      old.mock_fail_owner_enumeration();
    }
    if case == 6 {
      candidate.mock_set_valid(false);
    }
    let mut inventory = vec![candidate];
    if case == 4 {
      inventory.push(NativeWindow::mock_tab(WindowId(4), 7, Some(9)));
    }
    old.mock_set_owner_windows(inventory);
    destroy(&mut wm, &mut config);
    assert_eq!(slot.native().id(), WindowId(1));
    let started = wm.state.pending_native_tabs[0].started;
    reconcile_due(
      &mut wm.state,
      &mut config,
      started + Duration::from_millis(500),
    )
    .expect("Final close.");
    assert_eq!(wm.state.windows().len(), 1);
    clear(workspace);
  }
}
