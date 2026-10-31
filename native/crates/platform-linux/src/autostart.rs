//! "Start at login" on Linux: a `sharectrl.desktop` entry in the XDG autostart
//! directory, which every mainstream desktop (GNOME, KDE, XFCE, ...) runs at
//! sign-in.
//!
//! ## Why a hand-written writer, not `auto-launch` 0.6
//!
//! `auto-launch` writes the same kind of file, but three things rule it out:
//! it quotes `Exec` with its own rules, it cannot point the entry at an
//! AppImage's `$APPIMAGE` (see below), and its `is_enabled` only checks that the
//! file exists. We need the entry *verified* against the running executable so
//! that `reconcile` can re-point a stale path, which means parsing what we wrote
//! anyway. The format is a handful of lines, so a small writer with tests is
//! less code than wrapping a crate whose behaviour we would have to work around.
//!
//! ## AppImage
//!
//! The AppImage runtime mounts the image at a temporary path and exports
//! `$APPIMAGE` as the absolute path of the `.AppImage` file itself. The
//! `current_exe()` of a running AppImage is inside that temporary mount, which
//! disappears when the app quits, so an entry pointing at it would fail at the
//! next sign-in. We register `$APPIMAGE` when it is set and `current_exe()`
//! otherwise. If the user moves the AppImage, the stored path goes stale and the
//! next start re-points it, the same as an updated binary.
//!
//! ## References
//!
//! - Desktop Entry Specification, "Exec key" and "String" value escaping:
//!   <http://specifications.freedesktop.org/desktop-entry/latest/exec-variables.html>
//!   Arguments with reserved characters are double-quoted, `"`, `` ` ``, `$` and
//!   `\` inside quotes are backslash-escaped, a literal `%` is written `%%`, and
//!   then the whole value is string-escaped, which doubles every backslash.
//! - XDG Autostart Specification: entries live in `$XDG_CONFIG_HOME/autostart`
//!   (`~/.config/autostart` by default) and are ignored when `Hidden=true`.
//!   <http://specifications.freedesktop.org/autostart/latest/>
//! - `X-GNOME-Autostart-enabled` is a GNOME convention outside the spec. We write
//!   `true` so GNOME's Startup Applications shows the entry as enabled.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use platform_api::{Autostart, AUTOSTART_FLAG};

/// File name of our entry inside the autostart directory.
const ENTRY_NAME: &str = "sharectrl.desktop";

/// The autostart entry for one user, launching one executable.
pub struct XdgAutostart {
    dir: PathBuf,
    /// Absolute path the entry must launch (UTF-8, checked in [`Self::new`]).
    exe: String,
}

impl XdgAutostart {
    /// The entry for the current user. `dirs::config_dir()` honours
    /// `$XDG_CONFIG_HOME` and falls back to `~/.config`.
    pub fn for_current_user() -> Result<Self, String> {
        let config = dirs::config_dir().ok_or("no per-user config directory")?;
        let exe = match std::env::var_os("APPIMAGE").filter(|p| !p.is_empty()) {
            Some(appimage) => PathBuf::from(appimage),
            None => std::env::current_exe()
                .map_err(|e| format!("cannot determine this program's path: {e}"))?,
        };
        Self::new(&config, exe)
    }

    /// `config_home` is `$XDG_CONFIG_HOME`; `exe` is the program the entry launches.
    pub fn new(config_home: &Path, exe: PathBuf) -> Result<Self, String> {
        // The spec requires an absolute Exec path. A relative one would resolve
        // against whatever directory the session starts in.
        if !exe.is_absolute() {
            return Err(format!(
                "the program path is not absolute: {}",
                exe.display()
            ));
        }
        // The desktop file is UTF-8. A lossy conversion would silently register
        // a path that does not exist, so refuse instead.
        let exe = exe.into_os_string().into_string().map_err(|p| {
            format!(
                "the program path is not valid UTF-8: {}",
                p.to_string_lossy()
            )
        })?;
        Ok(Self {
            dir: config_home.join("autostart"),
            exe,
        })
    }

    fn entry_path(&self) -> PathBuf {
        self.dir.join(ENTRY_NAME)
    }

