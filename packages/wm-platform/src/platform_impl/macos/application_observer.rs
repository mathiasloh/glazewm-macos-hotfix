use std::{
  ptr::NonNull,
  sync::{Arc, Condvar, Mutex},
  time::Duration,
};

use objc2_application_services::{AXError, AXObserver, AXUIElement};
use objc2_core_foundation::{
  kCFRunLoopDefaultMode, CFRetained, CFRunLoop, CFRunLoopSource, CFString,
};
use tokio::sync::mpsc;

use crate::{
  platform_impl::{
    AXUIElementExt, Application, NativeWindow, ProcessId,
    WindowEventNotificationInner,
  },
  Dispatcher, NativeWindowExtMacOs, ThreadBound, WindowEvent, WindowId,
};

/// Notifications to register for the `AXUIElement` of an application.
const AX_APP_NOTIFICATIONS: &[&str] =
  &["AXFocusedWindowChanged", "AXWindowCreated"];

/// Notifications to register for the `AXUIElement` of a window.
const AX_WINDOW_NOTIFICATIONS: &[&str] = &[
  "AXTitleChanged",
  "AXUIElementDestroyed",
  "AXWindowMoved",
  "AXWindowResized",
  "AXWindowDeminiaturized",
  "AXWindowMiniaturized",
];

/// Context passed to the application event callback.
#[derive(Debug)]
struct ApplicationEventContext {
  application: Application,
  events_tx: mpsc::UnboundedSender<WindowEvent>,
  app_windows: Arc<Mutex<Vec<crate::NativeWindow>>>,
  observer: CFRetained<AXObserver>,
}

/// Callback state that remains valid for the lifetime of the leaked
/// callback context. The lifecycle is kept outside
/// [`ApplicationEventContext`] so an operation can be acquired before the
/// latter is accessed.
#[derive(Debug)]
struct ObserverCallbackContext {
  lifecycle: Arc<ObserverLifecycle>,
  context: ApplicationEventContext,
}

#[derive(Debug, Default)]
struct ObserverLifecycleState {
  cancelled: bool,
  in_flight: usize,
}

/// Coordinates callbacks and delayed operations with observer teardown.
#[derive(Debug)]
struct ObserverLifecycle {
  state: Mutex<ObserverLifecycleState>,
  wake: Condvar,
}

#[derive(Debug)]
struct ObserverOperation {
  lifecycle: Arc<ObserverLifecycle>,
}

impl ObserverLifecycle {
  fn new() -> Arc<Self> {
    Arc::new(Self {
      state: Mutex::new(ObserverLifecycleState::default()),
      wake: Condvar::new(),
    })
  }

  fn acquire(self: &Arc<Self>) -> Option<ObserverOperation> {
    let mut state = self.state.lock().unwrap();
    if state.cancelled {
      return None;
    }

    state.in_flight += 1;
    Some(ObserverOperation {
      lifecycle: Arc::clone(self),
    })
  }

  fn cancel(&self) {
    let mut state = self.state.lock().unwrap();
    state.cancelled = true;

    if state.in_flight == 0 {
      self.wake.notify_all();
    }
  }

  fn wait_for_operations(&self) {
    let mut state = self.state.lock().unwrap();

    while state.in_flight > 0 {
      state = self.wake.wait(state).unwrap();
    }
  }

  fn cancel_and_wait(&self) {
    self.cancel();
    self.wait_for_operations();
  }
}

impl Drop for ObserverOperation {
  fn drop(&mut self) {
    let mut state = self.lifecycle.state.lock().unwrap();
    debug_assert!(state.in_flight > 0);
    state.in_flight -= 1;

    if state.in_flight == 0 {
      self.lifecycle.wake.notify_all();
    }
  }
}

/// Represents an accessibility observer for a specific application.
#[derive(Debug)]
pub(crate) struct ApplicationObserver {
  pub(crate) pid: ProcessId,
  app_windows: Arc<Mutex<Vec<crate::NativeWindow>>>,
  events_tx: mpsc::UnboundedSender<WindowEvent>,
  lifecycle: Arc<ObserverLifecycle>,
  _observer: CFRetained<AXObserver>,
  observer_source: CFRetained<CFRunLoopSource>,
}

/// Returns whether a rediscovered native window is absent from the
/// tracker.
fn is_untracked_window(
  tracked_ids: impl IntoIterator<Item = WindowId>,
  candidate_id: WindowId,
) -> bool {
  !tracked_ids.into_iter().any(|id| id == candidate_id)
}

