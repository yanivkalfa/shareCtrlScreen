//! Private X servers for tests, shared by `tests/x11_capture.rs` and the
//! clipboard unit tests (`src/clipboard/tests.rs`, included with `#[path]`).
//!
//! Each server runs on a display number of its own, so tests never touch the
//! developer's desktop — which matters most under WSLg, where `:0` mirrors the
//! Windows clipboard and a Windows `cargo test` running at the same time would
//! change what the tests read.
//!
//! A missing program is a skip (a developer box without X tools stays green)
//! unless `SCT_X11_TESTS_REQUIRED=1` (the CI Linux job) makes it a failure.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

/// Report a missing prerequisite: a skip normally, a failure when
/// `SCT_X11_TESTS_REQUIRED=1` says this run must exercise everything.
pub fn skip(why: &str) {
    if std::env::var_os("SCT_X11_TESTS_REQUIRED").is_some_and(|v| v == "1") {
        panic!("{why}, and SCT_X11_TESTS_REQUIRED=1");
    }
    eprintln!("skipping: {why}");
}

/// Whether `program` is on `PATH` (a skip when it is not).
pub fn installed(program: &str) -> bool {
    let found = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()));
    if !found {
        skip(&format!("{program} is not installed"));
    }
    found
}

/// Next display number to try. Starts high and per process, so parallel
/// tests and parallel test binaries do not collide with each other or with a
/// desktop's `:0`/`:1`.
static NEXT_DISPLAY: AtomicU32 = AtomicU32::new(0);

fn next_display() -> u32 {
    let base = 200 + (std::process::id() % 400) * 16;
    base + NEXT_DISPLAY.fetch_add(1, Ordering::SeqCst)
}

/// Whether display `n` looks taken: its socket file, lock file or abstract
/// socket exists.
fn display_taken(n: u32) -> bool {
    let abstract_name = format!("@/tmp/.X11-unix/X{n}");
    std::path::Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
        || std::path::Path::new(&format!("/tmp/.X{n}-lock")).exists()
        || std::fs::read_to_string("/proc/net/unix")
            .is_ok_and(|t| t.lines().any(|l| l.ends_with(&abstract_name)))
}

/// An X server child process, killed on drop.
pub struct XServer {
    child: Child,
    /// The `DISPLAY` value that reaches it.
    pub name: String,
}

impl XServer {
    /// Start `program` on a free display with `args`. `-displayfd 1` makes
    /// the server print the display number on stdout once it accepts
    /// connections, which is the readiness signal. The number is chosen here
    /// rather than left to `-displayfd`'s own search: that search gives up
    /// fatally when a low display's socket exists without its lock file
    /// (WSLg's `/tmp/.X11-unix/X0`). A number someone else took is retried
    /// with the next one. `None` when the program is not installed.
    ///
    /// `tcp`: also listen on TCP and hand out `127.0.0.1:<n>`, for clients
    /// that cannot reach a server through the abstract socket alone (see the
    /// clipboard tests); otherwise Unix sockets only, `:<n>`.
    pub fn start(program: &str, args: &[&str], parent: Option<&str>, tcp: bool) -> Option<Self> {
        for _ in 0..8 {
            let n = next_display();
            if display_taken(n) {
                continue;
            }
            let mut cmd = Command::new(program);
            cmd.arg(format!(":{n}"))
                .args(args)
                .args(["-displayfd", "1"])
                .args(if tcp {
                    ["-listen", "tcp"]
                } else {
                    ["-nolisten", "tcp"]
                })
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            if let Some(d) = parent {
                cmd.env("DISPLAY", d);
            }
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    skip(&format!("{program} is not installed"));
                    return None;
                }
                Err(e) => panic!("cannot start {program}: {e}"),
            };
            let stdout = child.stdout.take().expect("piped stdout");
            let mut line = String::new();
            BufReader::new(stdout)
                .read_line(&mut line)
                .expect("read the display number");
            if line.trim().is_empty() {
                // Exited without becoming ready: the number was taken.
                let _ = child.wait();
                continue;
            }
            let host = if tcp { "127.0.0.1" } else { "" };
            return Some(Self {
                child,
                name: format!("{host}:{}", line.trim()),
            });
        }
        panic!("{program} did not start on any of 8 display numbers");
    }
}

impl Drop for XServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
