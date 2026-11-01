//! "Start at login" via a per-user LaunchAgent.
//!
//! The registration is one file, `~/Library/LaunchAgents/dev.netameta.sharectrl.plist`,
//! with `ProgramArguments` = [this executable, `--autostart`] and `RunAtLoad`
//! true. launchd reads every plist in that directory when the user signs in and
//! starts the job then. That is the same semantics as the Windows Run key
//! (`platform-windows/src/autostart.rs`): at sign-in, as the signed-in user, and
//! removable by the user without this app.
//!
//! ## Why the plist is written by hand (not `auto-launch` 0.6)
//! The plist is a dozen lines of fixed XML and one value needs reading back, so
//! a writer and a parser in this file are smaller than an extra dependency to
//! audit. The specific gaps in `auto-launch` 0.6 (docs.rs) are: its `app_path`
//! must be an existing `.app` bundle, while we register the executable inside
//! the bundle; it exposes no stale-path check, and `is_enabled` is existence
//! only; and its plist label is derived from the app name, not fixed to
//! `dev.netameta.sharectrl`. Owning the file keeps the label, the path check and
//! the rewrite-on-stale behaviour under our control.
//!
//! ## Why launchctl is not called
//! The plist alone is the registration, so there is no `launchctl` call on
//! enable or disable. Both obvious calls are wrong here:
//! - `launchctl bootstrap` loads the job and, because `RunAtLoad` is set,
//!   starts it immediately (launchd.plist(5): "launched once at the time the
//!   job is loaded"; launchctl(1) `load`: non-on-demand jobs "will be started as
//!   soon as possible"). Enabling from the running app would start a second copy
//!   of it, and the app has no single-instance guard.
//! - `launchctl bootout` on a loaded job terminates the process it launched.
//!   When the user disables the toggle inside an instance that sign-in started,
//!   that instance is this very process.
//!
//! Leaving the job loaded after a disable is harmless: launchd reads the plist
//! only at load, and with the file gone the job does not come back at the next
//! sign-in. The legacy `launchctl load -w` / `unload -w` fallback is therefore
//! not needed either.
//!
//! ## Stale paths and bundles
//! The recorded executable goes stale whenever the app is moved or updated, and
//! a stale plist fails silently at sign-in. [`Autostart::reconcile`] runs at
//! every start and re-points the entry. The registered program is the bundle's
//! own executable (`ShareCtrlScreen.app/Contents/MacOS/...`), which is what
//! `current_exe` reports. ProgramArguments is an array, so a path with spaces needs
//! no quoting. A copy run from a quarantined download is App-Translocated: macOS
//! runs it from a random read-only path under `AppTranslocation/` that vanishes on
//! quit, so registering it would be a dead entry. Enabling refuses with a message
//! that asks the user to move the app to Applications first.
//!
//! ## macOS 13+ "Allow in the Background"
//! From macOS 13 the first LaunchAgent that a user-installed app registers
//! produces a notification, and the entry appears under System Settings → General
//! → Login Items & Extensions → "Allow in the Background". If the user has switched
//! it off there, launchd skips the job at sign-in even though this file exists,
//! and `is_enabled` still reports true. Only the user can change that switch;
//! the Settings UI should mention it when the toggle is on.
//!
//! ## Where the tests run
//! The whole crate is macOS-only (`#![cfg(target_os = "macos")]` in `lib.rs`), like
//! the other lane-M modules, so the plist and stale-path tests in this file run on
//! the macOS CI runner (`cargo test --workspace` on `macos-latest`), against a
//! temporary home directory. They do not run on Windows or Linux. The real-home
//! round trip is `#[ignore = "needs a Mac"]` and is exercised by the device
//! checklist in `plans/checklists/M9.md`.
//!
//! ## References
//! - launchd.plist(5), Apple man page: `Label`, `ProgramArguments`, `RunAtLoad`
//!   (https://keith.github.io/xcode-man-pages/launchd.plist.5.html).
//! - launchctl(1), Apple man page: `bootstrap`/`bootout`, `load`/`unload`
//!   (https://keith.github.io/xcode-man-pages/launchctl.1.html).
//! - Apple, "SMAppService" (macOS 13+): the modern registration API. It needs the
//!   plist inside the bundle at `Contents/Library/LaunchAgents`, which conflicts
//!   with the per-user location chosen in `plans/00-DECISIONS.md`; kept for later.
//!   (https://developer.apple.com/documentation/servicemanagement/smappservice)
//! - auto-launch 0.6.0 (https://docs.rs/auto-launch/0.6.0/auto_launch/), evaluated
//!   and not used, see above.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use platform_api::{Autostart, AUTOSTART_FLAG};

/// launchd job label and plist file stem. Must be unique per user.
pub const LABEL: &str = "dev.netameta.sharectrl";

