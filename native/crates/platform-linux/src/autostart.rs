//! "Start at login" on Linux: a `sharectrl.desktop` entry in the XDG autostart
//! directory, which every mainstream desktop (GNOME, KDE, XFCE, ...) runs at
//! sign-in.
//!
//! ## Why a hand-written writer, not `auto-launch` 0.6
//!
//! `auto-launch` writes the same kind of file, but three things rule it out:
//! it quotes `Exec` with its own rules, it cannot choose between an AppImage's
//! `$APPIMAGE` and the executable (see below), and its `is_enabled` only checks
//! that the file exists. We need the entry *verified* against the running
//! executable so that `reconcile` can re-point a stale path, which means parsing
//! what we wrote anyway. The format is a handful of lines, so a small writer with
//! tests is less code than wrapping a crate whose behaviour we would have to work
//! around.
//!
//! ## Which program is registered
//!
//! [`choose_exe`] decides. `$APPIMAGE` is inherited by every child process, so
//! on its own it proves nothing: a `.deb` build started from a terminal inside
//! another AppImage would inherit it and register the wrong program. It is used
//! only when we run from an AppImage mount (`$APPDIR` is set and the executable
//! lives under it). The mount itself is temporary and must never be registered,
//! so if the image has been deleted the entry is not written at all. Everywhere
//! else the registered program is `current_exe()`. A package upgrade makes the
//! kernel report it as `… (deleted)`, which is stripped. Every choice must be an
//! existing file, checked before anything is written.
//!
//! ## Disabled entries
//!
//! The user can switch the entry off in GNOME Startup Applications, which writes
//! `X-GNOME-Autostart-enabled=false`, or set `Hidden=true` by hand. Both mean
//! "off" to us. `is_enabled` is then false, and `reconcile` re-points a stale
//! entry without re-enabling it. Only `set_enabled(true)`, an explicit user
//! action, writes an enabled entry.
//!
//! ## `is_enabled` on Linux
//!
//! Here it means "an enabled entry for this executable exists". On Windows it
//! means that any entry exists. Nothing in the app depends on the difference
//! today.
//!
//! ## System-wide entries
//!
//! If packaging ever ships `/etc/xdg/autostart/sharectrl.desktop`, the user's file
//! takes precedence over it. Disabling would then have to write `Hidden=true`
//! instead of deleting the user file, or the system entry would start the app
//! again. No system entry ships today, so `set_enabled(false)` deletes.
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
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use platform_api::{Autostart, AUTOSTART_FLAG};

/// File name of our entry inside the autostart directory.
const ENTRY_NAME: &str = "sharectrl.desktop";

/// Suffix the kernel appends to `/proc/self/exe` once the running binary has been
/// replaced on disk (a package upgrade). It is not part of the path.
const DELETED_SUFFIX: &str = " (deleted)";

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
        let current = std::env::current_exe()
            .map_err(|e| format!("cannot determine this program's path: {e}"))?;
        let exe = choose_exe(
            env_utf8("APPIMAGE").as_deref(),
            env_utf8("APPDIR").as_deref(),
            &current,
            |p| p.is_file(),
        )?;
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

    /// The entry as it is on disk, or `None` when there is no readable file.
    fn read_state(&self) -> Option<EntryState> {
        fs::read_to_string(self.entry_path())
            .ok()
            .map(|content| parse_entry(&content))
    }

    /// Writes the entry atomically, so a crash never leaves a half-written file:
    /// a temporary file in the same directory is synced and then renamed over the
    /// entry. Rename replaces a symlinked entry with our own regular file rather
    /// than writing through the link. That is intended, since the entry is ours.
    fn write_entry(&self, disabled: bool) -> Result<(), String> {
        let entry = self.entry_path();
        // Registering a missing program would fail silently at sign-in.
        if !Path::new(&self.exe).is_file() {
            return Err(format!(
                "could not write the autostart entry {}: {} is not a file",
                entry.display(),
                self.exe
            ));
        }
        let tmp = self
            .dir
            .join(format!(".{ENTRY_NAME}.{}.tmp", std::process::id()));
        let content = desktop_entry(&self.argv(), disabled);
        let result = fs::create_dir_all(&self.dir)
            .and_then(|()| write_synced(&tmp, &content))
            .and_then(|()| fs::rename(&tmp, &entry));
        result.map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!(
                "could not write the autostart entry {}: {e}",
                entry.display()
            )
        })
    }
}

