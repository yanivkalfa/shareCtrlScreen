//! "Start with Windows" via the per-user Run key.
//!
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` is deliberate: it needs
//! no administrator rights, is trivially inspectable and removable by the user
//! (Task Manager → Startup lists it), and runs the app as the logged-in user —
//! which it must be, because the app owns a window and captures that user's
//! desktop. A service or an HKLM entry would run before/outside the session and
//! could not do either.
//!
//! The consequence worth knowing: this starts the app **at logon**, not at boot.
//! Reaching a machine that has restarted but not been logged into needs the
//! SYSTEM service path instead, which is a different mechanism entirely.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
};

/// Our value name under the Run key.
const VALUE_NAME: PCWSTR = w!("ShareCtrlScreen");
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");

/// Marker passed to the auto-started instance so it can come up minimized
/// instead of throwing a window in the user's face at every logon.
pub const AUTOSTART_FLAG: &str = "--autostart";

/// The command we want registered: this executable, quoted (the path routinely
/// contains spaces — `C:\Program Files\…`), plus the marker.
fn desired_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\" {AUTOSTART_FLAG}", exe.display()))
}

fn to_utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn open_run_key(access: windows::Win32::System::Registry::REG_SAM_FLAGS) -> Option<HKEY> {
    let mut key = HKEY::default();
    // SAFETY: the Run key always exists for the current user; `key` is an out-param.
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, access, &mut key) };
    (rc == ERROR_SUCCESS).then_some(key)
}

/// The currently registered command, if any.
fn current_command() -> Option<String> {
    let key = open_run_key(KEY_QUERY_VALUE)?;
    let mut kind = windows::Win32::System::Registry::REG_VALUE_TYPE::default();
    let mut len = 0u32;
    // SAFETY: first call sizes the buffer (null data pointer), second fills it.
    let out = unsafe {
        let rc = RegQueryValueExW(key, VALUE_NAME, None, Some(&mut kind), None, Some(&mut len));
        if rc != ERROR_SUCCESS || len == 0 {
            let _ = RegCloseKey(key);
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        let rc = RegQueryValueExW(
            key,
            VALUE_NAME,
            None,
            Some(&mut kind),
            Some(buf.as_mut_ptr()),
            Some(&mut len),
        );
        let _ = RegCloseKey(key);
        if rc != ERROR_SUCCESS {
            return None;
        }
        buf.truncate(len as usize);
        let wide: Vec<u16> = buf
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&c| c != 0)
            .collect();
        String::from_utf16_lossy(&wide)
    };
    Some(out)
}

/// Whether the app is registered to start with Windows.
pub fn is_enabled() -> bool {
    current_command().is_some()
}

/// Register or unregister. Returns a human-readable error for the UI on failure
/// rather than failing silently — a startup toggle that quietly does nothing is
/// worse than one that says why.
pub fn set_enabled(on: bool) -> Result<(), String> {
    if !on {
        let Some(key) = open_run_key(KEY_SET_VALUE) else {
            return Err("cannot open the Run registry key".into());
        };
        // SAFETY: valid key handle; closed on both paths.
        let rc = unsafe {
            let rc = RegDeleteValueW(key, VALUE_NAME);
            let _ = RegCloseKey(key);
            rc
        };
        // Already absent is success, not failure.
        return match rc {
            r if r == ERROR_SUCCESS => Ok(()),
            r if r == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND => Ok(()),
            r => Err(format!(
                "could not remove the startup entry (error {})",
                r.0
            )),
        };
    }

    let cmd = desired_command().ok_or("cannot determine this program's path")?;
    let Some(key) = open_run_key(KEY_SET_VALUE) else {
        return Err("cannot open the Run registry key".into());
    };
    let wide = to_utf16(&cmd);
    // REG_SZ data is the UTF-16 bytes INCLUDING the terminating NUL.
    let bytes: &[u8] =
        // SAFETY: reinterpreting a u16 slice as bytes; length is exact and the
        // borrow does not outlive `wide`.
        unsafe { std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2) };
    // SAFETY: valid key handle and sized data; closed on both paths.
    let rc = unsafe {
        let rc = RegSetValueExW(key, VALUE_NAME, None, REG_SZ, Some(bytes));
        let _ = RegCloseKey(key);
        rc
    };
    if rc == ERROR_SUCCESS {
        tracing::info!("autostart registered: {cmd}");
        Ok(())
    } else {
        Err(format!(
            "could not write the startup entry (error {})",
            rc.0
        ))
    }
}

/// Re-point the registered command at this executable if it has moved.
///
/// The recorded path goes stale whenever the app is updated, reinstalled or run
/// from a different build directory, and a stale Run entry fails silently at
/// logon — the user just finds the app never started. Called at startup so the
/// setting keeps meaning what it says.
pub fn reconcile(should_be_enabled: bool) {
    let current = current_command();
    match (should_be_enabled, current) {
        (true, Some(existing)) => {
            if Some(&existing) != desired_command().as_ref() {
                tracing::info!("autostart path changed — re-registering");
                if let Err(e) = set_enabled(true) {
                    tracing::warn!("autostart re-registration failed: {e}");
                }
            }
        }
        (true, None) => {
            if let Err(e) = set_enabled(true) {
                tracing::warn!("autostart registration failed: {e}");
            }
        }
        // Setting is off but an entry lingers (e.g. edited config by hand).
        (false, Some(_)) => {
            let _ = set_enabled(false);
        }
        (false, None) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desired_command_is_quoted_and_flagged() {
        let cmd = desired_command().expect("current_exe available");
        assert!(cmd.starts_with('"'), "path must be quoted: {cmd}");
        assert!(cmd.ends_with(AUTOSTART_FLAG), "flag must be present: {cmd}");
        // The closing quote has to come before the flag, or the shell would treat
        // the flag as part of the path.
        let close = cmd.rfind('"').expect("closing quote");
        assert!(close < cmd.len() - AUTOSTART_FLAG.len());
    }

    #[test]
    fn enable_then_disable_roundtrips() {
        // Touches the real per-user Run key, then restores the prior state.
        let was = is_enabled();
        if set_enabled(true).is_err() {
            return; // locked-down registry — not a test failure
        }
        assert!(is_enabled());
        assert_eq!(current_command(), desired_command());
        set_enabled(false).expect("removal should succeed");
        assert!(!is_enabled());
        // Removing something already absent must be a no-op, not an error.
        set_enabled(false).expect("second removal is a no-op");
        if was {
            let _ = set_enabled(true);
        }
    }
}
