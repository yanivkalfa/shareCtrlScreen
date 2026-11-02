//! The host's UIAccess report (W5): one startup line in `app.log` and the
//! `--print-uiaccess` diagnostic.
//!
//! UIAccess is the Windows token flag that lets injected Alt+Tab and the
//! Windows key reach the shell. The SYSTEM service sets it on the host it
//! launches (`elevation::uiaccess`); a host the user starts has none. The text
//! and exit-code mapping is pure so it is tested on every OS; only the token
//! query is Windows-only.
//!
//! ## References
//! * `GetTokenInformation` / `TokenUIAccess` —
//!   <https://learn.microsoft.com/en-us/windows/win32/api/winnt/ne-winnt-token_information_class>.
//! * Why it matters: "Windows Integrity Mechanism Design" → "UIAccess for UI
//!   automation applications" —
//!   <https://learn.microsoft.com/en-us/previous-versions/dotnet/articles/bb625963(v=msdn.10)>.

/// The command-line flag that prints the line and exits.
pub const PRINT_UIACCESS_FLAG: &str = "--print-uiaccess";

/// The reason when the flag is simply absent: on Windows the only way this
/// build gets UIAccess is the service setting it at launch.
#[cfg(any(windows, test))]
pub const NOT_FROM_SERVICE: &str = "the host was not started by the service";

/// Whether this process has UIAccess, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAccessStatus {
    /// Only ever constructed by the token query, which is Windows-only
    /// (`from_query`/`probe` under `cfg(windows)`); on other OSes `probe`
    /// always returns `NotGranted`, so the variant is live only on Windows (and
    /// in tests). `line()`/`exit_code()` still match on it everywhere.
    #[cfg_attr(not(windows), allow(dead_code))]
    Granted,
    NotGranted(String),
}

impl UiAccessStatus {
    /// Map the token query's answer: the flag set → granted; the flag absent →
    /// [`NOT_FROM_SERVICE`]; a failed query → that error, verbatim.
    #[cfg(any(windows, test))]
    pub fn from_query<E: std::fmt::Display>(query: Result<bool, E>) -> Self {
        match query {
            Ok(true) => Self::Granted,
            Ok(false) => Self::NotGranted(NOT_FROM_SERVICE.to_string()),
            Err(e) => Self::NotGranted(format!("the token query failed: {e}")),
        }
    }

    /// The exact line logged at startup and printed by `--print-uiaccess`.
    pub fn line(&self) -> String {
        match self {
            Self::Granted => "UIAccess: granted".to_string(),
            Self::NotGranted(reason) => format!("UIAccess: not granted — {reason}"),
        }
    }

    /// `--print-uiaccess` exit code: 0 when granted, 1 otherwise, so a script
    /// can test it without parsing text.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Granted => 0,
            Self::NotGranted(_) => 1,
        }
    }
}

/// Whether the command line asks for `--print-uiaccess` (program name
/// excluded).
pub fn wants_print<I: IntoIterator<Item = String>>(args: I) -> bool {
    args.into_iter().any(|a| a == PRINT_UIACCESS_FLAG)
}

/// This process's status, from its own primary token.
#[cfg(windows)]
pub fn probe() -> UiAccessStatus {
    UiAccessStatus::from_query(elevation::query_process_uiaccess())
}

/// UIAccess is a Windows token flag; other OSes have no equivalent to report,
/// so `--print-uiaccess` there says so and exits 1.
#[cfg(not(windows))]
pub fn probe() -> UiAccessStatus {
    UiAccessStatus::NotGranted("UIAccess is a Windows feature".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn granted_is_the_exact_line_and_exit_zero() {
        let s = UiAccessStatus::from_query::<String>(Ok(true));
        assert_eq!(s.line(), "UIAccess: granted");
        assert_eq!(s.exit_code(), 0);
    }

    #[test]
    fn absent_flag_means_not_started_by_the_service() {
        let s = UiAccessStatus::from_query::<String>(Ok(false));
        assert_eq!(
            s.line(),
            "UIAccess: not granted — the host was not started by the service"
        );
        assert_eq!(s.exit_code(), 1);
    }

    #[test]
    fn a_failed_query_reports_its_error() {
        let s = UiAccessStatus::from_query(Err("Access is denied. (0x80070005)"));
        assert_eq!(
            s.line(),
            "UIAccess: not granted — the token query failed: Access is denied. (0x80070005)"
        );
        assert_eq!(s.exit_code(), 1);
    }

    #[test]
    fn the_flag_is_found_anywhere_on_the_command_line() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(wants_print(args(&["--print-uiaccess"])));
        assert!(wants_print(args(&["--autostart", "--print-uiaccess"])));
        assert!(!wants_print(args(&[])));
        assert!(!wants_print(args(&["--autostart", "--print-uiaccess=1"])));
    }

    /// The probe always yields one well-formed line whose exit code agrees
    /// with it (a test process is never granted UIAccess).
    #[test]
    fn the_probe_of_a_test_process_is_not_granted() {
        let s = probe();
        assert!(s.line().starts_with("UIAccess: not granted — "), "{s:?}");
        assert_eq!(s.exit_code(), 1);
    }
}