impl Autostart for XdgAutostart {
    /// True only for an enabled entry that launches this executable.
    fn is_enabled(&self) -> bool {
        self.read_state()
            .is_some_and(|state| !state.disabled && state.launches(&self.argv()))
    }

    fn set_enabled(&self, on: bool) -> Result<(), String> {
        if on {
            // An explicit request to enable is the only thing that clears a disable.
            return self.write_entry(false);
        }
        let entry = self.entry_path();
        // Removing an absent entry is what the caller asked for, so NotFound is success.
        match fs::remove_file(&entry) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!(
                "could not remove the autostart entry {}: {e}",
                entry.display()
            )),
        }
    }

    fn reconcile(&self, want: bool) {
        let present = self.entry_path().exists();
        match (want, self.read_state()) {
            (true, Some(state)) if state.launches(&self.argv()) => {}
            (true, state) => {
                // A stale or unreadable entry is re-pointed and keeps the user's
                // disable. Only a missing entry is created enabled.
                let disabled = state.is_some_and(|s| s.disabled);
                match self.write_entry(disabled) {
                    Ok(()) if present => {
                        tracing::info!("autostart entry re-pointed at {}", self.exe);
                    }
                    Ok(()) => tracing::info!("autostart entry written"),
                    Err(e) => tracing::warn!("autostart registration failed: {e}"),
                }
            }
            (false, _) if present => {
                if let Err(e) = self.set_enabled(false) {
                    tracing::warn!("autostart removal failed: {e}");
                }
            }
            (false, _) => {}
        }
    }
}

/// Picks the program an autostart entry must launch, and refuses anything that
/// is not an existing file. See the module docs for the rules.
///
/// `exists` is the file check, injected so the decision can be tested without a
/// filesystem.
fn choose_exe(
    appimage: Option<&str>,
    appdir: Option<&str>,
    current_exe: &Path,
    exists: impl Fn(&Path) -> bool,
) -> Result<PathBuf, String> {
    let current = strip_deleted_suffix(current_exe);
    let in_mount = appdir.is_some_and(|dir| !dir.is_empty() && current.starts_with(dir));
    if in_mount {
        return match appimage.map(PathBuf::from).filter(|image| exists(image)) {
            Some(image) => Ok(image),
            None => Err(
                "this program runs from an AppImage whose file is gone; start it from \
                 its .AppImage file to use start at login"
                    .into(),
            ),
        };
    }
    if exists(&current) {
        Ok(current)
    } else {
        Err(format!(
            "the program file {} does not exist",
            current.display()
        ))
    }
}

fn strip_deleted_suffix(path: &Path) -> PathBuf {
    path.to_str()
        .and_then(|s| s.strip_suffix(DELETED_SUFFIX))
        .map_or_else(|| path.to_path_buf(), PathBuf::from)
}

/// An environment variable as UTF-8, or `None` when unset, empty or not UTF-8.
fn env_utf8(name: &str) -> Option<String> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .and_then(|v| v.into_string().ok())
}

/// Creates `path` with `content` and flushes it to disk, so the rename that
/// follows publishes a complete file.
fn write_synced(path: &Path, content: &str) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()
}

/// The complete desktop file that registers `argv` as the login command.
fn desktop_entry(argv: &[String], disabled: bool) -> String {
    let exec = exec_value(argv);
    let enabled = !disabled;
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=ShareCtrlScreen\n\
         Exec={exec}\n\
         Terminal=false\n\
         Hidden={disabled}\n\
         X-GNOME-Autostart-enabled={enabled}\n"
    )
}

/// What a desktop file says about the entry: the Exec arguments (`None` if
/// absent or malformed) and whether the user has disabled it.
#[derive(Debug, PartialEq, Eq)]
struct EntryState {
    argv: Option<Vec<String>>,
    disabled: bool,
}

