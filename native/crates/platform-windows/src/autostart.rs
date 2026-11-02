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
//!
//! ## When the service is installed (task W4)
//!
//! The SYSTEM service launches the host into every session it sees, so with
//! the service present the Run entry would start a SECOND host at each
//! sign-in. The service therefore owns sign-in whenever
//! [`elevation::service::ServiceStatus::owns_sign_in`] says so, and then:
//! no Run entry is created, an existing one is removed (at startup and when
//! the setting is saved), and saving the setting still succeeds (the UI sends
//! it with every save; refusing would block unrelated settings). The saved
//! value is kept, so the Run entry comes back by itself if the service is
//! removed. A query failure counts as "no service": the Run key then behaves
//! as it always did, which is never worse than before W4.
//!
//! ## References
//!
//! * Run and RunOnce registry keys:
//!   <https://learn.microsoft.com/windows/win32/setupapi/run-and-runonce-registry-keys>

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
pub use platform_api::AUTOSTART_FLAG;

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
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
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
/// worse than one that says why. With the service owning sign-in, "on" removes
/// any entry instead of writing one (module docs).
pub fn set_enabled(on: bool) -> Result<(), String> {
    if on && service_owns_sign_in() {
        tracing::info!(
            "start at sign-in: the ShareCtrlScreen service starts the host at sign-in; \
             no per-user Run entry is written"
        );
        return remove_entry();
    }
    if on {
        write_entry()
    } else {
        remove_entry()
    }
}

/// Delete our Run value. Already absent is success.
fn remove_entry() -> Result<(), String> {
    let Some(key) = open_run_key(KEY_SET_VALUE) else {
        return Err("cannot open the Run registry key".into());
    };
    // SAFETY: valid key handle; closed on both paths.
    let rc = unsafe {
        let rc = RegDeleteValueW(key, VALUE_NAME);
        let _ = RegCloseKey(key);
        rc
    };
    match rc {
        r if r == ERROR_SUCCESS => Ok(()),
        r if r == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND => Ok(()),
        r => Err(format!(
            "could not remove the startup entry (error {})",
            r.0
        )),
    }
}

/// Write our Run value with [`desired_command`].
fn write_entry() -> Result<(), String> {
    let cmd = desired_command().ok_or("cannot determine this program's path")?;
    write_value(&cmd)?;
    tracing::info!("autostart registered: {cmd}");
    Ok(())
}

fn write_value(cmd: &str) -> Result<(), String> {
    let Some(key) = open_run_key(KEY_SET_VALUE) else {
        return Err("cannot open the Run registry key".into());
    };
    let wide = to_utf16(cmd);
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
        Ok(())
    } else {
        Err(format!(
            "could not write the startup entry (error {})",
            rc.0
        ))
    }
}

/// Whether the SYSTEM service, not the Run key, starts the host at sign-in.
fn service_owns_sign_in() -> bool {
    match elevation::service::query() {
        Ok(status) => status.owns_sign_in(),
        Err(e) => {
            tracing::warn!("service state unknown ({e}); treating it as not installed");
            false
        }
    }
}

/// What [`reconcile`] does to the Run entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunEntry {
    Keep,
    Write,
    Remove,
}

/// The reconcile decision, pure so it can be tested without the registry or
/// the SCM. `current` is the registered command, `desired` the one this
/// executable would register.
fn plan(
    want: bool,
    service_owns_sign_in: bool,
    current: Option<&str>,
    desired: Option<&str>,
) -> RunEntry {
    match (service_owns_sign_in, want, current) {
        // The service starts the host; any Run entry would be a second host.
        (true, _, Some(_)) => RunEntry::Remove,
        (true, _, None) => RunEntry::Keep,
        // A stale path fails silently at logon: re-point it.
        (false, true, Some(existing)) if Some(existing) != desired => RunEntry::Write,
        (false, true, Some(_)) => RunEntry::Keep,
        (false, true, None) => RunEntry::Write,
        // Setting is off but an entry lingers (e.g. edited config by hand).
        (false, false, Some(_)) => RunEntry::Remove,
        (false, false, None) => RunEntry::Keep,
    }
}

/// Make the Run entry match the setting and the service (startup).
///
/// The recorded path goes stale whenever the app is updated, reinstalled or run
/// from a different build directory, and a stale Run entry fails silently at
/// logon — the user just finds the app never started. Called at startup so the
/// setting keeps meaning what it says.
pub fn reconcile(should_be_enabled: bool) {
    let current = current_command();
    let service = service_owns_sign_in();
    let desired = desired_command();
    match plan(
        should_be_enabled,
        service,
        current.as_deref(),
        desired.as_deref(),
    ) {
        RunEntry::Keep => {}
        RunEntry::Write => {
            if current.is_some() {
                tracing::info!("autostart path changed — re-registering");
            }
            if let Err(e) = write_entry() {
                tracing::warn!("autostart registration failed: {e}");
            }
        }
        RunEntry::Remove => {
            if service {
                tracing::info!(
                    "removing the per-user Run entry: the ShareCtrlScreen service starts \
                     the host at sign-in"
                );
            }
            if let Err(e) = remove_entry() {
                tracing::warn!("autostart removal failed: {e}");
            }
        }
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
    fn write_then_remove_roundtrips() {
        // Touches the real per-user Run key, then restores the exact prior value.
        let was = current_command();
        if write_entry().is_err() {
            return; // locked-down registry — not a test failure
        }
        assert!(is_enabled());
        assert_eq!(current_command(), desired_command());
        remove_entry().expect("removal should succeed");
        assert!(!is_enabled());
        // Removing something already absent must be a no-op, not an error.
        remove_entry().expect("second removal is a no-op");
        if let Some(original) = was {
            write_value(&original).expect("restore the original entry");
        }
    }

    const OLD: Option<&str> = Some(r#""C:\old\sharectrl.exe" --autostart"#);
    const NEW: Option<&str> =
        Some(r#""C:\Program Files\ShareCtrlScreen\sharectrl.exe" --autostart"#);

    #[test]
    fn with_the_service_any_entry_goes_and_none_is_written() {
        for want in [true, false] {
            assert_eq!(plan(want, true, OLD, NEW), RunEntry::Remove, "want={want}");
            assert_eq!(plan(want, true, NEW, NEW), RunEntry::Remove, "want={want}");
            assert_eq!(plan(want, true, None, NEW), RunEntry::Keep, "want={want}");
        }
    }

    #[test]
    fn without_the_service_the_entry_follows_the_setting() {
        assert_eq!(plan(true, false, None, NEW), RunEntry::Write);
        assert_eq!(plan(true, false, OLD, NEW), RunEntry::Write);
        assert_eq!(plan(true, false, NEW, NEW), RunEntry::Keep);
        assert_eq!(plan(false, false, OLD, NEW), RunEntry::Remove);
        assert_eq!(plan(false, false, None, NEW), RunEntry::Keep);
    }
}