/// Runs notification admission effects only for actual AX windows.
///
/// Descendants such as Finder tab buttons can share a window's CG ID.
fn with_window_notification_target<T>(
  role: crate::Result<CFRetained<CFString>>,
  admit: impl FnOnce() -> T,
) -> Option<T> {
  role
    .ok()
    .filter(|role| role.to_string() == "AXWindow")
    .map(|_| admit())
}

/// Whether a failed initial AX window enumeration should be retried.
fn should_retry_initial_window_enumeration(
  is_crossover_wine_helper: bool,
  initial_enumeration_succeeded: bool,
) -> bool {
  is_crossover_wine_helper && !initial_enumeration_succeeded
}

// TODO: Remove this.
unsafe impl Send for ApplicationObserver {}

impl ApplicationObserver {
  /// Creates a new `ApplicationObserver` for the given application.
  ///
  /// If `is_startup` is `true`, the observer will not emit
  /// `WindowEvent::Shown` for windows already running on startup.
  pub fn new(
    app: &Application,
    events_tx: mpsc::UnboundedSender<WindowEvent>,
    is_startup: bool,
  ) -> crate::Result<Self> {
    let observer = unsafe {
      let mut observer = std::ptr::null_mut();

      let result = AXObserver::create(
        app.pid,
        Some(Self::window_event_callback),
        // SAFETY: Stack address of `observer` is guaranteed to be
        // non-null.
        NonNull::new(&raw mut observer).unwrap(),
      );

      if result != AXError::Success {
        return Err(crate::Error::Accessibility(
          "AXObserverCreate".to_string(),
          result.0,
        ));
      }

      CFRetained::retain(NonNull::new(observer).ok_or_else(|| {
        crate::Error::InvalidPointer("AXObserver is null.".to_string())
      })?)
    };

    let runloop =
      CFRunLoop::current().ok_or(crate::Error::EventLoopStopped)?;

    let observer_source = unsafe { observer.run_loop_source() };
    runloop.add_source(Some(&observer_source), unsafe {
      kCFRunLoopDefaultMode
    });

    // Enumerate windows already present at observer creation time.
    // This can transiently fail for applications whose accessibility
    // hierarchy is not fully initialized (e.g. terminal emulators
    // like WezTerm). Failures are treated as non-fatal: the observer is
    // already registered, so `AXWindowCreated` will pick up any new
    // windows as they become ready.
    let is_crossover_wine_helper = app.is_crossover_wine_helper();
    let initial_windows = app.windows();
    let retry_initial_enumeration =
      should_retry_initial_window_enumeration(
        is_crossover_wine_helper,
        initial_windows.is_ok(),
      );
    let app_windows = Arc::new(Mutex::new(match initial_windows {
      Ok(windows) => windows,
      Err(err) => {
        tracing::warn!(
          "Initial window enumeration failed for PID {}: {}. \
           Falling back to notification-driven discovery{}.",
          app.pid,
          err,
          if retry_initial_enumeration {
            " and a delayed retry"
          } else {
            ""
          },
        );
        Vec::new()
      }
    }));

    let lifecycle = ObserverLifecycle::new();
    let context = Box::into_raw(Box::new(ObserverCallbackContext {
      lifecycle: lifecycle.clone(),
      context: ApplicationEventContext {
        application: app.clone(),
        events_tx: events_tx.clone(),
        app_windows: app_windows.clone(),
        observer: observer.clone(),
      },
    }));

    // Register for all window notifications.
    // TODO: Remove from runloop if registration fails.
    Self::register_app_notifications(app, &observer, context)?;

    // Emit `WindowEvent::Shown` for all existing windows.
    for window in app_windows.lock().unwrap().iter() {
      if let Err(err) =
        Self::register_window_notifications(window, &observer, context)
      {
        tracing::warn!(
          "Failed to register window notifications for PID {}: {}",
          app.pid,
          err
        );
      }

      // Don't emit `WindowEvent::Shown` for windows that are already
      // running on startup.
      if !is_startup {
        if let Err(err) = events_tx.send(WindowEvent::Shown {
          window: window.clone(),
          notification: crate::WindowEventNotification(None),
        }) {
          tracing::warn!(
            "Failed to send window event for PID {}: {}",
            app.pid,
            err
          );
        }
      }
    }

    if retry_initial_enumeration {
      Self::schedule_window_reenumeration(
        context,
        lifecycle.clone(),
        app.dispatcher.clone(),
        app.pid,
        Duration::from_millis(500),
        !is_startup,
      );
    }

    Ok(Self {
      pid: app.pid,
      app_windows,
      events_tx,
      lifecycle,
      _observer: observer,
      observer_source,
    })
  }