impl EntryState {
    fn launches(&self, argv: &[String]) -> bool {
        self.argv.as_deref() == Some(argv)
    }
}

/// Reads the `[Desktop Entry]` group. Keys in other groups are ignored, and so
/// are spaces around `=`, which the spec allows.
fn parse_entry(content: &str) -> EntryState {
    let mut state = EntryState {
        argv: None,
        disabled: false,
    };
    let mut in_entry = false;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            if in_entry {
                break;
            }
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "Exec" if state.argv.is_none() => {
                state.argv = unescape_string(value).and_then(|v| split_exec(&v));
            }
            "Hidden" if value == "true" => state.disabled = true,
            "X-GNOME-Autostart-enabled" if value == "false" => state.disabled = true,
            _ => {}
        }
    }
    state
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

    /// Creates an empty program file under the scratch home and returns its path.
    fn program(home: &TempConfigHome, rel: &str) -> String {
        let path = home.0.join(rel);
        fs::create_dir_all(path.parent().expect("parent")).expect("create program dir");
        fs::write(&path, "").expect("create program file");
        path.to_string_lossy().into_owned()
    }

    fn autostart_for(home: &TempConfigHome, exe: &str) -> XdgAutostart {
        XdgAutostart::new(&home.0, PathBuf::from(exe)).expect("absolute utf-8 path")
    }

    fn argv(exe: &str) -> Vec<String> {
        vec![exe.to_string(), AUTOSTART_FLAG.to_string()]
    }

    fn exec_of(content: &str) -> Option<Vec<String>> {
        parse_entry(content).argv
    }

    fn write_raw(home: &TempConfigHome, content: &str) {
        fs::create_dir_all(home.entry().parent().expect("dir")).expect("create dir");
        fs::write(home.entry(), content).expect("write entry");
    }

    #[test]
    fn plain_path_writes_the_expected_exec_line() {
        let entry = desktop_entry(&argv("/opt/sharectrl/bin/sharectrl"), false);
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
        let entry = desktop_entry(&argv("/opt/My App/sc \"x\""), false);
        assert!(entry.contains(r#"Exec="/opt/My App/sc \\"x\\"" --autostart"#));
    }

    #[test]
    fn percent_is_written_as_a_literal() {
        let entry = desktop_entry(&argv("/srv/100%/sharectrl"), false);
        assert!(entry.contains("Exec=/srv/100%%/sharectrl --autostart"));
    }

    #[test]
    fn backslash_dollar_and_backtick_have_exact_encodings() {
        // Exact strings, so a matching-but-wrong encoder and decoder pair
        // cannot pass the round-trip test below.
        let backslash = desktop_entry(&argv("/a\\b"), false);
        assert!(backslash.contains(r#"Exec="/a\\\\b" --autostart"#));
        let dollar = desktop_entry(&argv("/a$b"), false);
        assert!(dollar.contains(r#"Exec="/a\\$b" --autostart"#));
        let backtick = desktop_entry(&argv("/a`b"), false);
        assert!(backtick.contains(r#"Exec="/a\\`b" --autostart"#));
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
            let entry = desktop_entry(&argv(path), false);
            assert_eq!(exec_of(&entry), Some(argv(path)), "path {path:?}");
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
        assert_eq!(exec_of(content), Some(argv("/right")));
        assert_eq!(exec_of("[Desktop Entry]\nExec[de]=/x\n"), None);
    }

    #[test]
    fn spaces_around_equals_are_accepted() {
        let content = "[Desktop Entry]\nExec = /sp --autostart\nHidden = true\n";
        let state = parse_entry(content);
        assert_eq!(state.argv, Some(argv("/sp")));
        assert!(state.disabled);
    }

    #[test]
    fn each_disable_flag_makes_the_entry_disabled() {
        let home = TempConfigHome::new("flags");
        let exe = program(&home, "bin/sharectrl");
        let enabled = desktop_entry(&argv(&exe), false);
        let hidden = enabled.replace("Hidden=false", "Hidden=true");
        let gnome = enabled.replace(
            "X-GNOME-Autostart-enabled=true",
            "X-GNOME-Autostart-enabled=false",
        );
        let autostart = autostart_for(&home, &exe);

        write_raw(&home, &enabled);
        assert!(autostart.is_enabled());

        write_raw(&home, &hidden);
        assert!(!autostart.is_enabled(), "Hidden=true must disable");

        write_raw(&home, &gnome);
        assert!(
            !autostart.is_enabled(),
            "X-GNOME-Autostart-enabled=false must disable"
        );
    }

    #[test]
    fn enable_then_disable_round_trips() {
        let home = TempConfigHome::new("roundtrip");
        let exe = program(&home, "bin/sharectrl");
        let autostart = autostart_for(&home, &exe);
        assert!(!autostart.is_enabled());

        autostart.set_enabled(true).expect("write entry");
        assert!(autostart.is_enabled());
        let written = fs::read_to_string(home.entry()).expect("entry exists");
        assert_eq!(exec_of(&written), Some(argv(&exe)));

        // The atomic write must leave no temporary file behind.
        let names: Vec<String> = fs::read_dir(home.0.join("autostart"))
            .expect("autostart dir")
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect();
        assert_eq!(names, vec![ENTRY_NAME.to_string()]);

        autostart.set_enabled(false).expect("remove entry");
        assert!(!autostart.is_enabled());
        assert!(!home.entry().exists());
        // Disabling an absent entry is success.
        autostart
            .set_enabled(false)
            .expect("second removal is a no-op");
    }

    #[test]
    fn an_explicit_enable_clears_a_disable() {
        let home = TempConfigHome::new("reenable");
        let exe = program(&home, "bin/sharectrl");
        write_raw(&home, &desktop_entry(&argv(&exe), true));
        let autostart = autostart_for(&home, &exe);
        assert!(!autostart.is_enabled());
        autostart.set_enabled(true).expect("write entry");
        assert!(autostart.is_enabled());
    }

    #[test]
    fn a_stale_path_is_detected_and_repointed() {
        let home = TempConfigHome::new("stale");
        let old = program(&home, "old/sharectrl");
        let new = program(&home, "new/sharectrl");
        autostart_for(&home, &old)
            .set_enabled(true)
            .expect("write entry");

        let moved = autostart_for(&home, &new);
        assert!(!moved.is_enabled());
        moved.reconcile(true);
        assert!(moved.is_enabled());
        let written = fs::read_to_string(home.entry()).expect("entry exists");
        assert_eq!(exec_of(&written), Some(argv(&new)));
    }

    #[test]
    fn a_stale_disabled_entry_stays_disabled_after_reconcile() {
        let home = TempConfigHome::new("stale-disabled");
        let old = program(&home, "old/sharectrl");
        let new = program(&home, "new/sharectrl");
        write_raw(&home, &desktop_entry(&argv(&old), true));

        let moved = autostart_for(&home, &new);
        moved.reconcile(true);
        let written = fs::read_to_string(home.entry()).expect("entry exists");
        assert_eq!(exec_of(&written), Some(argv(&new)), "Exec is re-pointed");
        assert!(
            parse_entry(&written).disabled,
            "the user's disable survives"
        );
        assert!(!moved.is_enabled());
    }

    #[test]
    fn an_unparseable_entry_counts_as_stale() {
        let home = TempConfigHome::new("garbage");
        let exe = program(&home, "bin/sharectrl");
        write_raw(&home, "not a desktop file");

        let autostart = autostart_for(&home, &exe);
        assert!(!autostart.is_enabled());
        autostart.reconcile(true);
        assert!(autostart.is_enabled());
    }

    #[test]
    fn reconcile_removes_the_entry_when_not_wanted() {
        let home = TempConfigHome::new("unwant");
        let exe = program(&home, "bin/sharectrl");
        let autostart = autostart_for(&home, &exe);
        autostart.set_enabled(true).expect("write entry");
        autostart.reconcile(false);
        assert!(!home.entry().exists());
    }

    #[test]
    fn reconcile_false_with_no_entry_does_nothing() {
        let home = TempConfigHome::new("noop");
        let exe = program(&home, "bin/sharectrl");
        autostart_for(&home, &exe).reconcile(false);
        assert!(!home.entry().exists());
        assert!(!home.0.join("autostart").exists(), "no directory created");
    }

    #[test]
    fn a_relative_path_is_refused() {
        let home = TempConfigHome::new("refuse");
        assert!(XdgAutostart::new(&home.0, PathBuf::from("sharectrl")).is_err());
    }

    #[test]
    fn a_missing_program_is_refused_before_writing() {
        let home = TempConfigHome::new("missing");
        let autostart = autostart_for(&home, "/nonexistent/sharectrl");
        let err = autostart
            .set_enabled(true)
            .expect_err("program does not exist");
        assert!(err.contains("is not a file"), "{err}");
        assert!(!home.entry().exists());
    }

    #[test]
    fn an_unwritable_location_gives_a_readable_error() {
        let home = TempConfigHome::new("unwritable");
        let exe = program(&home, "bin/sharectrl");
        // A regular file where the config home should be makes the
        // `autostart` directory uncreatable on every platform.
        let blocker = home.0.join("blocker");
        fs::write(&blocker, "").expect("create blocker file");
        let autostart = XdgAutostart::new(&blocker, PathBuf::from(exe)).expect("absolute path");
        let err = autostart
            .set_enabled(true)
            .expect_err("cannot write under a file");
        assert!(
            err.starts_with("could not write the autostart entry"),
            "{err}"
        );
    }

    #[test]
    fn inherited_appimage_is_ignored_outside_a_mount() {
        // A deb build started from a terminal inside another AppImage inherits
        // APPIMAGE and APPDIR, but it is not running from that mount.
        let chosen = choose_exe(
            Some("/home/u/Other.AppImage"),
            Some("/tmp/.mount_OtherXYZ"),
            Path::new("/usr/bin/sharectrl"),
            |_| true,
        );
        assert_eq!(chosen, Ok(PathBuf::from("/usr/bin/sharectrl")));
    }

    #[test]
    fn appimage_is_used_inside_its_mount() {
        let image = Path::new("/home/u/Share.AppImage");
        let chosen = choose_exe(
            image.to_str(),
            Some("/tmp/.mount_ShareXYZ"),
            Path::new("/tmp/.mount_ShareXYZ/usr/bin/sharectrl"),
            |p| p == image,
        );
        assert_eq!(chosen, Ok(image.to_path_buf()));
    }

    #[test]
    fn a_deleted_appimage_is_an_error_never_the_mount_path() {
        let chosen = choose_exe(
            Some("/home/u/Share.AppImage"),
            Some("/tmp/.mount_ShareXYZ"),
            Path::new("/tmp/.mount_ShareXYZ/usr/bin/sharectrl"),
            |_| false,
        );
        assert!(chosen.is_err());
    }

    #[test]
    fn a_replaced_binary_loses_the_deleted_suffix() {
        let chosen = choose_exe(None, None, Path::new("/usr/bin/sharectrl (deleted)"), |p| {
            p == Path::new("/usr/bin/sharectrl")
        });
        assert_eq!(chosen, Ok(PathBuf::from("/usr/bin/sharectrl")));
    }

    #[test]
    fn a_missing_program_file_is_an_error() {
        let chosen = choose_exe(None, None, Path::new("/usr/bin/sharectrl"), |_| false);
        assert!(chosen.is_err());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn generated_entry_passes_desktop_file_validate() {
        // Runs only where the validator is installed (`desktop-file-utils`). CI
        // sets SCT_REQUIRE_DESKTOP_VALIDATE=1 so a missing tool fails there.
        let home = TempConfigHome::new("validate");
        let exe = program(&home, "My App/bin/sharectrl");
        autostart_for(&home, &exe)
            .set_enabled(true)
            .expect("write entry");
        let output = match std::process::Command::new("desktop-file-validate")
            .arg(home.entry())
            .output()
        {
            Ok(output) => output,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if std::env::var("SCT_REQUIRE_DESKTOP_VALIDATE").is_ok_and(|v| v == "1") {
                    panic!("desktop-file-validate is required here but is not installed");
                }
                println!(
                    "skipped: desktop-file-validate is not installed \
                     (set SCT_REQUIRE_DESKTOP_VALIDATE=1 to require it)"
                );
                return;
            }
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
