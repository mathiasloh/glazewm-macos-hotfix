use std::{cell::RefCell, ffi::c_void, sync::Arc};

use objc2::MainThreadMarker;
use objc2_app_kit::{
  NSApplicationActivationOptions, NSWindow, NSWorkspace,
};
use objc2_application_services::{AXError, AXValue};
use objc2_core_foundation::{
  CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString,
  CGPoint, CGSize,
};
use objc2_core_graphics::{
  kCGWindowNumber, kCGWindowOwnerPID, CGDisplayIsAsleep, CGError,
  CGWindowListCopyWindowInfo, CGWindowListOption,
};

use crate::{
  platform_impl::{
    self, ffi, AXUIElement, AXUIElementExt, AXValueExt, Application,
    ProcessId,
  },
  Dispatcher, Point, Rect, ThreadBound, WindowId,
};

/// Result of checking a native window ID against `WindowServer`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowServerPresence {
  Present,
  Missing,
  OwnerMismatch,
  Unavailable,
}

/// Window identity data returned by `WindowServer`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowServerWindow {
  id: WindowId,
  owner_pid: ProcessId,
}

/// Determines whether a `WindowServer` lookup result is safe to keep
/// managed.
fn is_valid_window_server_presence(
  presence: WindowServerPresence,
  is_system_transitioning: bool,
) -> bool {
  match presence {
    WindowServerPresence::Present | WindowServerPresence::Unavailable => {
      true
    }
    WindowServerPresence::Missing
    | WindowServerPresence::OwnerMismatch => is_system_transitioning,
  }
}

/// Classifies the `WindowServer` records for a specific native window.
fn classify_window_server_presence(
  windows: impl IntoIterator<Item = WindowServerWindow>,
  window_id: WindowId,
  owner_pid: ProcessId,
) -> WindowServerPresence {
  let mut has_owner_mismatch = false;

  for window in windows {
    if window.id != window_id {
      continue;
    }

    if window.owner_pid == owner_pid {
      return WindowServerPresence::Present;
    }

    has_owner_mismatch = true;
  }

  if has_owner_mismatch {
    WindowServerPresence::OwnerMismatch
  } else {
    WindowServerPresence::Missing
  }
}

/// Returns the `WindowServer` query options and relative window ID.
///
/// Inactive native tabs require `OptionAll`, whose API contract requires
/// `kCGNullWindowID` (0). The candidate ID remains separate for filtering.
fn window_server_query_arguments(
  window_id: WindowId,
  include_inactive_tabs: bool,
) -> (CGWindowListOption, u32) {
  if include_inactive_tabs {
    (CGWindowListOption::OptionAll, 0)
  } else {
    (CGWindowListOption::OptionIncludingWindow, window_id.0)
  }
}

/// Gets a numeric value from a `WindowServer` window description.
fn window_description_number<'a>(
  description: &'a CFDictionary,
  key: &'static CFString,
) -> Option<&'a CFNumber> {
  let value = unsafe {
    // SAFETY: `description` is a valid WindowServer dictionary and `key`
    // is a framework-provided CFString.
    description.value(std::ptr::from_ref(key).cast::<c_void>())
  };

  // SAFETY: CoreGraphics documents these numeric fields as `CFNumber`
  // values; a null pointer is handled by `as_ref`.
  unsafe { value.cast::<CFNumber>().as_ref() }
}

/// Queries whether `WindowServer` still owns a native window ID for a
/// process.
fn window_server_presence(
  window_id: WindowId,
  owner_pid: ProcessId,
  include_inactive_tabs: bool,
) -> WindowServerPresence {
  let (options, relative_window_id) =
    window_server_query_arguments(window_id, include_inactive_tabs);
  let Some(window_descriptions) =
    CGWindowListCopyWindowInfo(options, relative_window_id)
  else {
    return WindowServerPresence::Unavailable;
  };

  let window_descriptions = (*window_descriptions).as_opaque();
  let mut windows = Vec::new();
  let mut has_malformed_target = false;

  for index in 0..window_descriptions.count() {
    let description = unsafe {
      // SAFETY: `CGWindowListCopyWindowInfo` returns dictionaries in its
      // window-description array; a null entry is handled by `as_ref`.
      window_descriptions
        .value_at_index(index)
        .cast::<CFDictionary>()
        .as_ref()
    };
    let Some(description) = description else {
      continue;
    };

    let window_number = unsafe {
      // SAFETY: `kCGWindowNumber` is a framework-provided constant.
      window_description_number(description, kCGWindowNumber)
    };
    let Some(id) = window_number
      .and_then(objc2_core_foundation::CFNumber::as_i64)
      .and_then(|id| u32::try_from(id).ok())
      .map(WindowId)
    else {
      continue;
    };

    if id != window_id {
      continue;
    }

    let owner_pid = unsafe {
      // SAFETY: `kCGWindowOwnerPID` is a framework-provided constant.
      window_description_number(description, kCGWindowOwnerPID)
    };
    let Some(owner_pid) = owner_pid.and_then(objc2_core_foundation::CFNumber::as_i32)
    else {
      has_malformed_target = true;
      continue;
    };

    windows.push(WindowServerWindow { id, owner_pid });
  }

  if has_malformed_target {
    return WindowServerPresence::Unavailable;
  }

  classify_window_server_presence(windows, window_id, owner_pid)
}

