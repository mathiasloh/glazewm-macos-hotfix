use anyhow::Context;
use tracing::info;
use wm_common::{try_warn, WindowRuleEvent, WindowState, WmEvent};
use wm_platform::{NativeWindow, RectDelta};

use crate::{
  commands::{
    container::{attach_container, set_focused_descendant},
    window::run_window_rules,
  },
  models::{
    Container, Monitor, NativeWindowProperties, NonTilingWindow,
    TilingWindow, WindowContainer,
  },
  traits::{CommonGetters, PositionGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

/// Admits a native window using the normal new-window focus policy.
pub fn manage_window(
  native_window: NativeWindow,
  target_parent: Option<Container>,
  state: &mut WmState,
  config: &mut UserConfig,
) -> anyhow::Result<()> {
  manage_window_with_focus(
    native_window,
    target_parent,
    state,
    config,
    true,
  )
}

/// Replays admission without treating an old notification as focus
/// authority.
#[cfg(target_os = "macos")]
pub(crate) fn manage_window_preserving_focus(
  native_window: NativeWindow,
  state: &mut WmState,
  config: &mut UserConfig,
) -> anyhow::Result<()> {
  let take_focus = state
    .dispatcher
    .focused_window()
    .is_ok_and(|focused| focused == native_window);
  manage_window_with_focus(native_window, None, state, config, take_focus)
}

/// Shares eligibility, creation, and Manage rules between live and
/// deferred admission.
fn manage_window_with_focus(
  native_window: NativeWindow,
  target_parent: Option<Container>,
  state: &mut WmState,
  config: &mut UserConfig,
  take_focus: bool,
) -> anyhow::Result<()> {
  let Some(native_properties) =
    check_is_manageable(&native_window).unwrap_or(None)
  else {
    return Ok(());
  };

  #[cfg(target_os = "macos")]
  let replacement = {
    use wm_platform::NativeWindowExtMacOs;

    native_window.refresh_tab_group();
    let is_active_valid = native_window.is_valid()
      && state
        .dispatcher
        .focused_window()
        .is_ok_and(|focused| focused == native_window);
    replace_native_tab(
      &native_window,
      &native_properties,
      state,
      config,
      is_active_valid,
      NativeWindow::is_valid,
      |old| native_window.has_same_tab_group(old),
    )
  };
  #[cfg(not(target_os = "macos"))]
  let replacement: Option<WindowContainer> = None;

  // Create only when no unique inactive tab can retain this layout slot.
  let window = match replacement {
    Some(window) => window,
    None => try_warn!(create_window(
      native_window,
      native_properties,
      target_parent,
      state,
      config
    )),
  };

  if take_focus {
    complete_window_management(window, state, config)
  } else {
    complete_window_management_with_focus(window, state, config, false)
  }
}

/// Applies admission rules and publishes the final managed window.
fn complete_window_management(
  window: WindowContainer,
  state: &mut WmState,
  config: &mut UserConfig,
) -> anyhow::Result<()> {
  complete_window_management_with_focus(window, state, config, true)
}

/// Publishes admission, retaining newer focus when replaying inactive
/// windows.
fn complete_window_management_with_focus(
  window: WindowContainer,
  state: &mut WmState,
  config: &mut UserConfig,
  take_focus: bool,
) -> anyhow::Result<()> {
  // Live admission runs rules as focused; deferred inactive admission does
  // not.
  if take_focus {
    set_focused_descendant(&window.clone().into(), None);
  }

  // Window might be detached if `ignore` command has been invoked.
  let updated_window = run_window_rules(
    window.clone(),
    &WindowRuleEvent::Manage,
    state,
    config,
  )?;

  if let Some(window) = updated_window {
    info!("New window managed: {window}");

    state.emit_event(WmEvent::WindowManaged {
      managed_window: window.to_dto()?,
    });

    // OS focus should be set to the newly added window in case it's not
    // already focused.
    if take_focus {
      state.pending_sync.queue_focus_change();

      // Initial focus notifications can precede admission, so update
      // effects and workspace order here only when focus is still
      // authorized.
      state.pending_sync.queue_focused_effect_update();
      state.pending_sync.queue_workspace_to_reorder(
        window.workspace().context("No workspace.")?,
      );
    }

    // Sibling containers need to be redrawn if the window is tiling.
    state.pending_sync.queue_container_to_redraw(
      if window.state() == WindowState::Tiling {
        window.parent().context("No parent.")?
      } else {
        window.into()
      },
    );
  }

  Ok(())
}

/// Reuses a unique inactive native tab's container, never its position.
///
/// Platform predicates are injected so layout tests need no live windows.
#[cfg(target_os = "macos")]
fn replace_native_tab(
  native: &NativeWindow,
  properties: &NativeWindowProperties,
  state: &WmState,
  config: &UserConfig,
  is_active_valid: bool,
  is_valid: impl Fn(&NativeWindow) -> bool,
  same_group: impl Fn(&NativeWindow) -> bool,
) -> Option<WindowContainer> {
  if !is_active_valid
    || state.ignored_windows.contains(native)
    || state.window_from_native(native).is_some()
  {
    return None;
  }

  let windows = state.windows();
  let mut matches = windows.into_iter().filter(|window| {
    !is_valid(&window.native()) && same_group(&window.native())
  });
  let window = matches.next()?;
  if matches.next().is_some() {
    return None;
  }

  if !native_tab_allowed(native, properties, config) {
    return None;
  }

  window.replace_native(native.clone(), properties.clone());
  Some(window)
}

/// Previews Ignore rules without attaching or changing the retained slot.
#[cfg(target_os = "macos")]
pub(crate) fn native_tab_allowed(
  native: &NativeWindow,
  properties: &NativeWindowProperties,
  config: &UserConfig,
) -> bool {
  let preview: WindowContainer = TilingWindow::new(
    None,
    native.clone(),
    properties.clone(),
    None,
    RectDelta::zero(),
    properties.frame.clone(),
    false,
    Default::default(),
    Vec::new(),
    None,
  )
  .into();
  !config
    .pending_window_rules(&preview, &WindowRuleEvent::Manage)
    .iter()
    .any(|rule| rule.commands.contains(&wm_common::InvokeCommand::Ignore))
}

/// Checks if a window is manageable and retrieves its native properties.
///
/// Returns `Ok(Some(properties))` if the window is manageable and its
/// properties were retrieved successfully.
pub(crate) fn check_is_manageable(
  native_window: &NativeWindow,
) -> anyhow::Result<Option<NativeWindowProperties>> {
  if !native_window.is_visible()? {
    tracing::debug!(
      "Window {} not manageable: not visible.",
      native_window.id().0,
    );
    return Ok(None);
  }

  #[cfg(target_os = "macos")]
  {
    use wm_platform::NativeWindowExtMacOs;

    let role = native_window.role()?;
    let subrole = native_window.subrole()?;
    let is_crossover_wine_helper =
      native_window.is_crossover_wine_helper();

    if !is_manageable_macos_window(
      &role,
      &subrole,
      is_crossover_wine_helper,
    ) {
      tracing::debug!(
        "Window {} not manageable: role={}, subrole={}.",
        native_window.id().0,
        role,
        subrole,
      );
      return Ok(None);
    }
  }

  // Ensure window has a valid process name, title, etc.
  let native_properties = NativeWindowProperties::try_from(native_window)?;

  #[cfg(target_os = "windows")]
  {
    use wm_platform::{
      NativeWindowWindowsExt, WS_CAPTION, WS_CHILD, WS_EX_NOACTIVATE,
      WS_EX_TOOLWINDOW,
    };

    // TODO: Temporary fix for managing Flow Launcher until a force manage
    // command is added.
    let is_flow_launcher = native_properties.process_name
      == "Flow.Launcher"
      && native_properties.title == "Flow.Launcher";

    if !is_flow_launcher {
      // Ensure window is top-level (i.e. not a child window). Ignore
      // windows that cannot be focused or if they're unavailable in
      // task switcher (alt+tab menu).
      if native_window.has_window_style(WS_CHILD)
        || native_window
          .has_window_style_ex(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW)
      {
        return Ok(None);
      }

      // Some applications spawn top-level windows for menus that
      // should be ignored. This includes the autocomplete popup in
      // Notepad++ and title bar menu in Keepass. Although not
      // foolproof, these can typically be identified by having an
      // owner window and no title bar.
      if native_window.has_owner_window()
        && !native_window.has_window_style(WS_CAPTION)
      {
        return Ok(None);
      }
    }
  }

  Ok(Some(native_properties))
}

#[cfg(target_os = "macos")]
fn is_manageable_macos_window(
  role: &str,
  subrole: &str,
  is_crossover_wine_helper: bool,
) -> bool {
  role == "AXWindow"
    && (subrole == "AXStandardWindow"
      || (subrole == "AXUnknown" && is_crossover_wine_helper))
}

fn create_window(
  native_window: NativeWindow,
  native_properties: NativeWindowProperties,
  target_parent: Option<Container>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<WindowContainer> {
  let nearest_monitor = state
    .nearest_monitor(&native_window)
    .context("No nearest monitor.")?;

  let nearest_workspace = nearest_monitor
    .displayed_workspace()
    .context("No nearest workspace.")?;

  let gaps_config = config.value.gaps.for_monitor(
    nearest_monitor.index(),
    &nearest_monitor.native_properties().device_name,
  );
  let window_state =
    window_state_to_create(&native_properties, &nearest_monitor, config)?;

  // Attach the new window as the first child of the target parent (if
  // provided), otherwise, add as a sibling of the focused container.
  let (target_parent, target_index) = match target_parent {
    Some(parent) => (parent, 0),
    None => insertion_target(&window_state, state)?,
  };

  let target_workspace =
    target_parent.workspace().context("No target workspace.")?;

  let prefers_centered = config
    .value
    .window_behavior
    .state_defaults
    .floating
    .centered;

  // Calculate where window should be placed when floating is enabled. Use
  // the original width/height of the window and optionally position it in
  // the center of the workspace.
  let is_same_workspace = nearest_workspace.id() == target_workspace.id();
  let floating_placement = {
    let placement = if !is_same_workspace || prefers_centered {
      native_properties
        .frame
        .translate_to_center(&target_workspace.to_rect()?)
    } else {
      native_properties.frame.clone()
    };

    // Clamp the window size to be within the workspace's outer gaps. 10px
    // is arbitrary - helps differentiate from tiling windows.
    let max_workspace_rect = target_workspace.max_workspace_rect()?;
    placement.clamp_size(
      max_workspace_rect.width() - 10,
      max_workspace_rect.height() - 10,
    )
  };

  // Window has no border delta unless it's later changed via the
  // `adjust_borders` command.
  let border_delta = RectDelta::zero();

  let window_container: WindowContainer = match window_state {
    WindowState::Tiling => TilingWindow::new(
      None,
      native_window,
      native_properties,
      None,
      border_delta,
      floating_placement,
      false,
      gaps_config,
      Vec::new(),
      None,
    )
    .into(),
    _ => NonTilingWindow::new(
      None,
      native_window,
      native_properties,
      window_state,
      None,
      border_delta,
      None,
      floating_placement,
      !prefers_centered,
      Vec::new(),
      None,
    )
    .into(),
  };

  attach_container(
    &window_container.clone().into(),
    &target_parent,
    Some(target_index),
  )?;

  // The OS might spawn the window on a different monitor to the target
  // parent, so adjustments might need to be made because of DPI.
  if nearest_monitor
    .has_dpi_difference(&window_container.clone().into())?
  {
    window_container.set_has_pending_dpi_adjustment(true);
  }

  Ok(window_container)
}

/// Gets the initial state for a window based on its native state.
///
/// Note that maximized windows are initialized as tiling.
fn window_state_to_create(
  native_properties: &NativeWindowProperties,
  nearest_monitor: &Monitor,
  config: &UserConfig,
) -> anyhow::Result<WindowState> {
  if native_properties.is_minimized {
    return Ok(WindowState::Minimized);
  }

  let nearest_workspace = nearest_monitor
    .displayed_workspace()
    .context("No workspace.")?;

  // Only initialize as fullscreen if the window *exceeds* the workspace
  // bounds (due to the 1px inset).
  //
  // For example, with 0px outer gaps and a window that covers the entire
  // workspace, it would still not be initialized as fullscreen. The window
  // needs to be within the workspace's outer gaps by at least 1px on each
  // side.
  if !native_properties.is_maximized
    && native_properties
      .frame
      .inset(1)
      .contains_rect(&nearest_workspace.max_workspace_rect()?)
  {
    return Ok(WindowState::Fullscreen(
      config
        .value
        .window_behavior
        .state_defaults
        .fullscreen
        .clone(),
    ));
  }

  // Initialize windows that can't be resized as floating.
  if !native_properties.is_resizable {
    return Ok(WindowState::Floating(
      config.value.window_behavior.state_defaults.floating.clone(),
    ));
  }

  Ok(WindowState::default_from_config(&config.value))
}

/// Gets where to insert a new window in the container tree.
///
/// Rules:
/// - For non-tiling windows: Always append to the workspace.
/// - For tiling windows:
///   1. Try to insert after the focused tiling window if one exists.
///   2. If a non-tiling window is focused, try to insert after the first
///      tiling window found.
///   3. If no tiling windows exist, append to the workspace.
///
/// Returns tuple of (parent container, insertion index).
fn insertion_target(
  window_state: &WindowState,
  state: &WmState,
) -> anyhow::Result<(Container, usize)> {
  let focused_container =
    state.focused_container().context("No focused container.")?;

  let focused_workspace =
    focused_container.workspace().context("No workspace.")?;

  // For tiling windows, try to find a suitable tiling window to insert
  // next to.
  if *window_state == WindowState::Tiling {
    let sibling = match focused_container {
      Container::TilingWindow(_) => Some(focused_container),
      _ => focused_workspace
        .descendant_focus_order()
        .find(Container::is_tiling_window),
    };

    if let Some(sibling) = sibling {
      return Ok((
        sibling.parent().context("No parent.")?,
        sibling.index() + 1,
      ));
    }
  }

  // Default to appending to workspace.
  Ok((
    focused_workspace.clone().into(),
    focused_workspace.child_count(),
  ))
}

#[cfg(all(test, target_os = "macos"))]
#[path = "manage_window_tab_tests.rs"]
mod native_tab_tests;

#[cfg(all(test, target_os = "macos"))]
mod tests {
  use super::*;

  #[test]
  fn standard_ax_window_is_manageable() {
    assert!(is_manageable_macos_window(
      "AXWindow",
      "AXStandardWindow",
      false,
    ));
  }

  #[test]
  fn unknown_non_crossover_window_is_rejected() {
    assert!(!is_manageable_macos_window("AXWindow", "AXUnknown", false,));
  }

  #[test]
  fn unknown_crossover_window_is_manageable() {
    assert!(is_manageable_macos_window("AXWindow", "AXUnknown", true,));
  }

  #[test]
  fn unknown_non_window_role_is_rejected() {
    assert!(!is_manageable_macos_window("AXButton", "AXUnknown", true,));
  }
}