  fn register_app_notifications(
    app: &Application,
    observer: &CFRetained<AXObserver>,
    context: *mut ObserverCallbackContext,
  ) -> crate::Result<()> {
    for notification in AX_APP_NOTIFICATIONS {
      unsafe {
        let notification_cfstr = CFString::from_static_str(notification);
        let result = observer.add_notification(
          app.ax_element.get_ref()?,
          &notification_cfstr,
          context.cast::<std::ffi::c_void>(),
        );

        if result != AXError::Success {
          return Err(crate::Error::Platform(format!(
            "Failed to add notification {} for PID {}: {:?}",
            notification, app.pid, result
          )));
        }
      }
    }

    Ok(())
  }

  fn register_window_notifications(
    window: &crate::NativeWindow,
    observer: &CFRetained<AXObserver>,
    context: *mut ObserverCallbackContext,
  ) -> crate::Result<()> {
    for notification in AX_WINDOW_NOTIFICATIONS {
      unsafe {
        let notification_cfstr = CFString::from_static_str(notification);
        let result = observer.add_notification(
          window.ax_ui_element().get_ref()?,
          &notification_cfstr,
          context.cast::<std::ffi::c_void>(),
        );

        if result != AXError::Success {
          return Err(crate::Error::Platform(format!(
            "Failed to add notification {} for window {}: {:?}",
            notification,
            window.id().0,
            result
          )));
        }
      }
    }

    Ok(())
  }

  pub(crate) fn cancel_and_wait(&self) {
    self.lifecycle.cancel_and_wait();
  }

  pub(crate) fn emit_all_windows_destroyed(&self) {
    for window in self.app_windows.lock().unwrap().iter() {
      if let Err(err) = self.events_tx.send(WindowEvent::Destroyed {
        window_id: window.id(),
        notification: crate::WindowEventNotification(None),
      }) {
        tracing::warn!(
          "Failed to send window event for PID {}: {}",
          self.pid,
          err
        );
      }
    }
  }

  pub(crate) fn emit_all_windows_hidden(&self) {
    for window in self.app_windows.lock().unwrap().iter() {
      if let Err(err) = self.events_tx.send(WindowEvent::Hidden {
        window: window.clone(),
        notification: crate::WindowEventNotification(None),
      }) {
        tracing::warn!(
          "Failed to send window event for PID {}: {}",
          self.pid,
          err
        );
      }
    }
  }

  pub(crate) fn emit_all_windows_shown(&self) {
    for window in self.app_windows.lock().unwrap().iter() {
      if let Err(err) = self.events_tx.send(WindowEvent::Shown {
        window: window.clone(),
        notification: crate::WindowEventNotification(None),
      }) {
        tracing::warn!(
          "Failed to send window event for PID {}: {}",
          self.pid,
          err
        );
      }
    }
  }

  /// Re-enumerates windows after Accessibility has had time to initialize.
  ///
  /// The observer remains registered while this retry runs, so any window
  /// created in the meantime is de-duplicated against `app_windows`.
  fn schedule_window_reenumeration(
    context: *mut ObserverCallbackContext,
    lifecycle: Arc<ObserverLifecycle>,
    dispatcher: Dispatcher,
    pid: ProcessId,
    delay: Duration,
    emit_shown: bool,
  ) {
    // SAFETY: The callback context is heap-allocated via `Box::into_raw`
    // and intentionally remains valid for the lifetime of the process.
    let context_addr = context as usize;

    std::thread::spawn(move || {
      std::thread::sleep(delay);

      // Acquire the lifecycle operation inside the event-loop closure. If
      // dispatching fails during shutdown, the closure is never run and no
      // operation is left for teardown to wait on.
      let result = dispatcher.dispatch_async(move || {
        let Some(_operation) = lifecycle.acquire() else {
          return;
        };

        // SAFETY: The callback context is intentionally leaked and
        // therefore remains valid. The operation guard prevents
        // teardown from invalidating the observer while its nested
        // context is accessed.
        let callback_context =
          unsafe { &*(context_addr as *const ObserverCallbackContext) };
        let context = &callback_context.context;
        let current_windows = match context.application.windows() {
          Ok(windows) => windows,
          Err(err) => {
            tracing::debug!(
              "Window re-enumeration failed for PID {}: {}",
              pid,
              err,
            );
            return;
          }
        };

        let mut tracked = context.app_windows.lock().unwrap();

        for new_window in current_windows {
          if !is_untracked_window(
            tracked.iter().map(crate::NativeWindow::id),
            new_window.id(),
          ) {
            continue;
          }

          // WindowServer is authoritative for whether the native ID still
          // belongs to this application.
          if !new_window.is_valid() {
            tracing::debug!(
              "Skipping WindowServer-invalid window {} for PID {}.",
              new_window.id().0,
              pid,
            );
            continue;
          }

          tracing::info!(
            "Re-discovered window {} for PID {}.",
            new_window.id().0,
            pid,
          );

          tracked.push(new_window.clone());

          if let Err(err) = Self::register_window_notifications(
            &new_window,
            &context.observer,
            context_addr as *mut ObserverCallbackContext,
          ) {
            tracing::warn!(
              "Failed to register window notifications for re-discovered \
               window {}: {}",
              new_window.id().0,
              err,
            );
          }

          if emit_shown {
            let _ = context.events_tx.send(WindowEvent::Shown {
              window: new_window,
              notification: crate::WindowEventNotification(None),
            });
          }
        }
      });

      if let Err(err) = result {
        tracing::debug!(
          "Failed to dispatch window re-enumeration for PID {}: {}",
          pid,
          err,
        );
      }
    });
  }