/// Returns whether macOS is transitioning between the active and login
/// sessions.
fn is_system_transitioning() -> bool {
  let has_login_window = NSWorkspace::sharedWorkspace()
    .frontmostApplication()
    .and_then(|app| app.bundleIdentifier())
    .is_some_and(|id| id.to_string() == "com.apple.loginwindow");

  // AX calls transiently fail with `InvalidUIElement` during sleep/wake.
  //
  // Events during sleep:
  //   1. Display goes asleep.
  //   2. AX calls fail with `InvalidUIElement`.
  //   3. Login window activates.
  //
  // Events during wake:
  //   1. Display wakes up.
  //   2. Login window deactivates and AX calls succeed again.
  CGDisplayIsAsleep(0) || has_login_window
}

/// Native identity retained without retaining another window's visit
/// history.
type TabVisit = (WindowId, Arc<ThreadBound<CFRetained<AXUIElement>>>);

/// Platform-specific implementation of [`NativeWindow`].
#[derive(Clone, Debug)]
pub(crate) struct NativeWindow {
  pub(crate) id: WindowId,
  pub(crate) element: Arc<ThreadBound<CFRetained<AXUIElement>>>,
  pub(crate) application: Application,
  #[cfg(feature = "test_utils")]
  pub(crate) mock_tab:
    Option<Arc<std::sync::Mutex<crate::test_utils::MockTab>>>,
  tab_group: Arc<ThreadBound<RefCell<Option<CFRetained<AXUIElement>>>>>,
  visited_tabs: Arc<std::sync::Mutex<Vec<TabVisit>>>,
}

impl NativeWindow {
  /// Creates an instance of `NativeWindow`.
  #[must_use]
  pub(crate) fn new(
    id: WindowId,
    element: ThreadBound<CFRetained<AXUIElement>>,
    application: Application,
  ) -> Self {
    let tab_group = Arc::new(ThreadBound::new(
      RefCell::new(None),
      application.dispatcher.clone(),
    ));
    Self {
      #[cfg(feature = "test_utils")]
      mock_tab: None,
      tab_group,
      visited_tabs: Arc::default(),
      element: Arc::new(element),
      id,
      application,
    }
  }

  /// Refreshes group identity only while this window is eligible.
  ///
  /// Inactive tabs do not expose their group. Retain their last active
  /// identity, but clear failed or missing lookups on eligible windows so
  /// detaching or moving a tab cannot keep an observed stale identity.
  pub(crate) fn refresh_tab_group(&self) {
    #[cfg(feature = "test_utils")]
    if let Some(mock) = &self.mock_tab {
      let mut mock = mock.lock().expect("Mock lock.");
      if mock.valid {
        if let Some(group) = mock.queried_group {
          if group != mock.group || group.is_none() {
            self.clear_visited_tabs();
          }
          mock.group = group;
        }
      }
      return;
    }
    if !self.is_valid() {
      return;
    }

    let _ = self.element.with(|el| {
      let group =
        (|| -> crate::Result<Option<CFRetained<AXUIElement>>> {
          if el.get_attribute::<CFString>("AXRole")?.to_string()
            != "AXWindow"
          {
            return Ok(None);
          }
          let children =
            el.get_attribute::<CFArray<AXUIElement>>("AXChildren")?;
          let mut group = None;
          for child in children.iter() {
            if child.get_attribute::<CFString>("AXRole")?.to_string()
              == "AXTabGroup"
            {
              if group.is_some() {
                return Ok(None);
              }
              group = Some(child);
            }
          }
          Ok(group)
        })()
        .ok()
        .flatten();
      if let Ok(cached) = self.tab_group.get_ref() {
        if *cached.borrow() != group || group.is_none() {
          self.clear_visited_tabs();
        }
        *cached.borrow_mut() = group;
      }
    });
  }