/// Shown when the app runs from a translocated copy (see module docs).
const TRANSLOCATED: &str = "ShareCtrlScreen is running from a temporary copy macOS made for \
     a downloaded app. Move it to Applications, open it from there, then turn this on again.";

/// The per-user LaunchAgent that starts this app at sign-in.
pub struct LaunchAgent {
    plist: PathBuf,
    exe: PathBuf,
}

impl LaunchAgent {
    /// An agent for `exe` whose plist lives under `home`. Tests pass a temporary
    /// home; the app passes the real one via [`LaunchAgent::for_current_user`].
    pub fn new(home: &Path, exe: PathBuf) -> Self {
        Self {
            plist: home
                .join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist")),
            exe,
        }
    }

    /// The agent for the logged-in user and this running executable.
    pub fn for_current_user() -> Result<Self, String> {
        let home = dirs::home_dir().ok_or("cannot find your home folder")?;
        let exe = std::env::current_exe()
            .map_err(|e| format!("cannot determine this program's path: {e}"))?;
        Ok(Self::new(&home, exe))
    }

    /// The executable path as the plist will record it. A non-UTF-8 path cannot
    /// be written as XML text, so it is refused rather than mangled.
    fn exe_str(&self) -> Option<&str> {
        self.exe.to_str()
    }

    fn recorded_program(&self) -> Option<String> {
        let xml = fs::read_to_string(&self.plist).ok()?;
        parse_program(&xml)
    }
}

impl Autostart for LaunchAgent {
    fn is_enabled(&self) -> bool {
        match (self.recorded_program(), self.exe_str()) {
            (Some(recorded), Some(exe)) => recorded == exe,
            _ => false,
        }
    }

    fn set_enabled(&self, on: bool) -> Result<(), String> {
        if !on {
            return match fs::remove_file(&self.plist) {
                Ok(()) => {
                    tracing::info!("start at sign-in removed");
                    Ok(())
                }
                // Already absent is success, not failure.
                Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!(
                    "could not remove the start-at-login entry ({}): {e}",
                    self.plist.display()
                )),
            };
        }

        if is_translocated(&self.exe) {
            return Err(TRANSLOCATED.to_string());
        }
        let exe = self
            .exe_str()
            .ok_or("this program's path is not valid UTF-8, so it cannot be registered")?;
        let xml = plist_xml(exe);
        write_atomically(&self.plist, &xml).map_err(|e| {
            format!(
                "could not save the start-at-login entry ({}): {e}",
                self.plist.display()
            )
        })?;
        tracing::info!("start at sign-in registered: {exe}");
        Ok(())
    }

    fn reconcile(&self, want: bool) {
        if want {
            // Correct already, or re-pointed below. A plist that is present but
            // names another executable is the stale case the trait asks us to fix.
            if self.is_enabled() {
                return;
            }
            if self.plist.exists() {
                tracing::info!("start-at-login path is stale — re-pointing it");
            }
            if let Err(e) = self.set_enabled(true) {
                tracing::warn!("start at sign-in: {e}");
            }
        } else if self.plist.exists() {
            if let Err(e) = self.set_enabled(false) {
                tracing::warn!("start at sign-in: {e}");
            }
        }
    }
}

/// Whether `exe` is a copy macOS App-Translocated out of a quarantined download.
fn is_translocated(exe: &Path) -> bool {
    exe.components()
        .any(|c| c.as_os_str() == "AppTranslocation")
}

/// The plist that launchd loads at sign-in.
fn plist_xml(exe: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>{flag}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
</dict>
</plist>
"#,
        label = xml_escape(LABEL),
        exe = xml_escape(exe),
        flag = xml_escape(AUTOSTART_FLAG),
    )
}

