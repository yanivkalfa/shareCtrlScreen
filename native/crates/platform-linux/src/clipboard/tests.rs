//! Clipboard tests against a real display server.
//!
//! The clipboard is one global resource per display and tests run on parallel
//! threads, so every test that touches it holds `exclusive()`. Tests that need a
//! display, or a helper tool (`xclip`, `wl-paste`), return early with a message
//! when it is missing: a headless CI runner is not a failure, and the early
//! return is visible in `--nocapture` output.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::*;

static CLIPBOARD_TEST_LOCK: Mutex<()> = Mutex::new(());

fn exclusive() -> MutexGuard<'static, ()> {
    lock(&CLIPBOARD_TEST_LOCK)
}

fn x11_available() -> bool {
    x11rb::connect(None).is_ok()
}

fn tool_available(name: &str) -> bool {
    // Tools disagree on how to ask for a version (Xvfb has no `--version`), so
    // look the executable up on PATH instead of running it.
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
}

/// Skips the calling test unless an X11 server and `xclip` are present.
macro_rules! require_x11 {
    () => {
        if !x11_available() || !tool_available("xclip") {
            eprintln!("skipped: needs an X11 display and xclip");
            return;
        }
    };
}

fn xclip(display: &str, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("xclip")
        .env("DISPLAY", display)
        .args(["-selection", "clipboard", "-o"])
        .args(args)
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// What another application's Ctrl+C looks like: `xclip -i` takes the selection
/// from whoever has it.
fn xclip_copy(display: &str, text: &str) {
    let mut child = Command::new("xclip")
        .env("DISPLAY", display)
        .args(["-selection", "clipboard", "-i"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("xclip starts");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(text.as_bytes())
        .expect("write to xclip");
    // xclip forks to keep serving and the parent exits once it owns the selection.
    child.wait().expect("xclip exits");
}

fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for: {what}");
}

fn display_env() -> String {
    std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into())
}

fn temp_file(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(name);
    std::fs::write(&p, b"x").expect("write temp file");
    p
}

// ---- text ----------------------------------------------------------------

#[test]
fn text_roundtrips_and_set_returns_the_count_that_follows() {
    require_x11!();
    let _lock = exclusive();
    let clip = LinuxClipboard::new(false);
    let marker = "sharectrl clipboard test \u{5d0}\u{5d1}"; // non-ASCII on purpose
    let count = clip.set_text(marker).expect("text set");
    assert_eq!(clip.get_text().as_deref(), Some(marker));
    // The echo guard depends on this equality.
    assert_eq!(clip.change_count(), count);
}

#[test]
fn empty_text_reads_back_as_none() {
    require_x11!();
    let _lock = exclusive();
    let clip = LinuxClipboard::new(false);
    clip.set_text("seed").expect("text set");
    let count = clip.set_text("").expect("clear");
    assert_eq!(clip.get_text(), None);
    assert_eq!(clip.change_count(), count);
}

#[test]
fn oversized_text_is_refused_not_truncated() {
    require_x11!();
    let _lock = exclusive();
    let clip = LinuxClipboard::new(false);
    clip.set_text("small").expect("text set");
    assert_eq!(clip.set_text(&"a".repeat(MAX_TEXT_BYTES + 1)), None);
    assert_eq!(clip.get_text().as_deref(), Some("small"));
    // And a foreign oversized copy is not read back truncated either.
    xclip_copy(&display_env(), &"b".repeat(MAX_TEXT_BYTES + 1));
    eventually("oversized foreign text is not read back", || {
        clip.get_text().is_none()
    });
}

#[test]
fn a_copy_in_another_app_advances_the_count() {
    require_x11!();
    let _lock = exclusive();
    let clip = LinuxClipboard::new(false);
    let ours = clip.set_text("ours").expect("text set");
    xclip_copy(&display_env(), "theirs");
    eventually("foreign text visible", || {
        clip.get_text().as_deref() == Some("theirs")
    });
    assert_ne!(clip.change_count(), ours);
}

// ---- file lists on X11 ---------------------------------------------------

#[test]
fn x11_file_list_is_offered_under_the_three_file_manager_targets() {
    require_x11!();
    let _lock = exclusive();
    let display = display_env();
    let clip = LinuxClipboard::new(false);
    let file = temp_file("sharectrl x11 target test.txt");
    let count = clip
        .set_files(std::slice::from_ref(&file))
        .expect("file list set");

    let targets = String::from_utf8(xclip(&display, &["-t", "TARGETS"]).expect("targets"))
        .expect("utf-8 targets");
    for t in [
        "text/uri-list",
        "x-special/gnome-copied-files",
        "application/x-kde-cutselection",
    ] {
        assert!(
            targets.lines().any(|l| l == t),
            "{t} missing from {targets}"
        );
    }
    // A file list must not double as text.
    for t in ["UTF8_STRING", "STRING", "text/plain"] {
        assert!(!targets.lines().any(|l| l == t), "{t} must not be offered");
    }

    let uri = format!("file://{}", file.display()).replace(' ', "%20");
    assert_eq!(
        xclip(&display, &["-t", "text/uri-list"]).expect("uri list"),
        format!("{uri}\r\n").into_bytes()
    );
    assert_eq!(
        xclip(&display, &["-t", "x-special/gnome-copied-files"]).expect("gnome"),
        format!("copy\n{uri}").into_bytes()
    );
    assert_eq!(
        xclip(&display, &["-t", "application/x-kde-cutselection"]).expect("kde"),
        b"0"
    );

    // Same contract as the Windows tests: no text left behind, count echoes.
    assert_eq!(clip.get_text(), None);
    assert_eq!(clip.change_count(), count);
    let _ = std::fs::remove_file(&file);
}

#[test]
fn x11_file_list_is_refused_when_empty_or_relative() {
    let clip = LinuxClipboard::new(false);
    assert_eq!(clip.set_files(&[]), None);
    assert_eq!(clip.set_files(&[PathBuf::from("relative.txt")]), None);
}

#[test]
fn x11_dropping_the_clipboard_releases_the_selection() {
    require_x11!();
    let _lock = exclusive();
    let display = display_env();
    let clip = LinuxClipboard::new(false);
    clip.set_files(&[PathBuf::from("/tmp/sharectrl-release-test")])
        .expect("file list set");
    assert!(xclip(&display, &["-t", "TARGETS"]).is_some());
    drop(clip);
    assert_eq!(xclip(&display, &["-t", "TARGETS"]), None);
}

#[test]
fn x11_another_app_copying_takes_over_from_a_file_list() {
    require_x11!();
    let _lock = exclusive();
    let clip = LinuxClipboard::new(false);
    let files = clip
        .set_files(&[PathBuf::from("/tmp/sharectrl-takeover-test")])
        .expect("file list set");
    xclip_copy(&display_env(), "text from elsewhere");
    eventually("text visible after the takeover", || {
        clip.get_text().as_deref() == Some("text from elsewhere")
    });
    assert_ne!(clip.change_count(), files);
}

#[test]
fn x11_setting_text_replaces_a_file_list() {
    require_x11!();
    let _lock = exclusive();
    let display = display_env();
    let clip = LinuxClipboard::new(false);
    clip.set_files(&[PathBuf::from("/tmp/sharectrl-replace-test")])
        .expect("file list set");
    let count = clip.set_text("now text").expect("text set");
    assert_eq!(clip.get_text().as_deref(), Some("now text"));
    assert_eq!(clip.change_count(), count);
    let targets = xclip(&display, &["-t", "TARGETS"]).expect("targets");
    assert!(!String::from_utf8_lossy(&targets).contains("text/uri-list"));
}

#[test]
fn x11_a_second_file_list_replaces_the_first() {
    require_x11!();
    let _lock = exclusive();
    let display = display_env();
    let clip = LinuxClipboard::new(false);
    let first = clip
        .set_files(&[PathBuf::from("/tmp/sharectrl-first")])
        .expect("first");
    let second = clip
        .set_files(&[PathBuf::from("/tmp/sharectrl-second")])
        .expect("second");
    assert_ne!(first, second);
    assert_eq!(clip.change_count(), second);
    assert_eq!(
        xclip(&display, &["-t", "text/uri-list"]).expect("uri list"),
        b"file:///tmp/sharectrl-second\r\n"
    );
}

// ---- file lists on Wayland ----------------------------------------------

#[test]
fn wayland_file_list_is_listed_by_wl_paste() {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() || !tool_available("wl-paste") {
        eprintln!("skipped: needs a Wayland session and wl-paste");
        return;
    }
    let _lock = exclusive();
    let offer = FileOffer::new(&[PathBuf::from("/tmp/sharectrl wayland")]).expect("offer");
    let owner = match WaylandOwner::acquire(&offer) {
        Ok(o) => o,
        Err(reason) => {
            eprintln!("skipped: this compositor has no data-control protocol ({reason})");
            return;
        }
    };
    let wl_paste = |args: &[&str]| {
        let out = Command::new("wl-paste")
            .args(args)
            .output()
            .expect("wl-paste");
        out.status.success().then_some(out.stdout)
    };
    let types = String::from_utf8(wl_paste(&["--list-types"]).expect("types")).expect("utf-8");
    for t in [
        "text/uri-list",
        "x-special/gnome-copied-files",
        "application/x-kde-cutselection",
    ] {
        assert!(types.lines().any(|l| l == t), "{t} missing from {types}");
    }
    for t in [
        "text/plain",
        "text/plain;charset=utf-8",
        "UTF8_STRING",
        "STRING",
    ] {
        assert!(!types.lines().any(|l| l == t), "{t} must not be offered");
    }
    assert_eq!(
        wl_paste(&["--no-newline", "--type", "text/uri-list"]).expect("uri list"),
        b"file:///tmp/sharectrl%20wayland\r\n"
    );
    assert!(owner.is_owned());

    drop(owner);
    assert_eq!(wl_paste(&["--list-types"]), None, "released on drop");
}

#[test]
fn wayland_set_files_through_the_clipboard_reports_the_count() {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("skipped: needs a Wayland session");
        return;
    }
    let _lock = exclusive();
    let clip = LinuxClipboard::new(true);
    let Some(count) = clip.set_files(&[PathBuf::from("/tmp/sharectrl-wayland-count")]) else {
        eprintln!("skipped: neither Wayland data-control nor X11 file lists work here");
        return;
    };
    assert_eq!(clip.get_text(), None);
    assert_eq!(clip.change_count(), count);
}