  /// Compares retained native group identities on their owning thread.
  pub(crate) fn has_same_tab_group(&self, other: &Self) -> bool {
    #[cfg(feature = "test_utils")]
    if let Some(mock) = &self.mock_tab {
      let (owner, group) = {
        let mock = mock.lock().expect("Mock lock.");
        (mock.owner, mock.group)
      };
      return other.mock_tab.as_ref().is_some_and(|other| {
        let other = other.lock().expect("Mock lock.");
        group.is_some() && owner == other.owner && group == other.group
      });
    }
    self.application.pid == other.application.pid
      && self
        .tab_group
        .with(|group| {
          let Ok(other_group) = other.tab_group.get_ref() else {
            return false;
          };
          match (&*group.borrow(), &*other_group.borrow()) {
            (Some(group), Some(other_group)) => group == other_group,
            _ => false,
          }
        })
        .unwrap_or(false)
  }

  /// Forgets associations when the eligible window leaves its group.
  fn clear_visited_tabs(&self) {
    if let Ok(mut visited) = self.visited_tabs.lock() {
      visited.clear();
    }
  }

  /// Copies bounded visit history after the caller proves group equality.
  pub(crate) fn remember_tab_group_member(&self, previous: &Self) {
    // ponytail: retain the last 32 visits; older tabs need fresh group
    // proof.
    let Ok(mut history) =
      previous.visited_tabs.lock().map(|ids| ids.clone())
    else {
      return;
    };
    history.retain(|(id, _)| *id != self.id && *id != previous.id);
    history.push((previous.id, previous.element.clone()));
    if history.len() > 32 {
      history.remove(0);
    }
    if let Ok(mut visited) = self.visited_tabs.lock() {
      *visited = history;
    }
  }

  /// Checks only previously associated native IDs, never owner proximity.
  pub(crate) fn has_visited_tab(&self, candidate: &Self) -> bool {
    let element = self.visited_tabs.lock().ok().and_then(|visited| {
      visited
        .iter()
        .find(|(id, _)| *id == candidate.id)
        .map(|(_, element)| element.clone())
    });
    element.is_some_and(|element| {
      element
        .with(|element| {
          candidate
            .element
            .get_ref()
            .is_ok_and(|current| current == element)
        })
        .unwrap_or(false)
    })
  }

  /// Implements [`NativeWindow::id`].
  pub(crate) fn id(&self) -> WindowId {
    self.id
  }

  /// Implements [`NativeWindow::title`].
  pub(crate) fn title(&self) -> crate::Result<String> {
    #[cfg(feature = "test_utils")]
    if let Some(_mock) = &self.mock_tab {
      return Ok(format!("Tab {}", self.id.0));
    }
    self.element.with(|el| {
      el.get_attribute::<CFString>("AXTitle")
        .map(|cf_string| cf_string.to_string())
    })?
  }

  /// Implements [`NativeWindow::process_name`].
  pub(crate) fn process_name(&self) -> crate::Result<String> {
    #[cfg(feature = "test_utils")]
    if let Some(_mock) = &self.mock_tab {
      return Ok("Finder".into());
    }
    self
      .application
      .process_name()
      .ok_or(crate::Error::Platform(
        "Failed to get application process name.".to_string(),
      ))
  }

  /// Implements [`NativeWindow::frame`].
  pub(crate) fn frame(&self) -> crate::Result<Rect> {
    #[cfg(feature = "test_utils")]
    if let Some(_mock) = &self.mock_tab {
      return Ok(Rect::from_xy(0, 0, 300, 200));
    }
    // TODO: Consider refactoring this to use a single dispatch.
    // TODO: Would `AXFrame` work instead?
    let size = self.size()?;
    let position = self.position()?;
    #[allow(clippy::cast_possible_truncation)]
    Ok(Rect::from_xy(
      position.0 as i32,
      position.1 as i32,
      size.0 as i32,
      size.1 as i32,
    ))
  }

