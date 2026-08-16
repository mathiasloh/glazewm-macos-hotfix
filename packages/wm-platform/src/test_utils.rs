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

impl Dispatcher {
  /// Creates a mock `Dispatcher` for use in tests.
  ///
  /// Calling any methods on the mock is undefined behavior and may panic.
  #[must_use]
  pub fn mock() -> Self {
    Self::new(None, Arc::new(AtomicBool::new(false)))
  }
}

impl NativeWindow {
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

      platform_impl::NativeWindow::new(WindowId(0), element, application)
        .into()
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
      #[allow(invalid_value)]
      inner: unsafe { std::mem::zeroed() },
    }
  }
}