// ---- echo guard ----------------------------------------------------------

/// The echo-guarded state machine of the engine's clipboard thread, copied from
/// `engine::session::clipboard::ClipboardSync::step` (it is private to the
/// engine, which cannot depend on a platform crate). Keep the two in step: this
/// test is only as strong as the copy is faithful.
struct Sync {
    last_count: u64,
    last_text: Option<String>,
}

impl Sync {
    fn new(clipboard: &dyn Clipboard) -> Self {
        Self {
            last_count: clipboard.change_count(),
            last_text: None,
        }
    }

    fn step(
        &mut self,
        clipboard: &dyn Clipboard,
        to_peer: &Sender<String>,
        from_peer: &Receiver<String>,
    ) {
        while let Ok(text) = from_peer.try_recv() {
            if self.last_text.as_deref() == Some(text.as_str()) {
                continue;
            }
            if let Some(count) = clipboard.set_text(&text) {
                self.last_count = count;
                self.last_text = Some(text);
            }
        }
        let count = clipboard.change_count();
        if count == self.last_count {
            return;
        }
        self.last_count = count;
        if let Some(text) = clipboard.get_text() {
            if self.last_text.as_deref() != Some(text.as_str()) && text.len() <= MAX_TEXT_BYTES {
                self.last_text = Some(text.clone());
                let _ = to_peer.send(text);
            }
        }
    }
}