  /// Implements [`NativeWindow::position`].
  pub(crate) fn position(&self) -> crate::Result<(f64, f64)> {
    self.element.with(move |el| {
      el.get_attribute::<AXValue>("AXPosition")
        .and_then(|ax_value| ax_value.value_strict::<CGPoint>())
        .map(|point| (point.x, point.y))
    })?
  }

  /// Implements [`NativeWindow::size`].
  pub(crate) fn size(&self) -> crate::Result<(f64, f64)> {
    self.element.with(move |el| {
      el.get_attribute::<AXValue>("AXSize")
        .and_then(|ax_value| ax_value.value_strict::<CGSize>())
        .map(|size| (size.width, size.height))
    })?
  }

  /// Implements [`NativeWindow::is_valid`].
  pub(crate) fn is_valid(&self) -> bool {
    #[cfg(feature = "test_utils")]
    if let Some(_mock) = &self.mock_tab {
      let mock = _mock;
      return mock.lock().expect("Mock lock.").valid;
    }
    self.has_window_server_presence(false)
  }

  /// Implements [`crate::NativeWindowExtMacOs::is_alive`].
  pub(crate) fn is_alive(&self) -> bool {
    #[cfg(feature = "test_utils")]
    if let Some(_mock) = &self.mock_tab {
      return true;
    }
    self.has_window_server_presence(true)
  }

  /// Checks native identity, optionally including inactive native tabs.
  fn has_window_server_presence(
    &self,
    include_inactive_tabs: bool,
  ) -> bool {
    let presence = window_server_presence(
      self.id,
      self.application.pid,
      include_inactive_tabs,
    );
    let is_system_transitioning = matches!(
      presence,
      WindowServerPresence::Missing | WindowServerPresence::OwnerMismatch
    ) && is_system_transitioning();

    is_valid_window_server_presence(presence, is_system_transitioning)
  }

  /// Implements [`NativeWindow::is_visible`].
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn is_visible(&self) -> crate::Result<bool> {
    #[cfg(feature = "test_utils")]
    if let Some(_mock) = &self.mock_tab {
      return Ok(true);
    }
    Ok(!self.application.is_hidden())
  }

  /// Implements [`NativeWindow::is_minimized`].
  pub(crate) fn is_minimized(&self) -> crate::Result<bool> {
    #[cfg(feature = "test_utils")]
    if let Some(_mock) = &self.mock_tab {
      return Ok(false);
    }
    self.element.with(|el| {
      el.get_attribute::<CFBoolean>("AXMinimized")
        .map(|cf_bool| cf_bool.value())
    })?
  }

  /// Implements [`NativeWindow::is_maximized`].
  pub(crate) fn is_maximized(&self) -> crate::Result<bool> {
    #[cfg(feature = "test_utils")]
    if let Some(_mock) = &self.mock_tab {
      return Ok(false);
    }
    self.element.with(|el| {
      el.get_attribute::<CFBoolean>("AXFullScreen")
        .map(|cf_bool| cf_bool.value())
    })?
  }

  /// Implements [`NativeWindow::is_resizable`].
  #[allow(clippy::unnecessary_wraps, clippy::unused_self)]
  pub(crate) fn is_resizable(&self) -> crate::Result<bool> {
    #[cfg(feature = "test_utils")]
    if let Some(_mock) = &self.mock_tab {
      return Ok(true);
    }
    // TODO: Not sure if this is even available via the AX API.
    Ok(true)
  }

  /// Implements [`NativeWindow::is_desktop_window`].
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn is_desktop_window(&self) -> crate::Result<bool> {
    Ok(
      self.application.bundle_id() == Some("com.apple.finder".to_string()),
    )
  }

  /// Implements [`NativeWindow::set_frame`].
  pub(crate) fn set_frame(&self, rect: &Rect) -> crate::Result<()> {
    #[cfg(feature = "test_utils")]
    if let Some(mock) = &self.mock_tab {
      mock
        .lock()
        .expect("Mock lock.")
        .operations
        .push("set_frame".into());
      return Ok(());
    }
    // TODO: Consider adding a separate `set_frame_async` method which
    // spawns a thread. Calling blocking AXUIElement methods from different
    // threads supposedly works fine.
    // TODO: Refactor the repeated `set_attribute` calls.
    let rect = rect.clone();
    self.with_enhanced_ui_disabled(move |el| -> crate::Result<()> {
      let ax_size = CGSize::new(rect.width().into(), rect.height().into());
      let ax_value = AXValue::new_strict(&ax_size)?;
      el.set_attribute("AXSize", &ax_value)?;
      let ax_point = CGPoint::new(rect.x().into(), rect.y().into());
      let ax_value = AXValue::new_strict(&ax_point)?;
      el.set_attribute("AXPosition", &ax_value)?;
      let ax_size = CGSize::new(rect.width().into(), rect.height().into());
      let ax_value = AXValue::new_strict(&ax_size)?;
      el.set_attribute("AXSize", &ax_value)
    })
  }