    /// The `Exec` arguments the entry must carry to start us at sign-in.
    fn argv(&self) -> Vec<String> {
        vec![self.exe.clone(), AUTOSTART_FLAG.to_string()]
    }

    /// Whether the entry exists and launches this executable with the flag.
    /// Any unreadable or unparseable file counts as not registered, so that
    /// `reconcile` rewrites it.
    fn is_registered(&self) -> bool {
        fs::read_to_string(self.entry_path())
            .ok()
            .and_then(|content| parse_exec(&content))
            .is_some_and(|argv| argv == self.argv())
    }
}

impl Autostart for XdgAutostart {
    fn is_enabled(&self) -> bool {
        self.is_registered()
    }

    fn set_enabled(&self, on: bool) -> Result<(), String> {
        let entry = self.entry_path();
        if !on {
            // Removing an absent entry is what the caller asked for, so NotFound is success.
            return match fs::remove_file(&entry) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!(
                    "could not remove the autostart entry {}: {e}",
                    entry.display()
                )),
            };
        }
        fs::create_dir_all(&self.dir)
            .and_then(|()| fs::write(&entry, desktop_entry(&self.argv())))
            .map_err(|e| {
                format!(
                    "could not write the autostart entry {}: {e}",
                    entry.display()
                )
            })
    }

    fn reconcile(&self, want: bool) {
        let present = self.entry_path().exists();
        if want {
            if self.is_registered() {
                return;
            }
            match self.set_enabled(true) {
                Ok(()) if present => {
                    tracing::info!("autostart entry re-pointed at {}", self.exe);
                }
                Ok(()) => tracing::info!("autostart entry written"),
                Err(e) => tracing::warn!("autostart registration failed: {e}"),
            }
        } else if present {
            if let Err(e) = self.set_enabled(false) {
                tracing::warn!("autostart removal failed: {e}");
            }
        }
    }
}

/// The complete desktop file that registers `argv` as the login command.
fn desktop_entry(argv: &[String]) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=ShareCtrlScreen\n\
         Exec={}\n\
         Terminal=false\n\
         Hidden=false\n\
         X-GNOME-Autostart-enabled=true\n",
        exec_value(argv)
    )
}

/// Characters the Exec spec says must be quoted when they appear in an argument.
fn is_reserved(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t'
            | '\n'
            | '"'
            | '\''
            | '\\'
            | '>'
            | '<'
            | '~'
            | '|'
            | '&'
            | ';'
            | '$'
            | '*'
            | '?'
            | '#'
            | '('
            | ')'
            | '`'
    )
}

/// The value of an `Exec` key for `argv`. Quoting is applied first, then the
/// Desktop Entry string escaping, as the spec orders them.
fn exec_value(argv: &[String]) -> String {
    let quoted: Vec<String> = argv.iter().map(|arg| quote_arg(arg)).collect();
    escape_string(&quoted.join(" "))
}

fn quote_arg(arg: &str) -> String {
    let needs_quotes = arg.is_empty() || arg.chars().any(is_reserved);
    let mut out = String::with_capacity(arg.len() + 2);
    if needs_quotes {
        out.push('"');
    }
    for c in arg.chars() {
        match c {
            // Inside quotes these four must be backslash-escaped. They are all
            // reserved, so an argument containing them is always quoted.
            '"' | '`' | '$' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            // `%` starts a field code in Exec; `%%` is the literal percent sign.
            '%' => out.push_str("%%"),
            _ => out.push(c),
        }
    }
    if needs_quotes {
        out.push('"');
    }
    out
}

/// Desktop Entry string escaping for a value. Backslash must be escaped, and
/// newline, tab and carriage return have no literal form in a desktop file.
fn escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

/// The `Exec` arguments recorded in a desktop file, or `None` if the file has no
/// `Exec` key in its `[Desktop Entry]` group or the value is malformed.
fn parse_exec(content: &str) -> Option<Vec<String>> {
    let mut in_entry = false;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        if let Some(value) = line.strip_prefix("Exec=") {
            return split_exec(&unescape_string(value)?);
        }
    }
    None
}

/// Undoes [`escape_string`]. Unknown escapes are rejected rather than guessed.
fn unescape_string(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            's' => out.push(' '),
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            '\\' => out.push('\\'),
            _ => return None,
        }
    }
    Some(out)
}

