//! Bounded recovery of a destroyed native tab's existing layout slot.

use std::time::{Duration, Instant};

use uuid::Uuid;
use wm_common::{WindowRuleEvent, WmEvent};
use wm_platform::{NativeWindow, NativeWindowExtMacOs, WindowId};

use crate::{
  commands::window::{
    check_is_manageable, manage_window_preserving_focus,
    native_tab_allowed, run_window_rules,
  },
  events::handle_window_destroyed,
  models::{NativeWindowProperties, WindowContainer},
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

// ponytail: one delayed probe, not a universal native settlement bound.
// A survivor still unqueryable then follows ordinary close.
const RECONCILE_DELAY: Duration = Duration::from_millis(500);

/// Reserves one managed instance, retaining its original native group.
pub(crate) struct PendingNativeTab {
  pub(crate) id: Uuid,
  original: NativeWindow,
  pub(crate) started: Instant,
}

/// Prevents cleanup from dropping a group-backed slot before destruction.
pub(crate) fn reserve(
  window: &WindowContainer,
  state: &mut WmState,
) -> bool {
  if state
    .pending_native_tabs
    .iter()
    .any(|pending| pending.id == window.id())
  {
    return true;
  }
  let native = window.native().clone();
  if !native.has_same_tab_group(&native) {
    return false;
  }
  state.pending_native_tabs.push(PendingNativeTab {
    id: window.id(),
    original: native,
    started: Instant::now(),
  });
  true
}

/// Whether native operations must skip this reserved dead binding.
pub(crate) fn is_reserved(
  window: &WindowContainer,
  state: &WmState,
) -> bool {
  state.pending_native_tabs.iter().any(|pending| {
    pending.id == window.id()
      && pending.original.id() == window.native().id()
  })
}

/// Finds the one eligible same-owner/group successor, never normal
/// admission.
fn successor(
  original: &NativeWindow,
  state: &WmState,
  config: &UserConfig,
) -> Option<(NativeWindow, NativeWindowProperties)> {
  let mut candidates = original.tab_group_candidates().ok()?;
  candidates.sort_by_key(NativeWindow::id);
  candidates.dedup_by_key(|candidate| candidate.id());
  let mut matches = candidates.into_iter().filter_map(|candidate| {
    if candidate.id() == original.id()
      || !candidate.is_valid()
      || state.window_from_native(&candidate).is_some()
      || state.ignored_windows.contains(&candidate)
    {
      return None;
    }
    candidate.refresh_tab_group();
    if !candidate.has_same_tab_group(original) {
      return None;
    }
    let properties = check_is_manageable(&candidate).ok()??;
    native_tab_allowed(&candidate, &properties, config)
      .then_some((candidate, properties))
  });
  let candidate = matches.next()?;
  if matches.next().is_some() {
    return None;
  }
  Some(candidate)
}

/// Replaces the native binding without changing focus order or layout.
fn replace(
  window: WindowContainer,
  original: &NativeWindow,
  state: &mut WmState,
  config: &mut UserConfig,
) -> anyhow::Result<bool> {
  let Some((native, properties)) = successor(original, state, config)
  else {
    return Ok(false);
  };
  window.replace_native(native, properties);
  // Remove the reservation before Manage rules can detach or transform U.
  state
    .pending_native_tabs
    .retain(|pending| pending.id != window.id());
  if let Some(window) =
    run_window_rules(window, &WindowRuleEvent::Manage, state, config)?
  {
    state.emit_event(WmEvent::WindowManaged {
      managed_window: window.to_dto()?,
    });
    state.pending_sync.queue_container_to_redraw(window.clone());
    // Replacement is not permission to activate another application. The
    // existing WM focus order is preserved even when global focus is
    // stale.
    if window.has_focus(None)
      && state
        .dispatcher
        .focused_window()
        .is_ok_and(|focused| focused == *window.native())
    {
      state.is_focus_synced = true;
      state.pending_sync.queue_focused_effect_update();
    }
  }
  Ok(true)
}

/// Probes before ordinary destruction can detach or flatten the slot.
pub(crate) fn destroyed(
  native_id: WindowId,
  state: &mut WmState,
  config: &mut UserConfig,
) -> anyhow::Result<()> {
  state
    .deferred_native_tab_windows
    .retain(|native| native.id() != native_id);
  let window = state
    .windows()
    .into_iter()
    .find(|window| window.native().id() == native_id);
  if let Some(window) = window {
    // Duplicate destruction must neither restart the deadline nor
    // re-probe.
    if is_reserved(&window, state) {
      return Ok(());
    }
    if reserve(&window, state) {
      let original = window.native().clone();
      replace(window, &original, state, config)?;
      return Ok(());
    }
  }
  handle_window_destroyed(native_id, state)
}

/// Returns the earliest one-shot deadline; no polling task is created.
pub(crate) fn deadline(state: &WmState) -> Option<Instant> {
  // Cancellation can leave deferred activity without any reservation. Wake
  // once to drain it, including when Ignore happened through an IPC
  // command.
  let detached = state.pending_native_tabs.iter().any(|pending| {
    !state.container_by_id(pending.id).is_some_and(|container| {
      container
        .as_window_container()
        .is_ok_and(|window| window.native().id() == pending.original.id())
    })
  });
  if detached
    || state
      .deferred_native_tab_windows
      .iter()
      .any(|native| !owner_reserved(native, state))
  {
    return Some(Instant::now());
  }
  state
    .pending_native_tabs
    .iter()
    .map(|pending| pending.started + RECONCILE_DELAY)
    .min()
}

/// Holds unowned same-owner activity until replacement-or-close resolves
/// atomically.
pub(crate) fn blocks_admission(
  native: &NativeWindow,
  state: &mut WmState,
) -> bool {
  if state.window_from_native(native).is_some()
    || state.ignored_windows.contains(native)
    || !owner_reserved(native, state)
  {
    return false;
  }
  // Group lookup can still be stale here. Hold owner activity, not merely
  // positively matching groups, without recording an unbounded event log.
  state
    .deferred_native_tab_windows
    .retain(|window| window.is_alive());
  if !state.deferred_native_tab_windows.contains(native)
    && native.is_alive()
  {
    state.deferred_native_tab_windows.push(native.clone());
  }
  true
}

/// Checks the cached process identity without any native focus/group
/// query.
fn owner_reserved(native: &NativeWindow, state: &WmState) -> bool {
  state
    .pending_native_tabs
    .iter()
    .any(|pending| native.has_same_owner(&pending.original))
}

/// Drains ready identities once; unresolved owners keep their original
/// deadline.
fn take_ready(state: &mut WmState) -> Vec<NativeWindow> {
  let mut ready = Vec::new();
  for native in std::mem::take(&mut state.deferred_native_tab_windows) {
    if owner_reserved(&native, state) {
      state.deferred_native_tab_windows.push(native);
    } else if native.is_valid()
      && !state.ignored_windows.contains(&native)
      && state.window_from_native(&native).is_none()
    {
      ready.push(native);
    }
  }
  ready
}

/// Replays actual ordinary eligibility, not stale event focus authority.
pub(crate) fn replay_ready(state: &mut WmState, config: &mut UserConfig) {
  for native in take_ready(state) {
    if let Err(err) = manage_window_preserving_focus(native, state, config)
    {
      // One failed admission must not strand the remaining finite queue.
      tracing::warn!("Failed deferred native window admission: {err:?}");
    }
  }
}

/// Gives newly cleanup-reserved slots the same immediate probe as
/// destruction.
pub(crate) fn probe_new(
  state: &mut WmState,
  config: &mut UserConfig,
  previous: &[Uuid],
) -> anyhow::Result<()> {
  let records = state
    .pending_native_tabs
    .iter()
    .filter(|pending| !previous.contains(&pending.id))
    .map(|pending| (pending.id, pending.original.clone()))
    .collect::<Vec<_>>();
  for (id, original) in records {
    if let Some(window) = state
      .container_by_id(id)
      .and_then(|container| container.as_window_container().ok())
    {
      replace(window, &original, state, config)?;
    }
  }
  Ok(())
}

/// Resolves expired reservations even while paused, using monotonic age.
pub(crate) fn reconcile_due(
  state: &mut WmState,
  config: &mut UserConfig,
  now: Instant,
) -> anyhow::Result<()> {
  let records = state
    .pending_native_tabs
    .iter()
    .map(|pending| (pending.id, pending.original.clone(), pending.started))
    .collect::<Vec<_>>();
  for (id, original, started) in records {
    let Some(window) = state
      .container_by_id(id)
      .and_then(|container| container.as_window_container().ok())
    else {
      state.pending_native_tabs.retain(|pending| pending.id != id);
      continue;
    };
    if window.native().id() != original.id() {
      state.pending_native_tabs.retain(|pending| pending.id != id);
      continue;
    }
    let expired =
      now.saturating_duration_since(started) >= RECONCILE_DELAY;
    if !expired {
      continue;
    }
    if replace(window, &original, state, config)? {
      continue;
    }
    state.pending_native_tabs.retain(|pending| pending.id != id);
    tracing::info!(
      "Native tab reconciliation exhausted one 500ms attempt: {:?}.",
      original.id()
    );
    handle_window_destroyed(original.id(), state)?;
  }
  Ok(())
}

#[cfg(test)]
#[path = "native_tabs_tests.rs"]
mod tests;