  /// Implements [`NativeWindow::resize`].
  pub(crate) fn resize(
    &self,
    width: i32,
    height: i32,
  ) -> crate::Result<()> {
    self.with_enhanced_ui_disabled(move |el| -> crate::Result<()> {
      let ax_size = CGSize::new(width.into(), height.into());
      let ax_value = AXValue::new_strict(&ax_size)?;
      el.set_attribute("AXSize", &ax_value)
    })
  }

  /// Implements [`NativeWindow::reposition`].
  pub(crate) fn reposition(&self, x: i32, y: i32) -> crate::Result<()> {
    self.with_enhanced_ui_disabled(move |el| -> crate::Result<()> {
      let ax_point = CGPoint::new(x.into(), y.into());
      let ax_value = AXValue::new_strict(&ax_point)?;
      el.set_attribute("AXPosition", &ax_value)
    })
  }

  /// Implements [`NativeWindow::minimize`].
  pub(crate) fn minimize(&self) -> crate::Result<()> {
    self.element.with(move |el| -> crate::Result<()> {
      let ax_bool = CFBoolean::new(true);
      el.set_attribute::<CFBoolean>("AXMinimized", &ax_bool.into())
    })?
  }

  /// Implements [`NativeWindow::maximize`].
  pub(crate) fn maximize(&self) -> crate::Result<()> {
    self.element.with(move |el| -> crate::Result<()> {
      let ax_bool = CFBoolean::new(true);
      el.set_attribute::<CFBoolean>("AXFullScreen", &ax_bool.into())
    })?
  }

  /// Implements [`NativeWindow::focus`].
  pub(crate) fn focus(&self) -> crate::Result<()> {
    #[cfg(feature = "test_utils")]
    if let Some(mock) = &self.mock_tab {
      mock
        .lock()
        .expect("Mock lock.")
        .operations
        .push("focus".into());
      return Ok(());
    }
    let psn = self.application.psn()?;
    self.set_front_process(&psn)?;
    self.set_key_window(&psn)?;
    self.raise()
  }

  /// Implements [`NativeWindow::close`].
  pub(crate) fn close(&self) -> crate::Result<()> {
    self.element.with(|el| -> crate::Result<()> {
      let close_button =
        el.get_attribute::<AXUIElement>("AXCloseButton")?;

      // Simulate pressing the window's close button.
      let result = unsafe {
        close_button.perform_action(&CFString::from_str("AXPress"))
      };

      if result != AXError::Success {
        return Err(crate::Error::Accessibility(
          "AXPress".to_string(),
          result.0,
        ));
      }

      Ok(())
    })?
  }

  /// Executes a callback with the `AXEnhancedUserInterface` attribute
  /// temporarily disabled on the application `AXUIElement`.
  ///
  /// This is to prevent inconsistent window resizing and repositioning
  /// for certain applications (e.g. Firefox).
  ///
  /// References:
  /// - <https://github.com/koekeishiya/yabai/commit/3fe4c77b001e1a4f613c26f01ea68c0f09327f3a>
  /// - <https://github.com/rxhanson/Rectangle/pull/285>
  fn with_enhanced_ui_disabled<F, R>(
    &self,
    callback: F,
  ) -> crate::Result<R>
  where
    F: FnOnce(&CFRetained<AXUIElement>) -> crate::Result<R> + Send,
    R: Send,
  {
    self.application.ax_element.with(|app_el| {
      // Get whether enhanced UI is currently enabled.
      let was_enabled = app_el
        .get_attribute::<CFBoolean>("AXEnhancedUserInterface")
        .is_ok_and(|cf_bool| cf_bool.value());

      // Disable enhanced UI if it was enabled.
      if was_enabled {
        let ax_bool = CFBoolean::new(false);
        let _ = app_el.set_attribute::<CFBoolean>(
          "AXEnhancedUserInterface",
          &ax_bool.into(),
        );
      }

      // Execute the callback with the window element.
      let result = self.element.with(callback);

      // Restore enhanced UI if it was originally enabled.
      if was_enabled {
        let ax_bool = CFBoolean::new(true);
        let _ = app_el.set_attribute::<CFBoolean>(
          "AXEnhancedUserInterface",
          &ax_bool.into(),
        );
      }

      result
    })??
  }

