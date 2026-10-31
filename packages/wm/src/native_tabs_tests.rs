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
  models::{NonTilingWindow, Workspace},
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