  /// Callback function for accessibility window events.
  #[allow(clippy::too_many_lines)]
  unsafe extern "C-unwind" fn window_event_callback(
    _observer: NonNull<AXObserver>,
    element: NonNull<AXUIElement>,
    notification_name: NonNull<CFString>,
    context: *mut std::ffi::c_void,
  ) {
    if context.is_null() {
      tracing::error!("Window event callback received null context.");
      return;
    }

    let callback_context_ptr = context.cast::<ObserverCallbackContext>();
    // The outer callback context is intentionally leaked, so reading its
    // lifecycle handle is safe even when teardown has already started.
    let lifecycle = unsafe { (*callback_context_ptr).lifecycle.clone() };
    let Some(_operation) = lifecycle.acquire() else {
      return;
    };

    // The operation guard now covers all access to the nested application
    // context, including tracking, notification registration, and
    // emission.
    let callback_context = unsafe { &*callback_context_ptr };
    let context = &callback_context.context;
    let ax_element = unsafe { CFRetained::retain(element) };
    let notification = WindowEventNotificationInner {
      name: notification_name.as_ref().to_string(),
      ax_element_ptr: element.as_ptr().cast::<std::ffi::c_void>(),
    };

    tracing::debug!(
      "Received window event: {} for PID: {}",
      notification.name,
      context.application.pid
    );

    if notification.name.as_str() == "AXUIElementDestroyed" {
      // Destroyed elements may no longer answer attribute queries.
      let found_window = context
        .app_windows
        .lock()
        .unwrap()
        .iter()
        .find(|window| {
          window.ax_ui_element().get_ref().ok() == Some(&ax_element)
        })
        .cloned();

      if let Some(window) = &found_window {
        context
          .app_windows
          .lock()
          .unwrap()
          .retain(|w| w.id() != window.id());

        if let Err(err) = context.events_tx.send(WindowEvent::Destroyed {
          window_id: window.id(),
          notification: crate::WindowEventNotification(Some(notification)),
        }) {
          tracing::warn!(
            "Failed to send window event for PID {}: {}",
            context.application.pid,
            err
          );
        }

        // Re-enumerate the application's windows after a short delay
        // to catch replacements whose `AXWindowCreated` notification
        // was missed (e.g. Spotify rapidly destroys and recreates its
        // window on launch).
        Self::schedule_window_reenumeration(
          callback_context_ptr,
          lifecycle.clone(),
          context.application.dispatcher.clone(),
          context.application.pid,
          Duration::from_millis(500),
          true,
        );
      }

      return;
    }

    // Focus notifications can name the application rather than its window.
    let ax_element = if notification.name == "AXFocusedWindowChanged"
      && ax_element
        .get_attribute::<CFString>("AXRole")
        .is_ok_and(|role| role.to_string() == "AXApplication")
    {
      let Ok(window_element) =
        ax_element.get_attribute::<AXUIElement>("AXFocusedWindow")
      else {
        return;
      };
      window_element
    } else {
      ax_element
    };

    let Some(window) = with_window_notification_target(
      ax_element.get_attribute::<CFString>("AXRole"),
      || {
        let found_window = {
          let app_windows = context.app_windows.lock().unwrap();

          app_windows
            .iter()
            .find(|window| {
              window.ax_ui_element().get_ref().ok() == Some(&ax_element)
            })
            .cloned()
        };

        let is_new_window = found_window.is_none();
        let window = found_window.unwrap_or_else(|| {
          let window_id = WindowId::from_window_element(&ax_element);
          let ax_element = ThreadBound::new(
            ax_element,
            context.application.dispatcher.clone(),
          );
          NativeWindow::new(
            window_id,
            ax_element,
            context.application.clone(),
          )
          .into()
        });

        if is_new_window {
          context.app_windows.lock().unwrap().push(window.clone());
          let _ = Self::register_window_notifications(
            &window,
            &context.observer,
            callback_context_ptr,
          );

          if let Err(err) = context.events_tx.send(WindowEvent::Shown {
            window: window.clone(),
            notification: crate::WindowEventNotification(Some(
              notification.clone(),
            )),
          }) {
            tracing::warn!(
              "Failed to send window event for PID {}: {}",
              context.application.pid,
              err
            );
          }
        }

        window
      },
    ) else {
      return;
    };

    let window_event = match notification.name.as_str() {
      "AXFocusedWindowChanged" => WindowEvent::Focused {
        window,
        notification: crate::WindowEventNotification(Some(notification)),
      },
      "AXWindowMoved" | "AXWindowResized" => WindowEvent::MovedOrResized {
        window,
        is_interactive_start: false,
        is_interactive_end: false,
        notification: crate::WindowEventNotification(Some(notification)),
      },
      "AXWindowMiniaturized" => WindowEvent::Minimized {
        window,
        notification: crate::WindowEventNotification(Some(notification)),
      },
      "AXWindowDeminiaturized" => WindowEvent::MinimizeEnded {
        window,
        notification: crate::WindowEventNotification(Some(notification)),
      },
      "AXTitleChanged" => WindowEvent::TitleChanged {
        window,
        notification: crate::WindowEventNotification(Some(notification)),
      },
      _ => {
        tracing::debug!(
          "Unhandled window notification: {} for PID: {}",
          notification.name,
          context.application.pid
        );
        return;
      }
    };

    if let Err(err) = context.events_tx.send(window_event) {
      tracing::warn!(
        "Failed to send window event for PID {}: {}",
        context.application.pid,
        err
      );
    }
  }
}