  fn raise(&self) -> crate::Result<()> {
    self.element.with(move |el| -> crate::Result<()> {
      // This has a couple of caveats:
      // - Some windows do not get raised without first calling
      //   `_SLPSSetFrontProcessWithOptions`.
      // - This changes focus if raising a window of the frontmost (active)
      //   application. For example, if 2 Chrome windows are open and one
      //   is focused, raising the other will change focus to the other
      //   window.
      //
      // Because of these caveats, this method is not exposed as a public
      // API. It's also the reason why the GlazeWM feature of bringing all
      // tiling/floating windows to the front on focus change is not
      // implemented for macOS.
      let result =
        unsafe { el.perform_action(&CFString::from_str("AXRaise")) };

      if result != AXError::Success {
        return Err(crate::Error::Accessibility(
          "AXRaise".to_string(),
          result.0,
        ));
      }

      Ok(())
    })?
  }

  fn set_front_process(
    &self,
    psn: &ffi::ProcessSerialNumber,
  ) -> crate::Result<()> {
    let result = unsafe {
      #[allow(clippy::cast_possible_wrap)]
      ffi::_SLPSSetFrontProcessWithOptions(
        psn,
        self.id.0 as i32,
        ffi::CPS_USER_GENERATED,
      )
    };

    if result != CGError::Success {
      return Err(crate::Error::Platform(
        "Failed to set front process.".to_string(),
      ));
    }

    Ok(())
  }

