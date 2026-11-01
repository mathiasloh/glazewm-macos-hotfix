//! Utilities for testing.
//!
//! Available via the `test_utils` Cargo feature.
use std::sync::{atomic::AtomicBool, Arc};

#[cfg(target_os = "macos")]
use objc2_app_kit::NSRunningApplication;

use crate::platform_impl;
#[cfg(target_os = "macos")]
use crate::ThreadBound;
#[cfg(target_os = "macos")]
pub use crate::WindowId;
pub use crate::{Dispatcher, Display, NativeWindow};

/// Records dispatcher operations without touching the operator desktop.
#[derive(Debug, Default)]
pub(crate) struct MockDispatcher {
  pub(crate) focus: Option<NativeWindow>,
  pub(crate) nearest_display: Option<Display>,
  pub(crate) operations: Vec<String>,
}

impl Dispatcher {
  /// Sets the display returned by native window admission without OS
  /// access.
  pub fn mock_set_nearest_display(&self, display: Display) {
    self
      .mock_focus
      .as_ref()
      .expect("Mock dispatcher.")
      .lock()
      .expect("Mock lock.")
      .nearest_display = Some(display);
  }

  /// Returns native dispatcher operations requested by production sync.
  pub fn mock_operations(&self) -> Vec<String> {
    self
      .mock_focus
      .as_ref()
      .expect("Mock dispatcher.")
      .lock()
      .expect("Mock lock.")
      .operations
      .clone()
  }
  /// Sets global focus without any Accessibility or desktop access.
  pub fn mock_set_focused_window(&self, window: Option<NativeWindow>) {
    self
      .mock_focus
      .as_ref()
      .expect("Mock dispatcher.")
      .lock()
      .expect("Mock lock.")
      .focus = window;
  }
  /// Creates a mock `Dispatcher` for use in tests.
  ///
  /// Calling any methods on the mock is undefined behavior and may panic.
  #[must_use]
  pub fn mock() -> Self {
    let mut dispatcher = Self::new(None, Arc::new(AtomicBool::new(false)));
    dispatcher.mock_focus =
      Some(Arc::new(std::sync::Mutex::new(MockDispatcher::default())));
    dispatcher
  }
}

/// Test-only native boundary data, shared by clones of one window.
#[cfg(target_os = "macos")]
#[derive(Debug)]
pub(crate) struct MockTab {
  pub(crate) owner: i32,
  pub(crate) group: Option<u32>,
  pub(crate) queried_group: Option<Option<u32>>,
  pub(crate) valid: bool,
  pub(crate) owner_windows: Vec<NativeWindow>,
  pub(crate) enumeration_failed: bool,
  pub(crate) operations: Vec<String>,
}

impl NativeWindow {
  /// Creates an isolated native boundary fixture with retained group
  /// identity.
  #[cfg(target_os = "macos")]
  pub fn mock_tab(id: WindowId, owner: i32, group: Option<u32>) -> Self {
    let mut window = Self::mock_with_id(id);
    window.inner.mock_tab =
      Some(Arc::new(std::sync::Mutex::new(MockTab {
        owner,
        group,
        queried_group: None,
        valid: true,
        owner_windows: Vec::new(),
        enumeration_failed: false,
        operations: Vec::new(),
      })));
    window
  }

  /// Changes eligibility without changing lifetime or cached group
  /// identity.
  #[cfg(target_os = "macos")]
  pub fn mock_set_valid(&self, valid: bool) {
    self
      .inner
      .mock_tab
      .as_ref()
      .expect("Mock tab.")
      .lock()
      .expect("Mock lock.")
      .valid = valid;
  }

  /// Changes the retained group identity independently of owner inventory.
  #[cfg(target_os = "macos")]
  pub fn mock_set_tab_group(&self, group: Option<u32>) {
    self
      .inner
      .mock_tab
      .as_ref()
      .expect("Mock tab.")
      .lock()
      .expect("Mock lock.")
      .group = group;
  }

  /// Sets the next group query without changing the retained identity.
  #[cfg(target_os = "macos")]
  pub fn mock_set_queried_tab_group(&self, group: Option<u32>) {
    self
      .inner
      .mock_tab
      .as_ref()
      .expect("Mock tab.")
      .lock()
      .expect("Mock lock.")
      .queried_group = Some(group);
  }

  /// Sets a fresh owner inventory independently of global focus.
  #[cfg(target_os = "macos")]
  pub fn mock_set_owner_windows(&self, windows: Vec<Self>) {
    self
      .inner
      .mock_tab
      .as_ref()
      .expect("Mock tab.")
      .lock()
      .expect("Mock lock.")
      .owner_windows = windows;
  }

  /// Makes the owner enumeration fail without fabricating a final close
  /// signal.
  #[cfg(target_os = "macos")]
  pub fn mock_fail_owner_enumeration(&self) {
    self
      .inner
      .mock_tab
      .as_ref()
      .expect("Mock tab.")
      .lock()
      .expect("Mock lock.")
      .enumeration_failed = true;
  }

  /// Returns operations performed through the real native sync boundary.
  #[cfg(target_os = "macos")]
  pub fn mock_operations(&self) -> Vec<String> {
    self
      .inner
      .mock_tab
      .as_ref()
      .expect("Mock tab.")
      .lock()
      .expect("Mock lock.")
      .operations
      .clone()
  }
  /// Creates a mock with a distinct native ID, without querying a window.
  #[cfg(target_os = "macos")]
  #[must_use]
  pub fn mock_with_id(id: WindowId) -> Self {
    let mut window = Self::mock();
    window.inner.id = id;
    window
  }

  /// Creates a mock `NativeWindow` for use in tests.
  ///
  /// Calling any methods on the mock is undefined behavior and may panic.
  #[must_use]
  pub fn mock() -> Self {
    #[cfg(target_os = "windows")]
    {
      platform_impl::NativeWindow::new(0).into()
    }
    #[cfg(target_os = "macos")]
    {
      let dispatcher = Dispatcher::mock();
      let application = platform_impl::Application::new(
        NSRunningApplication::currentApplication(),
        dispatcher.clone(),
      );
      let element = ThreadBound::new(
        // SAFETY: `AXUIElement::new_system_wide` returns a retained
        // system accessibility element suitable for a test
        // fixture.
        unsafe { platform_impl::AXUIElement::new_system_wide() },
        dispatcher,
      );

      let mut window = platform_impl::NativeWindow::new(
        WindowId(0),
        element,
        application,
      );
      window.mock_tab = Some(Arc::new(std::sync::Mutex::new(MockTab {
        owner: 0,
        group: None,
        queried_group: None,
        valid: true,
        owner_windows: Vec::new(),
        enumeration_failed: false,
        operations: Vec::new(),
      })));
      window.into()
    }
  }
}

impl Display {
  /// Creates a mock `Display` for use in tests.
  ///
  /// Calling any methods on the mock is undefined behavior and may panic.
  #[must_use]
  pub fn mock() -> Self {
    Self {
      #[cfg(target_os = "windows")]
      inner: platform_impl::Display::new(0),
      #[cfg(target_os = "macos")]
      inner: platform_impl::Display::mock(),
    }
  }
}
