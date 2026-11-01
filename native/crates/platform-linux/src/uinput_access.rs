//! One-time, opt-in setup of `/dev/uinput` access (decision #4).
//!
//! The uinput injector (a fallback for sessions without the RemoteDesktop portal's
//! libei support) needs write access to `/dev/uinput`, which is `root:root 0600` on
//! a stock system. [`install_uinput_access`] asks polkit (`pkexec`) for a single
//! authorisation and then, as root: loads the `uinput` module (now and at boot),
//! writes a udev rule giving the `input` group access, adds the invoking user to
//! that group and reloads udev. Group membership only applies to new login
//! sessions, which the outcome reports as [`UinputInstallOutcome::relogin_required`].
//!
//! The Settings panel calls this from a button; nothing here runs implicitly.

use std::fmt;
use std::io;
use std::process::Command;

/// Where the rule is installed. The `70-` prefix orders it before the
/// `73-seat-late` / `99-` rules that could override group or mode.
pub const UINPUT_RULE_PATH: &str = "/etc/udev/rules.d/70-sharectrl-uinput.rules";

/// Loads the module at boot; many distributions do not autoload `uinput`.
pub const UINPUT_MODULES_LOAD_PATH: &str = "/etc/modules-load.d/sharectrl-uinput.conf";

/// The group that owns `/dev/uinput` after the rule applies.
pub const UINPUT_GROUP: &str = "input";

/// `static_node=uinput` makes udev apply the permissions at boot even before the
/// module is loaded (udev(7), `OPTIONS`).
const UINPUT_RULE: &str = concat!(
    "# Installed by ShareCtrlScreen: lets members of the \"input\" group create\n",
    "# virtual input devices for remote control. Remove this file to revoke.\n",
    "KERNEL==\"uinput\", SUBSYSTEM==\"misc\", GROUP=\"input\", MODE=\"0660\", ",
    "OPTIONS+=\"static_node=uinput\"\n",
);

/// Runs as root under pkexec. Arguments: `$1` rule text, `$2` rule path,
/// `$3` modules-load path, `$4` group, `$5` user. Data travels as arguments, never
/// interpolated into this text.
const ROOT_SCRIPT: &str = r#"set -eu
umask 022
printf '%s' "$1" > "$2.tmp"
mv -f "$2.tmp" "$2"
printf 'uinput\n' > "$3"
getent group "$4" > /dev/null || groupadd --system "$4"
usermod --append --groups "$4" "$5"
modprobe uinput
udevadm control --reload-rules
udevadm trigger --subsystem-match=misc --sysname-match=uinput
"#;

/// Whether to actually elevate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMode {
    /// Run `pkexec` and change the system.
    Apply,
    /// Only build and return the plan; nothing is spawned or written.
    DryRun,
}

/// Exactly what an install does; returned so the UI can show it and tests can
/// assert on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UinputInstallPlan {
    pub rule_path: &'static str,
    pub rule: &'static str,
    pub user: String,
    /// The full argv, starting with `pkexec`.
    pub command: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UinputInstallOutcome {
    pub plan: UinputInstallPlan,
    /// False for a dry run.
    pub applied: bool,
    /// The new `input` group membership is picked up at the next login.
    pub relogin_required: bool,
}

#[derive(Debug)]
pub enum UinputInstallError {
    /// The invoking user could not be determined or is not a valid account name.
    UnknownUser(String),
    /// `pkexec` (polkit) is not installed.
    PkexecMissing,
    /// The user dismissed the authentication dialog.
    Cancelled,
    /// polkit refused (no agent, or not authorised).
    NotAuthorized,
    /// The privileged script failed.
    Failed {
        code: Option<i32>,
        stderr: String,
    },
    Io(io::Error),
}

impl fmt::Display for UinputInstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownUser(why) => write!(f, "cannot determine the current user: {why}"),
            Self::PkexecMissing => {
                f.write_str("pkexec is not installed; install polkit or follow the manual steps")
            }
            Self::Cancelled => f.write_str("authentication was cancelled"),
            Self::NotAuthorized => f.write_str("not authorized to change system settings"),
            Self::Failed { code, stderr } => {
                write!(f, "setup failed (exit {code:?}): {}", stderr.trim())
            }
            Self::Io(e) => write!(f, "cannot run pkexec: {e}"),
        }
    }
}

impl std::error::Error for UinputInstallError {}

/// Installs the udev rule and group membership for `/dev/uinput`.
pub fn install_uinput_access(
    mode: InstallMode,
) -> Result<UinputInstallOutcome, UinputInstallError> {
    install_with("pkexec", &current_user()?, mode)
}

fn install_with(
    pkexec: &str,
    user: &str,
    mode: InstallMode,
) -> Result<UinputInstallOutcome, UinputInstallError> {
    validate_user(user)?;
    let plan = build_plan(pkexec, user);
    if mode == InstallMode::DryRun {
        return Ok(UinputInstallOutcome {
            plan,
            applied: false,
            relogin_required: false,
        });
    }
    let out = Command::new(&plan.command[0])
        .args(&plan.command[1..])
        .output()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => UinputInstallError::PkexecMissing,
            _ => UinputInstallError::Io(e),
        })?;
    if let Some(err) = classify_exit(out.status.code(), &String::from_utf8_lossy(&out.stderr)) {
        return Err(err);
    }
    tracing::info!(
        user,
        "uinput access installed; a new login session is required"
    );
    Ok(UinputInstallOutcome {
        plan,
        applied: true,
        relogin_required: true,
    })
}

