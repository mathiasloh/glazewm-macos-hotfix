use anyhow::{bail, Context};
use tokio::sync::mpsc::{self};
use tracing::warn;
use uuid::Uuid;
#[cfg(target_os = "windows")]
use wm_common::TitleBarVisibility;
use wm_common::{
  FloatingStateConfig, FullscreenStateConfig, InvokeCommand, WindowState,
  WmEvent,
};
#[cfg(target_os = "windows")]
use wm_platform::NativeWindowWindowsExt;
use wm_platform::{
  Dispatcher, LengthValue, PlatformEvent, RectDelta, WindowEvent,
};

use crate::{
  commands::{
    container::{
      focus_container_by_id, focus_in_direction, set_tiling_direction,
      toggle_tiling_direction,
    },
    general::{
      cycle_focus, disable_binding_mode, enable_binding_mode,
      platform_sync, reload_config, shell_exec, toggle_pause,
    },
    monitor::focus_monitor,
    window::{
      ignore_window, move_window_in_direction, move_window_to_workspace,
      resize_window, set_window_position, set_window_size,
      update_window_state, WindowPositionTarget,
    },
    workspace::{
      focus_workspace, move_workspace_in_direction,
      update_workspace_config,
    },
  },
  events::{
    handle_display_settings_changed, handle_mouse_move,
    handle_window_destroyed, handle_window_focused, handle_window_hidden,
    handle_window_minimize_ended, handle_window_minimized,
    handle_window_moved_or_resized, handle_window_shown,
    handle_window_title_changed,
  },
  ipc_server::IpcServer,
  models::{Container, WorkspaceTarget},
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

/// Re-admits an unmanaged foreground window on native tab activity.
///
/// Managed and ignored windows bypass admission, avoiding focus changes
/// and geometry feedback from repeated move notifications. Native lookups
/// and admission are injectable so tests need no Accessibility
/// permissions.
#[cfg(target_os = "macos")]
fn reconcile_active_window(
  event: &WindowEvent,
  state: &mut WmState,
  is_active_valid: impl FnOnce(&wm_platform::NativeWindow, &WmState) -> bool,
  show_window: impl FnOnce(
    wm_platform::NativeWindow,
    &mut WmState,
  ) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
  let window = match event {
    WindowEvent::Focused { window, .. }
    | WindowEvent::MovedOrResized { window, .. }
    | WindowEvent::TitleChanged { window, .. } => window,
    _ => return Ok(()),
  };

  if state.window_from_native(window).is_none()
    && !state.ignored_windows.contains(window)
    && is_active_valid(window, state)
  {
    show_window(window.clone(), state)?;
  }

  Ok(())
}

pub struct WindowManager {
  pub event_rx: mpsc::UnboundedReceiver<WmEvent>,
  pub exit_rx: mpsc::UnboundedReceiver<()>,
  pub restart_keybinding_rx: mpsc::UnboundedReceiver<()>,
  pub state: WmState,
}

impl WindowManager {
  pub fn new(
    config: &mut UserConfig,
    dispatcher: Dispatcher,
  ) -> anyhow::Result<Self> {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (exit_tx, exit_rx) = mpsc::unbounded_channel();
    let (restart_keybinding_tx, restart_keybinding_rx) =
      mpsc::unbounded_channel();

    let mut state =
      WmState::new(dispatcher, event_tx, exit_tx, restart_keybinding_tx);
    state.populate(config)?;

    Ok(Self {
      event_rx,
      exit_rx,
      restart_keybinding_rx,
      state,
    })
  }

  pub fn process_event(
    &mut self,
    event: PlatformEvent,
    config: &mut UserConfig,
  ) -> anyhow::Result<()> {
    let state = &mut self.state;

    #[cfg(target_os = "macos")]
    if let PlatformEvent::Window(window_event) = &event {
      // Native tab activation can emit only move/title notifications.
      // Observer tracking outlives invalid-window cleanup, so these need
      // the same admission path as a newly shown window.
      reconcile_active_window(
        window_event,
        state,
        |window, state| {
          state
            .dispatcher
            .focused_window()
            .is_ok_and(|focused| focused == *window)
            && window.is_valid()
        },
        |window, state| handle_window_shown(window, state, config),
      )?;
    }

    match event {
      PlatformEvent::DisplaySettingsChanged => {
        handle_display_settings_changed(state, config)
      }
      PlatformEvent::Keybinding(keybinding_event) => {
        // Find the keybinding config that matches this keybinding.
        let commands = config
          .active_keybinding_configs(
            &self.state.binding_modes,
            self.state.is_paused,
          )
          .find(|kb_config| {
            kb_config.bindings.contains(&keybinding_event.0)
          })
          .map(|kb_config| kb_config.commands.clone());

        if let Some(commands) = commands {
          self.process_commands(&commands, None, config)?;
        }

        // Return early since we don't want to redraw twice.
        return Ok(());
      }
      PlatformEvent::Mouse(event) => {
        handle_mouse_move(&event, state, config)
      }
      PlatformEvent::Window(window_event) => match window_event {
        WindowEvent::Focused { window, .. } => {
          handle_window_focused(&window, state, config)
        }
        WindowEvent::Shown { window, .. } => {
          handle_window_shown(window, state, config)
        }
        WindowEvent::Hidden { window, .. } => {
          handle_window_hidden(&window, state, config)
        }
        WindowEvent::MovedOrResized {
          window,
          is_interactive_start,
          is_interactive_end,
          ..
        } => handle_window_moved_or_resized(
          &window,
          is_interactive_start,
          is_interactive_end,
          state,
          config,
        ),
        WindowEvent::Minimized { window, .. } => {
          handle_window_minimized(&window, state, config)
        }
        WindowEvent::MinimizeEnded { window, .. } => {
          handle_window_minimize_ended(&window, state, config)
        }
        WindowEvent::TitleChanged { window, .. } => {
          handle_window_title_changed(&window, state, config)
        }
        WindowEvent::Destroyed { window_id, .. } => {
          handle_window_destroyed(window_id, state)
        }
      },
    }?;

    if !state.is_paused && state.pending_sync.has_changes() {
      platform_sync(state, config)?;
    }

    Ok(())
  }

  pub fn process_commands(
    &mut self,
    commands: &Vec<InvokeCommand>,
    subject_container_id: Option<Uuid>,
    config: &mut UserConfig,
  ) -> anyhow::Result<Uuid> {
    let state = &mut self.state;

    // Get the container to run WM commands with.
    let subject_container = match subject_container_id {
      Some(id) => state.container_by_id(id).with_context(|| {
        format!("No container found with the given ID '{id}'.")
      })?,
      None => state
        .focused_container()
        .context("No subject container for command.")?,
    };

    let new_subject_container_id = WindowManager::run_commands(
      commands,
      subject_container,
      state,
      config,
    )?;

    if state.pending_sync.has_changes() {
      platform_sync(state, config)?;
    }

    Ok(new_subject_container_id)
  }

  pub fn run_commands(
    commands: &Vec<InvokeCommand>,
    subject_container: Container,
    state: &mut WmState,
    config: &mut UserConfig,
  ) -> anyhow::Result<Uuid> {
    let mut current_subject_container = subject_container;

    for command in commands {
      WindowManager::run_command(
        command,
        current_subject_container.clone(),
        state,
        config,
      )?;

      // Update the subject container in case the container type changes.
      // For example, when going from a tiling to a floating window.
      current_subject_container =
        if current_subject_container.is_detached() {
          match state.container_by_id(current_subject_container.id()) {
            Some(container) => container,
            None => break,
          }
        } else {
          current_subject_container
        }
    }

    Ok(current_subject_container.id())
  }

  #[allow(clippy::too_many_lines)]
  pub fn run_command(
    command: &InvokeCommand,
    subject_container: Container,
    state: &mut WmState,
    config: &mut UserConfig,
  ) -> anyhow::Result<()> {
    // No-op if WM is currently paused.
    if state.is_paused
      && *command != InvokeCommand::WmTogglePause
      && *command != InvokeCommand::WmRestartKeybindingListener
    {
      return Ok(());
    }

    if subject_container.is_detached() {
      bail!("Cannot run command because subject container is detached.");
    }

    match &command {
      InvokeCommand::AdjustBorders(args) => {
        match subject_container.as_window_container() {
          Ok(window) => {
            let args = args.clone();
            let border_delta = RectDelta::new(
              args.left.unwrap_or(LengthValue::from_px(0)),
              args.top.unwrap_or(LengthValue::from_px(0)),
              args.right.unwrap_or(LengthValue::from_px(0)),
              args.bottom.unwrap_or(LengthValue::from_px(0)),
            );

            window.set_border_delta(border_delta);
            state.pending_sync.queue_container_to_redraw(window);

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::Close => {
        match subject_container.as_window_container() {
          Ok(window) => {
            // Window handle might no longer be valid here.
            if let Err(err) = window.native().close() {
              warn!("Failed to close window: {:?}", err);
            }

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::Focus(args) => {
        if let Some(direction) = &args.direction {
          focus_in_direction(&subject_container, direction, state)?;
        }

        if let Some(direction) = &args.workspace_in_direction {
          focus_workspace(
            WorkspaceTarget::Direction(direction.clone()),
            state,
            config,
          )?;
        }

        if let Some(container_id) = &args.container_id {
          focus_container_by_id(container_id, state)?;
        }

        if let Some(name) = &args.workspace {
          focus_workspace(
            WorkspaceTarget::Name(name.clone()),
            state,
            config,
          )?;
        }

        if let Some(monitor_index) = &args.monitor {
          focus_monitor(*monitor_index, state, config)?;
        }

        if args.next_active_workspace {
          focus_workspace(WorkspaceTarget::NextActive, state, config)?;
        }

        if args.prev_active_workspace {
          focus_workspace(WorkspaceTarget::PreviousActive, state, config)?;
        }

        if args.next_workspace {
          focus_workspace(WorkspaceTarget::Next, state, config)?;
        }

        if args.prev_workspace {
          focus_workspace(WorkspaceTarget::Previous, state, config)?;
        }

        if args.recent_workspace {
          focus_workspace(WorkspaceTarget::Recent, state, config)?;
        }

        if args.next_active_workspace_on_monitor {
          focus_workspace(
            WorkspaceTarget::NextActiveInMonitor,
            state,
            config,
          )?;
        }

        if args.prev_active_workspace_on_monitor {
          focus_workspace(
            WorkspaceTarget::PreviousActiveInMonitor,
            state,
            config,
          )?;
        }

        Ok(())
      }
      InvokeCommand::Ignore => {
        match subject_container.as_window_container() {
          Ok(window) => ignore_window(window, state),
          _ => Ok(()),
        }
      }
      InvokeCommand::Move(args) => {
        match subject_container.as_window_container() {
          Ok(window) => {
            if let Some(direction) = &args.direction {
              move_window_in_direction(
                window.clone(),
                direction,
                state,
                config,
              )?;
            }

            if let Some(direction) = &args.workspace_in_direction {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::Direction(direction.clone()),
                state,
                config,
              )?;
            }

            if let Some(name) = &args.workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::Name(name.clone()),
                state,
                config,
              )?;
            }

            if args.next_active_workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::NextActive,
                state,
                config,
              )?;
            }

            if args.prev_active_workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::PreviousActive,
                state,
                config,
              )?;
            }

            if args.next_workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::Next,
                state,
                config,
              )?;
            }

            if args.prev_workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::Previous,
                state,
                config,
              )?;
            }

            if args.recent_workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::Recent,
                state,
                config,
              )?;
            }

            if args.next_active_workspace_on_monitor {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::NextActiveInMonitor,
                state,
                config,
              )?;
            }

            if args.prev_active_workspace_on_monitor {
              move_window_to_workspace(
                window,
                WorkspaceTarget::PreviousActiveInMonitor,
                state,
                config,
              )?;
            }
            Ok(())
          }

          _ => Ok(()),
        }
      }
      InvokeCommand::MoveWorkspace { direction } => {
        let workspace =
          subject_container.workspace().context("No workspace.")?;

        move_workspace_in_direction(&workspace, direction, state, config)
      }
      InvokeCommand::Position(args) => {
        match subject_container.as_window_container() {
          Ok(window) => {
            if args.centered {
              set_window_position(
                window,
                &WindowPositionTarget::Centered,
                state,
              )
            } else {
              set_window_position(
                window,
                &WindowPositionTarget::Coordinates(args.x_pos, args.y_pos),
                state,
              )
            }
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::UpdateWorkspaceConfig {
        workspace,
        new_config,
      } => {
        let workspace = if let Some(workspace_name) = workspace {
          state
            .workspace_by_name(workspace_name)
            .context("Workspace doesn't exist.")?
        } else {
          subject_container.workspace().context("No workspace.")?
        };
        update_workspace_config(&workspace, state, config, new_config)
      }
      InvokeCommand::Resize(args) => {
        match subject_container.as_window_container() {
          Ok(window) => resize_window(
            &window,
            args.width.clone(),
            args.height.clone(),
            state,
          ),
          _ => Ok(()),
        }
      }
      InvokeCommand::SetFloating {
        centered,
        shown_on_top,
        x_pos,
        y_pos,
        width,
        height,
      } => match subject_container.as_window_container() {
        Ok(window) => {
          let floating_defaults =
            &config.value.window_behavior.state_defaults.floating;
          let centered = centered.unwrap_or(floating_defaults.centered);

          let window = update_window_state(
            window.clone(),
            WindowState::Floating(FloatingStateConfig {
              centered,
              shown_on_top: shown_on_top
                .unwrap_or(floating_defaults.shown_on_top),
            }),
            state,
            config,
          )?;

          // Allow size and position to be set if window has not previously
          // been manually placed.
          if !window.has_custom_floating_placement() {
            if width.is_some() || height.is_some() {
              set_window_size(
                window.clone(),
                width.clone(),
                height.clone(),
                state,
              )?;
            }

            if centered {
              set_window_position(
                window,
                &WindowPositionTarget::Centered,
                state,
              )?;
            } else if x_pos.is_some() || y_pos.is_some() {
              set_window_position(
                window,
                &WindowPositionTarget::Coordinates(*x_pos, *y_pos),
                state,
              )?;
            }
          }

          Ok(())
        }
        _ => Ok(()),
      },
      InvokeCommand::SetFullscreen {
        maximized,
        shown_on_top,
      } => match subject_container.as_window_container() {
        Ok(window) => {
          let fullscreen_defaults =
            &config.value.window_behavior.state_defaults.fullscreen;

          update_window_state(
            window.clone(),
            WindowState::Fullscreen(FullscreenStateConfig {
              maximized: maximized
                .unwrap_or(fullscreen_defaults.maximized),
              shown_on_top: shown_on_top
                .unwrap_or(fullscreen_defaults.shown_on_top),
            }),
            state,
            config,
          )?;

          Ok(())
        }
        _ => Ok(()),
      },
      InvokeCommand::SetMinimized => {
        match subject_container.as_window_container() {
          Ok(window) => {
            update_window_state(
              window.clone(),
              WindowState::Minimized,
              state,
              config,
            )?;

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::SetTiling => {
        match subject_container.as_window_container() {
          Ok(window) => {
            update_window_state(
              window,
              WindowState::Tiling,
              state,
              config,
            )?;

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::SetTitleBarVisibility {
        // LINT: `visibility` is only used on Windows.
        #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
        visibility,
      } => match subject_container.as_window_container() {
        #[cfg(target_os = "windows")]
        Ok(window) => {
          _ = window.native().set_title_bar_visibility(
            *visibility == TitleBarVisibility::Shown,
          );
          Ok(())
        }
        _ => Ok(()),
      },
      // LINT: `args` is only used on Windows.
      #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
      InvokeCommand::SetTransparency(args) => {
        match subject_container.as_window_container() {
          #[cfg(target_os = "windows")]
          Ok(window) => {
            if let Some(opacity) = &args.opacity {
              _ = window.native().set_transparency(opacity);
            }

            if let Some(opacity_delta) = &args.opacity_delta {
              _ = window.native().adjust_transparency(opacity_delta);
            }

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::ShellExec {
        hide_window,
        command,
      } => shell_exec(&command.join(" "), *hide_window, state),
      InvokeCommand::Size(args) => {
        match subject_container.as_window_container() {
          Ok(window) => set_window_size(
            window,
            args.width.clone(),
            args.height.clone(),
            state,
          ),
          _ => Ok(()),
        }
      }
      InvokeCommand::ToggleFloating {
        centered,
        shown_on_top,
      } => match subject_container.as_window_container() {
        Ok(window) => {
          let floating_defaults =
            &config.value.window_behavior.state_defaults.floating;

          let centered = centered.unwrap_or(floating_defaults.centered);
          let target_state = WindowState::Floating(FloatingStateConfig {
            centered,
            shown_on_top: shown_on_top
              .unwrap_or(floating_defaults.shown_on_top),
          });

          let window = update_window_state(
            window.clone(),
            window.toggled_state(target_state, config),
            state,
            config,
          )?;

          if !window.has_custom_floating_placement() && centered {
            set_window_position(
              window,
              &WindowPositionTarget::Centered,
              state,
            )?;
          }

          Ok(())
        }
        _ => Ok(()),
      },
      InvokeCommand::ToggleFullscreen {
        maximized,
        shown_on_top,
      } => match subject_container.as_window_container() {
        Ok(window) => {
          let fullscreen_defaults =
            &config.value.window_behavior.state_defaults.fullscreen;

          let target_state =
            WindowState::Fullscreen(FullscreenStateConfig {
              maximized: maximized
                .unwrap_or(fullscreen_defaults.maximized),
              shown_on_top: shown_on_top
                .unwrap_or(fullscreen_defaults.shown_on_top),
            });

          update_window_state(
            window.clone(),
            window.toggled_state(target_state, config),
            state,
            config,
          )?;

          Ok(())
        }
        _ => Ok(()),
      },
      InvokeCommand::ToggleMinimized => {
        match subject_container.as_window_container() {
          Ok(window) => {
            update_window_state(
              window.clone(),
              window.toggled_state(WindowState::Minimized, config),
              state,
              config,
            )?;

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::ToggleTiling => {
        match subject_container.as_window_container() {
          Ok(window) => {
            update_window_state(
              window.clone(),
              window.toggled_state(WindowState::Tiling, config),
              state,
              config,
            )?;

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::ToggleTilingDirection => {
        toggle_tiling_direction(subject_container, state, config)
      }
      InvokeCommand::SetTilingDirection { tiling_direction } => {
        set_tiling_direction(
          subject_container,
          state,
          config,
          tiling_direction,
        )
      }
      InvokeCommand::WmCycleFocus {
        omit_floating,
        omit_fullscreen,
        omit_minimized,
        omit_tiling,
      } => cycle_focus(
        *omit_floating,
        *omit_fullscreen,
        *omit_minimized,
        *omit_tiling,
        state,
        config,
      ),
      InvokeCommand::WmDisableBindingMode { name } => {
        disable_binding_mode(name, state);
        Ok(())
      }
      InvokeCommand::WmEnableBindingMode { name } => {
        enable_binding_mode(name, state, config)
      }
      InvokeCommand::WmExit => state.emit_exit(),
      InvokeCommand::WmRestartKeybindingListener => {
        state.emit_restart_keybinding()
      }
      InvokeCommand::WmRedraw => {
        state
          .pending_sync
          .queue_container_to_redraw(state.root_container.clone());

        Ok(())
      }
      InvokeCommand::WmReloadConfig => reload_config(state, config),
      InvokeCommand::WmTogglePause => {
        toggle_pause(state);
        Ok(())
      }
    }
  }

  /// Runs cleanup tasks when the WM is exiting.
  pub(crate) fn cleanup(
    &mut self,
    config: &mut UserConfig,
    ipc_server: &mut IpcServer,
  ) {
    self.state.emit_event(WmEvent::ApplicationExiting);

    // Ensure that the WM is unpaused, otherwise, shutdown commands won't
    // get executed.
    self.state.is_paused = false;

    // Run user's shutdown commands.
    if let Err(err) = self.process_commands(
      &config.value.general.shutdown_commands.clone(),
      None,
      config,
    ) {
      tracing::warn!("Failed to run shutdown commands: {:?}", err);
    }

    // Emit remaining WM events before exiting.
    while let Ok(wm_event) = self.event_rx.try_recv() {
      tracing::info!(
        "Emitting WM event before shutting down: {:?}",
        wm_event
      );

      if let Err(err) = ipc_server.process_event(wm_event) {
        tracing::warn!("{:?}", err);
      }
    }
  }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
  use wm_platform::{NativeWindow, WindowEventNotification};

  use super::*;
  use crate::{
    commands::{
      container::attach_container,
      window::{ignore_window, unmanage_window},
    },
    models::{TilingWindow, WindowContainer, Workspace},
  };

  /// Creates a managed window using the cleanup tests' root/workspace
  /// tree.
  ///
  /// No monitor or native `Display` is needed for event reconciliation.
  fn managed_window() -> (WmState, WindowContainer, Container) {
    let (event_tx, _) = mpsc::unbounded_channel();
    let (exit_tx, _) = mpsc::unbounded_channel();
    let (restart_tx, _) = mpsc::unbounded_channel();
    let state =
      WmState::new(Dispatcher::mock(), event_tx, exit_tx, restart_tx);
    let window = TilingWindow::mock().call();
    let workspace = Workspace::mock()
      .tiling_containers(vec![window.clone().into()])
      .call();
    assert!(attach_container(
      &workspace.clone().into(),
      &state.root_container.clone().into(),
      None,
    )
    .is_ok());
    (state, window.into(), workspace.into())
  }

  /// Creates the notification observed when switching Finder tabs.
  fn moved(window: &NativeWindow) -> WindowEvent {
    WindowEvent::MovedOrResized {
      window: window.clone(),
      is_interactive_start: false,
      is_interactive_end: false,
      notification: WindowEventNotification(None),
    }
  }

  /// A tracked AX window can be absent from WM state after tab cleanup.
  #[test]
  fn active_tab_move_readmits_evicted_window_without_focus_notification() {
    let (mut state, window, workspace) = managed_window();
    let native = window.native().clone();
    assert!(unmanage_window(window.clone(), &mut state).is_ok());
    assert!(state.window_from_native(&native).is_none());

    let mut admissions = 0;
    let result = reconcile_active_window(
      &moved(&native),
      &mut state,
      |_, _| true,
      |candidate, state| {
        assert_eq!(candidate, native);
        admissions += 1;
        // Stand in for native-property acquisition and window creation.
        attach_container(&window.clone().into(), &workspace, None)?;
        assert!(state.window_from_native(&candidate).is_some());
        Ok(())
      },
    );
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(admissions, 1);
    assert!(state.window_from_native(&native).is_some());

    // Geometry notifications after admission must not show/redraw again.
    assert!(reconcile_active_window(
      &moved(&native),
      &mut state,
      |_, _| panic!("Managed events must not probe native focus."),
      |_, _| panic!("Managed events must not run admission."),
    )
    .is_ok());
    assert!(unmanage_window(window, &mut state).is_ok());
  }

  /// Inactive, background, or invalid tabs must not gain focus via
  /// admission.
  #[test]
  fn inactive_tab_activity_does_not_readmit_window() {
    let (mut state, window, _) = managed_window();
    let native = window.native().clone();
    assert!(unmanage_window(window, &mut state).is_ok());
    assert!(reconcile_active_window(
      &moved(&native),
      &mut state,
      |_, _| false,
      |_, _| panic!("Inactive or invalid windows must not be admitted."),
    )
    .is_ok());
    assert!(state.windows().is_empty());
  }

  /// Ignoring a tab survives inactivity, cleanup, and later activation.
  #[test]
  fn ignored_tab_survives_inactive_cleanup_and_is_not_readmitted() {
    let (mut state, window, _) = managed_window();
    let native = window.native().clone();
    assert!(ignore_window(window, &mut state).is_ok());
    assert!(state.windows().is_empty());

    // Inactive tabs fail the managed probe but remain in CG OptionAll.
    assert!(state
      .cleanup_invalid_windows_with(|_| false, |_| true)
      .is_ok());
    assert_eq!(state.ignored_windows, vec![native.clone()]);

    for event in [
      moved(&native),
      WindowEvent::Focused {
        window: native.clone(),
        notification: WindowEventNotification(None),
      },
    ] {
      assert!(reconcile_active_window(
        &event,
        &mut state,
        |_, _| panic!("Ignored windows must bypass native focus lookup."),
        |_, _| panic!("Ignored windows must not be admitted."),
      )
      .is_ok());
    }
    assert!(state.windows().is_empty());
  }

  /// Destruction removes ignored entries even without a managed window.
  #[test]
  fn destroyed_ignored_tab_is_forgotten() {
    let (mut state, window, _) = managed_window();
    let native_id = window.native().id();
    assert!(ignore_window(window, &mut state).is_ok());
    assert_eq!(state.ignored_windows.len(), 1);
    assert!(state.windows().is_empty());

    assert!(handle_window_destroyed(native_id, &mut state).is_ok());
    assert!(state.ignored_windows.is_empty());
    assert!(state.windows().is_empty());
  }

  /// Periodic cleanup removes ignored ghosts when destruction is missed.
  #[test]
  fn ignored_tab_missing_from_all_windows_is_pruned() {
    let (mut state, window, _) = managed_window();
    assert!(ignore_window(window, &mut state).is_ok());
    assert_eq!(state.ignored_windows.len(), 1);

    assert!(state
      .cleanup_invalid_windows_with(|_| false, |_| false)
      .is_ok());
    assert!(state.ignored_windows.is_empty());
    assert!(state.windows().is_empty());
  }

  /// Genuine close still removes a managed tab and never admits it.
  #[test]
  fn destroyed_tab_is_unmanaged_without_reconciliation() {
    let (mut state, window, _) = managed_window();
    let native_id = window.native().id();
    let event = WindowEvent::Destroyed {
      window_id: native_id,
      notification: WindowEventNotification(None),
    };
    assert!(reconcile_active_window(
      &event,
      &mut state,
      |_, _| panic!("Destroyed events must not probe native focus."),
      |_, _| panic!("Destroyed windows must not be admitted."),
    )
    .is_ok());
    assert_eq!(state.windows().len(), 1);
    let result = handle_window_destroyed(native_id, &mut state);
    assert!(result.is_ok(), "{result:?}");
    assert!(state.windows().is_empty());
  }
}