  fn set_key_window(
    &self,
    psn: &ffi::ProcessSerialNumber,
  ) -> crate::Result<()> {
    // Ref: https://github.com/Hammerspoon/hammerspoon/issues/370#issuecomment-545545468
    let window_id = self.id.0.to_ne_bytes();
    let mut event1 = [0; 0x100];
    event1[0x04] = 0xf8;
    event1[0x08] = 0x01;
    event1[0x3a] = 0x10;
    event1[0x3c..(0x3c + window_id.len())].copy_from_slice(&window_id);
    event1[0x20..(0x20 + 0x10)].fill(0xff);

    let mut event2 = event1;
    event2[0x08] = 0x02;

    for event in [event1, event2] {
      let result =
        unsafe { ffi::SLPSPostEventRecordTo(psn, event.as_ptr().cast()) };

      if result != CGError::Success {
        return Err(crate::Error::Platform(
          "Failed to set key window.".to_string(),
        ));
      }
    }

    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn present_window_is_valid() {
    assert!(is_valid_window_server_presence(
      WindowServerPresence::Present,
      false,
    ));
  }

  #[test]
  fn missing_window_is_invalid_during_active_session() {
    assert!(!is_valid_window_server_presence(
      WindowServerPresence::Missing,
      false,
    ));
  }

  #[test]
  fn owner_mismatch_is_invalid_during_active_session() {
    assert!(!is_valid_window_server_presence(
      WindowServerPresence::OwnerMismatch,
      false,
    ));
  }

  #[test]
  fn unavailable_window_server_is_kept() {
    assert!(is_valid_window_server_presence(
      WindowServerPresence::Unavailable,
      false,
    ));
  }

  #[test]
  fn session_transition_keeps_missing_windows() {
    assert!(is_valid_window_server_presence(
      WindowServerPresence::Missing,
      true,
    ));
    assert!(is_valid_window_server_presence(
      WindowServerPresence::OwnerMismatch,
      true,
    ));
  }

  #[test]
  fn matching_window_server_record_is_present() {
    let window_id = WindowId(42);
    let presence = classify_window_server_presence(
      [WindowServerWindow {
        id: window_id,
        owner_pid: 123,
      }],
      window_id,
      123,
    );

    assert_eq!(presence, WindowServerPresence::Present);
  }

  #[test]
  fn mismatched_window_server_record_is_owner_mismatch() {
    let window_id = WindowId(42);
    let presence = classify_window_server_presence(
      [WindowServerWindow {
        id: window_id,
        owner_pid: 456,
      }],
      window_id,
      123,
    );

    assert_eq!(presence, WindowServerPresence::OwnerMismatch);
  }

  #[test]
  fn including_window_probe_keeps_hidden_and_off_screen_windows_eligible()
  {
    let (options, _) = window_server_query_arguments(WindowId(42), false);

    assert!(options.contains(CGWindowListOption::OptionIncludingWindow));
    assert!(!options.contains(CGWindowListOption::OptionOnScreenOnly));
  }

  /// All-window queries require a null relative ID, not the candidate ID.
  #[test]
  fn lifetime_and_managed_queries_use_the_required_relative_window_ids() {
    let candidate_id = WindowId(42);
    assert_eq!(
      window_server_query_arguments(candidate_id, true),
      (CGWindowListOption::OptionAll, 0),
    );
    assert_eq!(
      window_server_query_arguments(candidate_id, false),
      (CGWindowListOption::OptionIncludingWindow, candidate_id.0),
    );
  }
}

impl From<NativeWindow> for crate::NativeWindow {
  fn from(window: NativeWindow) -> Self {
    crate::NativeWindow { inner: window }
  }
}

/// Implements [`Dispatcher::visible_windows`].
pub(crate) fn visible_windows(
  dispatcher: &Dispatcher,
) -> crate::Result<Vec<crate::NativeWindow>> {
  Ok(
    platform_impl::all_applications(dispatcher)?
      .iter()
      .filter_map(|app| app.windows().ok())
      .flat_map(std::iter::IntoIterator::into_iter)
      .collect(),
  )
}

/// Implements [`Dispatcher::window_by_id`].
pub(crate) fn window_by_id(
  id: WindowId,
  dispatcher: &Dispatcher,
) -> crate::Result<Option<crate::NativeWindow>> {
  // TODO: The performance of this is terrible. A better solution would be
  // to have a cache of window ID <-> `NativeWindow` instances.
  for app in platform_impl::all_applications(dispatcher)? {
    if let Ok(windows) = app.windows() {
      if let Some(win) = windows.into_iter().find(|w| w.id() == id) {
        return Ok(Some(win));
      }
    }
  }

  Ok(None)
}

/// Implements [`Dispatcher::window_from_point`].
pub(crate) fn window_from_point(
  point: &Point,
  dispatcher: &Dispatcher,
) -> crate::Result<Option<crate::NativeWindow>> {
  // Get the top-most window ID at the given point.
  let window_id = dispatcher.dispatch_sync(|| {
    let cg_point = CGPoint {
      x: f64::from(point.x),
      y: f64::from(point.y),
    };

    let window_id = unsafe {
      NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(
        cg_point,
        // 0 for all windows.
        0,
        MainThreadMarker::new_unchecked(),
      )
    };

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    WindowId(window_id as u32)
  })?;

  // No window found at the given point.
  if window_id.0 == 0 {
    return Ok(None);
  }

  window_by_id(window_id, dispatcher)
    .map_err(|_| crate::Error::WindowNotFound)
}

/// Implements [`Dispatcher::focused_window`].
pub(crate) fn focused_window(
  dispatcher: &Dispatcher,
) -> crate::Result<crate::NativeWindow> {
  dispatcher
    .dispatch_sync(|| {
      // Get the frontmost (active) application.
      let frontmost_app = NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map(|app| Application::new(app, dispatcher.clone()));

      // Get the focused window of the frontmost application.
      frontmost_app.and_then(|app| app.focused_window().ok().flatten())
    })?
    .ok_or(crate::Error::WindowNotFound)
}

/// Implements [`Dispatcher::reset_focus`].
// TODO: Move this to a better-suited module.
pub(crate) fn reset_focus(dispatcher: &Dispatcher) -> crate::Result<()> {
  let Some(application) = platform_impl::application_for_bundle_id(
    "com.apple.finder",
    dispatcher,
  )?
  else {
    return Err(crate::Error::Platform(
      "Failed to get desktop application.".to_string(),
    ));
  };

  let success = application.ns_app.activateWithOptions(
    NSApplicationActivationOptions::ActivateAllWindows,
  );

  if !success {
    return Err(crate::Error::Platform(
      "Failed to activate desktop application.".to_string(),
    ));
  }

  Ok(())
}