/// Undoes the argument quoting of an Exec value. An unterminated quote, an
/// unknown escape inside quotes, or a lone `%` (a field code we would not have
/// written) yields `None`, so the entry is treated as stale and rewritten.
fn split_exec(s: &str) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut has_arg = false;
    let mut in_quotes = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (c, in_quotes) {
            ('"', _) => {
                in_quotes = !in_quotes;
                has_arg = true;
            }
            ('\\', true) => {
                match chars.next()? {
                    e @ ('"' | '`' | '$' | '\\') => current.push(e),
                    _ => return None,
                }
                has_arg = true;
            }
            ('%', _) => {
                if chars.next()? != '%' {
                    return None;
                }
                current.push('%');
                has_arg = true;
            }
            (c, false) if c.is_whitespace() => {
                if has_arg {
                    args.push(std::mem::take(&mut current));
                    has_arg = false;
                }
            }
            (c, _) => {
                current.push(c);
                has_arg = true;
            }
        }
    }
    if in_quotes {
        return None;
    }
    if has_arg {
        args.push(current);
    }
    Some(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory standing in for `$XDG_CONFIG_HOME`, removed on drop.
    struct TempConfigHome(PathBuf);

    impl TempConfigHome {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let dir =
                std::env::temp_dir().join(format!("sct-l10-{tag}-{}-{nanos}", std::process::id()));
            fs::create_dir_all(&dir).expect("create scratch config home");
            Self(dir)
        }

        fn entry(&self) -> PathBuf {
            self.0.join("autostart").join(ENTRY_NAME)
        }
    }

    impl Drop for TempConfigHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn autostart_for(home: &TempConfigHome, exe: &str) -> XdgAutostart {
        XdgAutostart::new(&home.0, PathBuf::from(exe)).expect("absolute utf-8 path")
    }

    fn argv(exe: &str) -> Vec<String> {
        vec![exe.to_string(), AUTOSTART_FLAG.to_string()]
    }

    #[test]
    fn plain_path_writes_the_expected_exec_line() {
        let entry = desktop_entry(&argv("/opt/sharectrl/bin/sharectrl"));
        assert!(entry.contains("\nExec=/opt/sharectrl/bin/sharectrl --autostart\n"));
        assert!(entry.contains("\nType=Application\n"));
        assert!(entry.contains("\nTerminal=false\n"));
        assert!(entry.contains("\nHidden=false\n"));
        assert!(entry.contains("\nX-GNOME-Autostart-enabled=true\n"));
    }

    #[test]
    fn spaces_and_quotes_follow_both_escaping_layers() {
        // Quoting gives `"/opt/My App/sc \"x\""`; string escaping then doubles
        // each backslash, so the file holds `\\"` for every quote.
        let entry = desktop_entry(&argv("/opt/My App/sc \"x\""));
        assert!(entry.contains(r#"Exec="/opt/My App/sc \\"x\\"" --autostart"#));
    }

    #[test]
    fn percent_is_written_as_a_literal() {
        let entry = desktop_entry(&argv("/srv/100%/sharectrl"));
        assert!(entry.contains("Exec=/srv/100%%/sharectrl --autostart"));
    }

    #[test]
    fn special_paths_round_trip_through_the_file() {
        let paths = [
            "/opt/My Apps/sharectrl",
            "/home/a\"b/sharectrl",
            "/home/$HOME/`x`/sharectrl",
            "/srv/100%/sharectrl",
            "/home/back\\slash/sharectrl",
            "/tmp/tab\there/sharectrl",
            "/tmp/new\nline/sharectrl",
            "/home/it's/sharectrl",
            "/home/(x)/s;h&&/sharectrl",
            "/home/ünï/sharectrl",
        ];
        for path in paths {
            let entry = desktop_entry(&argv(path));
            assert_eq!(parse_exec(&entry), Some(argv(path)), "path {path:?}");
        }
    }

    #[test]
    fn malformed_exec_values_are_rejected() {
        assert_eq!(split_exec("\"unterminated"), None);
        assert_eq!(split_exec("\"\\q\""), None);
        assert_eq!(split_exec("/bin/app %U"), None);
        assert_eq!(unescape_string("trailing\\"), None);
        assert_eq!(unescape_string("bad\\x"), None);
    }

    #[test]
    fn exec_is_read_only_from_the_desktop_entry_group() {
        let content = "[Other]\nExec=/wrong\n[Desktop Entry]\nExec=/right --autostart\n";
        assert_eq!(parse_exec(content), Some(argv("/right")));
        assert_eq!(parse_exec("[Desktop Entry]\nExec[de]=/x\n"), None);
    }

    #[test]
    fn enable_then_disable_round_trips() {
        let home = TempConfigHome::new("roundtrip");
        let autostart = autostart_for(&home, "/opt/sharectrl/bin/sharectrl");
        assert!(!autostart.is_enabled());

        autostart.set_enabled(true).expect("write entry");
        assert!(autostart.is_enabled());
        let written = fs::read_to_string(home.entry()).expect("entry exists");
        assert!(written.contains("Exec=/opt/sharectrl/bin/sharectrl --autostart"));

        autostart.set_enabled(false).expect("remove entry");
        assert!(!autostart.is_enabled());
        assert!(!home.entry().exists());
        // Disabling an absent entry is success.
        autostart
            .set_enabled(false)
            .expect("second removal is a no-op");
    }

    #[test]
    fn a_stale_path_is_detected_and_repointed() {
        let home = TempConfigHome::new("stale");
        autostart_for(&home, "/old/place/sharectrl")
            .set_enabled(true)
            .expect("write entry");

        let moved = autostart_for(&home, "/new/place/sharectrl");
        assert!(!moved.is_enabled());
        moved.reconcile(true);
        assert!(moved.is_enabled());
        let written = fs::read_to_string(home.entry()).expect("entry exists");
        assert!(written.contains("Exec=/new/place/sharectrl --autostart"));
        assert!(!written.contains("/old/place"));
    }

    #[test]
    fn an_unparseable_entry_counts_as_stale() {
        let home = TempConfigHome::new("garbage");
        fs::create_dir_all(home.entry().parent().expect("dir")).expect("create dir");
        fs::write(home.entry(), "not a desktop file").expect("write garbage");

        let autostart = autostart_for(&home, "/opt/sharectrl/bin/sharectrl");
        assert!(!autostart.is_enabled());
        autostart.reconcile(true);
        assert!(autostart.is_enabled());
    }

    #[test]
    fn reconcile_removes_the_entry_when_not_wanted() {
        let home = TempConfigHome::new("unwant");
        let autostart = autostart_for(&home, "/opt/sharectrl/bin/sharectrl");
        autostart.set_enabled(true).expect("write entry");
        autostart.reconcile(false);
        assert!(!home.entry().exists());
        // Nothing to do when neither wanted nor present.
        autostart.reconcile(false);
    }

    #[test]
    fn a_relative_path_is_refused() {
        let home = TempConfigHome::new("refuse");
        assert!(XdgAutostart::new(&home.0, PathBuf::from("sharectrl")).is_err());
    }

    #[test]
    fn an_unwritable_location_gives_a_readable_error() {
        let home = TempConfigHome::new("unwritable");
        // A regular file where the config home should be makes the
        // `autostart` directory uncreatable on every platform.
        let blocker = home.0.join("blocker");
        fs::write(&blocker, "").expect("create blocker file");
        let autostart =
            XdgAutostart::new(&blocker, PathBuf::from("/opt/sc/sharectrl")).expect("absolute path");
        let err = autostart
            .set_enabled(true)
            .expect_err("cannot write under a file");
        assert!(
            err.starts_with("could not write the autostart entry"),
            "{err}"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn generated_entry_passes_desktop_file_validate() {
        // Runs only where the validator is installed (`desktop-file-utils`).
        let home = TempConfigHome::new("validate");
        let autostart = autostart_for(&home, "/opt/My App/bin/sharectrl");
        autostart.set_enabled(true).expect("write entry");
        let output = match std::process::Command::new("desktop-file-validate")
            .arg(home.entry())
            .output()
        {
            Ok(output) => output,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return,
            Err(e) => panic!("cannot run desktop-file-validate: {e}"),
        };
        assert!(
            output.status.success(),
            "desktop-file-validate failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