fn build_plan(pkexec: &str, user: &str) -> UinputInstallPlan {
    let command = [
        pkexec,
        "/bin/sh",
        "-c",
        ROOT_SCRIPT,
        "sharectrl-uinput",
        UINPUT_RULE,
        UINPUT_RULE_PATH,
        UINPUT_MODULES_LOAD_PATH,
        UINPUT_GROUP,
        user,
    ]
    .map(String::from)
    .to_vec();
    UinputInstallPlan {
        rule_path: UINPUT_RULE_PATH,
        rule: UINPUT_RULE,
        user: user.to_owned(),
        command,
    }
}

/// pkexec(1): 126 = the authentication dialog was dismissed, 127 = not
/// authorised or no authentication agent; any other code is the script's.
fn classify_exit(code: Option<i32>, stderr: &str) -> Option<UinputInstallError> {
    match code {
        Some(0) => None,
        Some(126) => Some(UinputInstallError::Cancelled),
        Some(127) => Some(UinputInstallError::NotAuthorized),
        code => Some(UinputInstallError::Failed {
            code,
            stderr: stderr.to_owned(),
        }),
    }
}

fn current_user() -> Result<String, UinputInstallError> {
    ["USER", "LOGNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .ok_or_else(|| UinputInstallError::UnknownUser("USER and LOGNAME are unset".into()))
}

/// Conservative useradd(8) `NAME_REGEX`. The name is passed as an argument, not
/// parsed by a shell, but a leading `-` would still read as an option to usermod.
fn validate_user(user: &str) -> Result<(), UinputInstallError> {
    let mut chars = user.chars();
    let first_ok = matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_');
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '$'));
    if first_ok && rest_ok && user.len() <= 32 {
        Ok(())
    } else {
        Err(UinputInstallError::UnknownUser(format!(
            "{user:?} is not a valid account name"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Serialises every test that spawns a process (see `run_with_fake_pkexec`).
    static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn dry_run_spawns_nothing_and_reports_the_plan() {
        // A dry run that tried to spawn "/nonexistent/pkexec" would fail.
        let out = install_with("/nonexistent/pkexec", "alice", InstallMode::DryRun).unwrap();
        assert!(!out.applied);
        assert!(!out.relogin_required);
        assert_eq!(out.plan.rule_path, UINPUT_RULE_PATH);
        assert_eq!(out.plan.user, "alice");
        assert_eq!(out.plan.command[0], "/nonexistent/pkexec");
        assert_eq!(out.plan.command.last().unwrap(), "alice");
    }

    #[test]
    fn rule_grants_the_input_group_and_the_user_is_never_in_the_script() {
        let plan = build_plan("pkexec", "alice");
        assert!(plan.rule.contains(r#"KERNEL=="uinput""#));
        assert!(plan.rule.contains(r#"GROUP="input""#));
        assert!(plan.rule.contains(r#"MODE="0660""#));
        assert!(!ROOT_SCRIPT.contains("alice"));
        assert!(!plan.rule.contains("alice"));
    }

    #[test]
    fn account_names_are_validated() {
        for ok in ["alice", "_svc", "a-b.c", "user1", "host$"] {
            assert!(validate_user(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-rf", "Alice", "a b", "a;b", "1abc", &"x".repeat(33)] {
            assert!(validate_user(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn pkexec_exit_codes_map_to_errors() {
        assert!(classify_exit(Some(0), "").is_none());
        assert!(matches!(
            classify_exit(Some(126), ""),
            Some(UinputInstallError::Cancelled)
        ));
        assert!(matches!(
            classify_exit(Some(127), ""),
            Some(UinputInstallError::NotAuthorized)
        ));
        assert!(matches!(
            classify_exit(Some(1), "boom"),
            Some(UinputInstallError::Failed { code: Some(1), .. })
        ));
        assert!(matches!(
            classify_exit(None, ""),
            Some(UinputInstallError::Failed { code: None, .. })
        ));
    }

    #[test]
    fn missing_pkexec_is_reported() {
        let _guard = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
        let err = install_with("/nonexistent/pkexec", "alice", InstallMode::Apply).unwrap_err();
        assert!(matches!(err, UinputInstallError::PkexecMissing));
    }

    /// Writes an executable stand-in for pkexec and runs an install through it.
    /// Writing and spawning happen under one lock: a fork from a concurrently
    /// running test while the script is still open for writing makes exec fail
    /// with ETXTBSY.
    fn run_with_fake_pkexec(
        tag: &str,
        body: &str,
    ) -> (
        Result<UinputInstallOutcome, UinputInstallError>,
        std::path::PathBuf,
    ) {
        let _guard = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("sct-pkexec-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pkexec");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let result = install_with(&path.to_string_lossy(), "alice", InstallMode::Apply);
        (result, dir)
    }

    #[test]
    fn apply_runs_the_command_and_reports_relogin() {
        let log = std::env::temp_dir().join(format!("sct-pkexec-argv-{}", std::process::id()));
        let (result, dir) = run_with_fake_pkexec(
            "ok",
            &format!("printf '%s\\n' \"$@\" > '{}'", log.display()),
        );
        let out = result.unwrap();
        assert!(out.applied && out.relogin_required);
        let argv = std::fs::read_to_string(&log).unwrap();
        assert!(argv.contains(UINPUT_RULE_PATH) && argv.contains("alice"));
        std::fs::remove_file(&log).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_surfaces_a_dismissed_dialog() {
        let (result, dir) = run_with_fake_pkexec("cancel", "exit 126");
        assert!(matches!(result, Err(UinputInstallError::Cancelled)));
        std::fs::remove_dir_all(&dir).ok();
    }
}
