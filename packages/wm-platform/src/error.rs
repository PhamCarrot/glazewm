#[derive(Debug, thiserror::Error)]
pub enum Error {
  #[error(transparent)]
  Io(#[from] std::io::Error),

  #[error(transparent)]
  #[cfg(target_os = "windows")]
  Windows(#[from] windows::core::Error),

  #[error("Accessibility operation failed for attribute {0} with error code: {1}")]
  #[cfg(target_os = "macos")]
  Accessibility(String, i32),

  #[error(transparent)]
  Parse(#[from] ParseError),

  #[error("Invalid pointer: {0}")]
  InvalidPointer(String),

  #[error("AXValue creation failed: {0}")]
  #[cfg(target_os = "macos")]
  AXValueCreation(String),

  #[error(transparent)]
  ChannelRecv(#[from] std::sync::mpsc::RecvTimeoutError),

  #[error("Channel receive error")]
  OneshotRecv(#[from] tokio::sync::oneshot::error::RecvError),

  #[error(transparent)]
  IntConversion(#[from] std::num::TryFromIntError),

  #[error("Channel send error")]
  ChannelSend,

  #[error("Display enumeration failed")]
  DisplayEnumerationFailed,

  #[error("Display mode not found")]
  DisplayModeNotFound,

  #[error("Primary display not found")]
  PrimaryDisplayNotFound,

  #[error("Not main thread")]
  NotMainThread,

  #[error("Display not found")]
  DisplayNotFound,

  #[error("Display device not found")]
  DisplayDeviceNotFound,

  #[error("Hardware enumeration failed")]
  HardwareEnumerationFailed,

  #[error("Window enumeration failed")]
  WindowEnumerationFailed,

  #[error("Window not found")]
  WindowNotFound,

  #[error("Thread error: {0}")]
  Thread(String),

  #[error("Window message error: {0}")]
  WindowMessage(String),

  #[error("Platform error: {0}")]
  Platform(String),

  #[error("Event loop has been stopped")]
  EventLoopStopped,

  #[error("Keybinding is empty")]
  InvalidKeybinding,
}

impl Error {
  /// Whether this error reports a Windows integrity/access boundary.
  #[cfg(target_os = "windows")]
  #[must_use]
  pub fn is_access_denied(&self) -> bool {
    const HRESULT_ACCESS_DENIED: u32 = 0x8007_0005;
    matches!(
      self,
      Self::Windows(err) if err.code().0 as u32 == HRESULT_ACCESS_DENIED
    )
  }

  /// Whether the Windows shell temporarily has no `IApplicationView` for
  /// an otherwise live HWND.
  ///
  /// `IApplicationViewCollection::GetViewForHwnd` can return
  /// `TYPE_E_ELEMENTNOTFOUND` while Explorer is still publishing a newly
  /// activated view. This is distinct from a destroyed HWND and from an
  /// integrity/access boundary: callers may retry it on a later event-loop
  /// tick, but must not discard the window's state.
  #[cfg(target_os = "windows")]
  #[must_use]
  pub fn is_application_view_unavailable(&self) -> bool {
    const TYPE_E_ELEMENTNOTFOUND: u32 = 0x8002_802B;
    matches!(
      self,
      Self::Windows(err) if err.code().0 as u32 == TYPE_E_ELEMENTNOTFOUND
    )
  }

  /// Whether this error reports that a Windows HWND ceased to exist.
  ///
  /// Checks the operation's returned native error rather than relying on a
  /// pre-call liveness probe, because destruction can race the operation.
  #[cfg(target_os = "windows")]
  #[must_use]
  pub fn is_invalid_window_handle(&self) -> bool {
    const HRESULT_INVALID_HANDLE: u32 = 0x8007_0006;
    const HRESULT_INVALID_WINDOW_HANDLE: u32 = 0x8007_0578;
    match self {
      Self::Windows(err) => matches!(
        err.code().0 as u32,
        HRESULT_INVALID_HANDLE | HRESULT_INVALID_WINDOW_HANDLE
      ),
      Self::WindowNotFound => true,
      _ => false,
    }
  }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
  #[error(
    "Invalid length value '{0}': expected a finite decimal from -100000px to \
     100000px, or from -1000% to 1000% (no unit means pixels)."
  )]
  Length(String),

  #[error("Invalid keybinding: {0}")]
  Keybinding(String),

  #[error(
    "Invalid opacity value '{0}': expected a finite value from 0.0 to 1.0 \
     (for example '0.75') or from 0% to 100% (for example '75%')."
  )]
  Opacity(String),

  #[error(
    "Invalid color '{0}': must be of format '#RRGGBB' or '#RRGGBBAA'."
  )]
  Color(String),

  #[error("Invalid delta value: {0}")]
  Delta(String),

  #[error("Invalid direction '{0}': must be one of 'left', 'right', 'up', or 'down'.")]
  Direction(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(all(test, target_os = "windows"))]
mod tests {
  use windows::core::{Error as WindowsError, HRESULT, HSTRING};

  use super::Error;

  #[test]
  fn classifies_returned_invalid_hwnd_errors() {
    for code in [0x8007_0006_u32, 0x8007_0578_u32] {
      let error = Error::Windows(WindowsError::new(
        HRESULT(code as i32),
        HSTRING::new(),
      ));
      assert!(error.is_invalid_window_handle());
    }
  }

  #[test]
  fn preserves_live_native_failures() {
    let access_denied = Error::Windows(WindowsError::new(
      HRESULT(0x8007_0005_u32 as i32),
      HSTRING::new(),
    ));
    assert!(!access_denied.is_invalid_window_handle());
    assert!(access_denied.is_access_denied());

    let generic_failure = Error::Windows(WindowsError::new(
      HRESULT(0x8000_4005_u32 as i32),
      HSTRING::new(),
    ));
    assert!(!generic_failure.is_access_denied());
  }

  #[test]
  fn classifies_transient_application_view_unavailability_separately() {
    let unavailable = Error::Windows(WindowsError::new(
      HRESULT(0x8002_802B_u32 as i32),
      HSTRING::new(),
    ));
    assert!(unavailable.is_application_view_unavailable());
    assert!(!unavailable.is_invalid_window_handle());
    assert!(!unavailable.is_access_denied());

    let access_denied = Error::Windows(WindowsError::new(
      HRESULT(0x8007_0005_u32 as i32),
      HSTRING::new(),
    ));
    assert!(!access_denied.is_application_view_unavailable());
  }
}