impl Drop for ApplicationObserver {
  fn drop(&mut self) {
    self.cancel_and_wait();

    // Invalidate the runloop source after all callbacks and detached
    // operations have finished. This is thread-safe and is OK to call
    // after the run loop is stopped.
    self.observer_source.invalidate();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Finder's `AXTabButton` has role `AXRadioButton` and can share a CG ID
  /// with its window; it must never reach notification admission effects.
  #[test]
  fn notification_admission_rejects_tab_buttons_and_accepts_windows() {
    let mut admissions = Vec::new();

    let rejected = with_window_notification_target(
      Ok(CFString::from_static_str("AXRadioButton")),
      || admissions.push("tab"),
    );
    assert_eq!(rejected, None);
    assert!(admissions.is_empty());

    let accepted = with_window_notification_target(
      Ok(CFString::from_static_str("AXWindow")),
      || admissions.push("window"),
    );
    assert_eq!(accepted, Some(()));
    assert_eq!(admissions, vec!["window"]);

    let unavailable = with_window_notification_target(
      Err(crate::Error::Platform("Role lookup failed.".to_string())),
      || admissions.push("unavailable"),
    );
    assert_eq!(unavailable, None);
    assert_eq!(admissions, vec!["window"]);
  }

  #[test]
  fn rediscovered_window_is_only_tracked_once() {
    let candidate_id = WindowId(42);

    assert!(!is_untracked_window([WindowId(42)], candidate_id));
    assert!(is_untracked_window([WindowId(7)], candidate_id));
  }

  #[test]
  fn initial_enumeration_retry_is_scoped_to_crossover_helpers() {
    assert!(should_retry_initial_window_enumeration(true, false));
    assert!(!should_retry_initial_window_enumeration(false, false));
    assert!(!should_retry_initial_window_enumeration(true, true));
  }

  #[test]
  fn lifecycle_rejects_operations_after_cancellation() {
    let lifecycle = ObserverLifecycle::new();

    lifecycle.cancel_and_wait();

    assert!(lifecycle.acquire().is_none());
  }

  #[test]
  fn lifecycle_waits_for_and_releases_in_flight_operations() {
    let lifecycle = ObserverLifecycle::new();
    let operation = lifecycle.acquire().unwrap();
    lifecycle.cancel();

    assert!(lifecycle.acquire().is_none());
    drop(operation);

    // Waiting after the guard is released must return immediately.
    lifecycle.wait_for_operations();
    lifecycle.cancel_and_wait();
  }
}