#[test]
fn echo_guard_one_machine_applies_peer_text_without_sending_it_back() {
    require_x11!();
    let _lock = exclusive();
    let display = display_env();
    let clip = LinuxClipboard::new(false);
    let (to_peer, sent) = channel();
    let (peer_tx, from_peer) = channel();
    let mut sync = Sync::new(&clip);

    xclip_copy(&display, "local copy");
    eventually("local copy visible", || {
        clip.get_text().as_deref() == Some("local copy")
    });
    sync.step(&clip, &to_peer, &from_peer);
    sync.step(&clip, &to_peer, &from_peer);
    assert_eq!(sent.try_iter().collect::<Vec<_>>(), ["local copy"]);

    peer_tx.send("from peer".into()).expect("channel open");
    for _ in 0..3 {
        sync.step(&clip, &to_peer, &from_peer);
    }
    assert_eq!(clip.get_text().as_deref(), Some("from peer"));
    assert!(
        sent.try_iter().next().is_none(),
        "applied peer text must not bounce back"
    );

    // Echo suppression must not blind the loop to the next real copy.
    xclip_copy(&display, "second local copy");
    eventually("second copy visible", || {
        clip.get_text().as_deref() == Some("second local copy")
    });
    for _ in 0..3 {
        sync.step(&clip, &to_peer, &from_peer);
    }
    assert_eq!(sent.try_iter().collect::<Vec<_>>(), ["second local copy"]);
}

// The two-machine case (each side applying the other's text) cannot be driven
// in one process: arboard's X11 backend keeps a single process-wide connection,
// so two clipboards in one test would share one display. What the loop relies
// on is covered above: `set_text` returns the count `change_count` then reports.