/// The first `ProgramArguments` entry of a plist written by [`plist_xml`], or
/// `None` when the text does not have that shape. Only the exact layout we write
/// is accepted, so a hand-edited or foreign file reads as "not registered"
/// rather than as a path we guessed at.
fn parse_program(xml: &str) -> Option<String> {
    let rest = xml
        .split_once("<key>ProgramArguments</key>")?
        .1
        .trim_start();
    let rest = rest.strip_prefix("<array>")?.trim_start();
    let rest = rest.strip_prefix("<string>")?;
    let (value, _) = rest.split_once("</string>")?;
    Some(xml_unescape(value))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Write via a sibling temp file and rename, so launchd never reads a half-written
/// plist and a failed write leaves the previous entry in place.
fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique empty directory under the OS temp dir, standing in for `$HOME`.
    fn temp_home(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("sharectrl-m9-{tag}-{}-{nanos}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp home");
        dir
    }

    const APP_EXE: &str = "/Applications/ShareCtrlScreen.app/Contents/MacOS/sharectrl";
    const OTHER_EXE: &str =
        "/Users/tester/Downloads/build/ShareCtrlScreen.app/Contents/MacOS/sharectrl";

    #[test]
    fn plist_has_label_program_arguments_and_run_at_load_only() {
        let xml = plist_xml(APP_EXE);
        assert!(xml.contains(&format!("<string>{LABEL}</string>")));
        assert!(xml.contains(&format!("<string>{APP_EXE}</string>")));
        assert!(xml.contains(&format!("<string>{AUTOSTART_FLAG}</string>")));
        assert!(xml.contains("<key>RunAtLoad</key>\n\t<true/>"));
        // Not a keep-alive job: the app must be able to quit without launchd
        // restarting it.
        assert!(!xml.contains("KeepAlive"));
    }

    #[test]
    fn parse_program_round_trips_awkward_paths() {
        for path in [
            APP_EXE,
            "/Users/Ann & Bob/My <Apps>/ShareCtrlScreen.app/Contents/MacOS/sharectrl",
            "/Users/o'brien/\"quoted\"/sharectrl",
            "/Users/zoë/Apps/sharectrl",
        ] {
            assert_eq!(parse_program(&plist_xml(path)).as_deref(), Some(path));
        }
    }

    #[test]
    fn parse_program_rejects_foreign_text() {
        assert_eq!(parse_program("not a plist"), None);
        assert_eq!(
            parse_program("<dict><key>Label</key><string>x</string></dict>"),
            None
        );
        // ProgramArguments present but not an array of strings.
        assert_eq!(
            parse_program("<key>ProgramArguments</key><string>/bin/x</string>"),
            None
        );
    }

    #[test]
    fn translocated_copies_are_detected() {
        assert!(is_translocated(Path::new(
            "/private/var/folders/ab/T/AppTranslocation/0F3/d/ShareCtrlScreen.app/Contents/MacOS/sharectrl"
        )));
        assert!(!is_translocated(Path::new(APP_EXE)));
    }

    #[test]
    fn enable_then_disable_round_trips_in_temp_home() {
        let home = temp_home("roundtrip");
        let agent = LaunchAgent::new(&home, PathBuf::from(APP_EXE));
        assert!(!agent.is_enabled());

        agent.set_enabled(true).expect("enable");
        assert!(agent.is_enabled());
        assert!(home
            .join("Library/LaunchAgents/dev.netameta.sharectrl.plist")
            .exists());
        assert!(!home
            .join("Library/LaunchAgents/dev.netameta.sharectrl.plist.tmp")
            .exists());

        agent.set_enabled(false).expect("disable");
        assert!(!agent.is_enabled());
        // Removing an absent entry is a no-op, not an error.
        agent.set_enabled(false).expect("second disable is a no-op");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn stale_path_is_not_enabled_and_reconcile_repoints_it() {
        let home = temp_home("stale");
        LaunchAgent::new(&home, PathBuf::from(OTHER_EXE))
            .set_enabled(true)
            .expect("register old copy");

        let current = LaunchAgent::new(&home, PathBuf::from(APP_EXE));
        assert!(
            !current.is_enabled(),
            "entry for another executable is not ours"
        );

        current.reconcile(true);
        assert!(current.is_enabled());
        assert_eq!(current.recorded_program().as_deref(), Some(APP_EXE));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn reconcile_removes_entry_when_not_wanted() {
        let home = temp_home("unwant");
        let agent = LaunchAgent::new(&home, PathBuf::from(APP_EXE));
        agent.set_enabled(true).expect("enable");
        agent.reconcile(false);
        assert!(!agent.plist.exists());
        // And with nothing registered, reconcile(false) must not create anything.
        agent.reconcile(false);
        assert!(!agent.plist.exists());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn translocated_copy_refuses_to_register() {
        let home = temp_home("translocated");
        let translocated = "/private/var/folders/ab/T/AppTranslocation/0F3/d/ShareCtrlScreen.app/Contents/MacOS/sharectrl";
        let agent = LaunchAgent::new(&home, PathBuf::from(translocated));
        let err = agent.set_enabled(true).expect_err("must refuse");
        assert!(err.contains("Move it to Applications"));
        assert!(!agent.plist.exists());
        let _ = fs::remove_dir_all(&home);
    }

    /// Writes the real `~/Library/LaunchAgents` entry for the running test binary
    /// and removes it again. Only meaningful on a Mac with a logged-in user.
    #[test]
    #[ignore = "needs a Mac"]
    fn real_home_round_trip() {
        let agent = LaunchAgent::for_current_user().expect("home and exe");
        let was = agent.is_enabled();
        agent.set_enabled(true).expect("enable in real home");
        assert!(agent.is_enabled());
        agent.set_enabled(false).expect("disable in real home");
        assert!(!agent.is_enabled());
        if was {
            let _ = agent.set_enabled(true);
        }
    }
}
