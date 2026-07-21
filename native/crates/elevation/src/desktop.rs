//! Input-desktop follower (Plan 04 §8b). A SYSTEM-context capture/inject helper
//! thread must re-attach to whichever desktop currently has input focus
//! (Default ↔ Winlogon/secure ↔ screen-saver) before injecting, so the secure
//! desktop / UAC prompt can be captured and clicked. This is exactly how the
//! UAC prompt gets driven.

use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, HDESK, UOI_NAME,
};

#[derive(Debug, thiserror::Error)]
pub enum DesktopError {
    #[error("windows: {0}")]
    Win(#[from] windows::core::Error),
}

// GENERIC_ALL-equivalent desktop access needed to set the thread desktop and
// inject input.
const DESKTOP_ALL: u32 = 0x01FF;

/// Tracks the current input desktop and re-attaches the calling thread whenever
/// it changes. Call [`follow`](Self::follow) before each injected input burst.
#[derive(Default)]
pub struct InputDesktopFollower {
    current_name: String,
    current: Option<isize>,
}

impl InputDesktopFollower {
    pub fn new() -> Self {
        Self::default()
    }

    /// If the input desktop changed since the last call, re-attach this thread to
    /// it via `SetThreadDesktop`. Returns `Ok(true)` when a switch happened (the
    /// capture side should re-initialize DDA on the new desktop).
    pub fn follow(&mut self) -> Result<bool, DesktopError> {
        // SAFETY: opens a handle to the current input desktop.
        let hdesk: HDESK = unsafe {
            OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(DESKTOP_ALL),
            )?
        };

        let name = desktop_name(hdesk).unwrap_or_default();
        if name == self.current_name && self.current.is_some() {
            // No change; drop the freshly-opened handle.
            // SAFETY: valid handle from OpenInputDesktop.
            let _ = unsafe { CloseDesktop(hdesk) };
            return Ok(false);
        }

        // Attach this thread to the new input desktop.
        // SAFETY: valid desktop handle.
        unsafe { SetThreadDesktop(hdesk)? };

        // Close the previous desktop handle, if any.
        if let Some(prev) = self.current.take() {
            // SAFETY: previously-opened desktop handle.
            let _ = unsafe { CloseDesktop(HDESK(prev as *mut _)) };
        }
        self.current = Some(hdesk.0 as isize);
        self.current_name = name;
        Ok(true)
    }

    /// The name of the desktop currently attached (e.g. `Default`, `Winlogon`).
    pub fn current(&self) -> &str {
        &self.current_name
    }
}

impl Drop for InputDesktopFollower {
    fn drop(&mut self) {
        if let Some(h) = self.current.take() {
            // SAFETY: our desktop handle.
            let _ = unsafe { CloseDesktop(HDESK(h as *mut _)) };
        }
    }
}

/// Read a desktop object's name (`GetUserObjectInformation(UOI_NAME)`).
fn desktop_name(hdesk: HDESK) -> Option<String> {
    let mut buf = [0u16; 256];
    let mut needed = 0u32;
    // SAFETY: buffer sized in bytes; out length reported in `needed`.
    let ok = unsafe {
        GetUserObjectInformationW(
            windows::Win32::Foundation::HANDLE(hdesk.0),
            UOI_NAME,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        )
    };
    if ok.is_err() {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}
