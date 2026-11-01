//! Test driver: a wlroots virtual pointer that turns stdin lines into real seat
//! pointer events, so the spike's input routing can be checked on a headless
//! wlroots compositor (sway with `WLR_BACKENDS=headless`, the plan's T4 job),
//! where the seat otherwise has no pointer device at all.
//!
//! Commands, one per line (layout coordinates, logical px):
//! `move X Y` · `press BUTTON` · `release BUTTON` · `wheel V120` (positive =
//! down) · `sleep MS`. BUTTON is an evdev code (272 left, 273 right, 274 middle).
//!
//! Usage: `drive <layout-width> <layout-height> < commands`. Each sent command
//! is echoed to stdout as `sent <unix-µs> <command>`.
//!
//! ## References
//! * wlr-virtual-pointer-unstable-v1 (`motion_absolute` with an extent, `axis`
//!   + `axis_discrete`, `frame`): <https://wayland.app/protocols/wlr-virtual-pointer-unstable-v1>
//! * wayland-client 0.31 globals: <https://docs.rs/wayland-client/0.31/wayland_client/globals/>

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the driver runs on Linux only");
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    match driver::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("drive: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "linux")]
mod driver {
    use std::io::BufRead;
    use std::time::{Duration, Instant};

    use wayland_client::globals::{registry_queue_init, GlobalListContents};
    use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};
    use wayland_client::protocol::wl_registry::WlRegistry;
    use wayland_client::protocol::wl_seat::WlSeat;
    use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle};
    use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
    use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

    struct State;

    impl Dispatch<WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &WlRegistry,
            _: <WlRegistry as Proxy>::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    delegate_noop!(State: ignore WlSeat);
    delegate_noop!(State: ZwlrVirtualPointerManagerV1);
    delegate_noop!(State: ZwlrVirtualPointerV1);

    fn arg(args: &mut std::env::Args, what: &str) -> Result<u32, String> {
        args.next()
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("usage: drive <layout-width> <layout-height>; missing {what}"))
    }

    pub fn run() -> Result<(), String> {
        let mut args = std::env::args();
        args.next();
        let extent = (arg(&mut args, "width")?, arg(&mut args, "height")?);
        let conn = Connection::connect_to_env().map_err(|e| format!("connect: {e}"))?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).map_err(|e| format!("registry: {e}"))?;
        let qh = queue.handle();
        let seat: WlSeat = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| "no wl_seat".to_string())?;
        let manager: ZwlrVirtualPointerManagerV1 = globals
            .bind(&qh, 1..=2, ())
            .map_err(|_| "the compositor has no zwlr_virtual_pointer_manager_v1".to_string())?;
        let pointer = manager.create_virtual_pointer(Some(&seat), &qh, ());
        let started = Instant::now();
        let now = || started.elapsed().as_millis() as u32;
        queue
            .roundtrip(&mut State)
            .map_err(|e| format!("roundtrip: {e}"))?;

        for line in std::io::stdin().lock().lines() {
            let line = line.map_err(|e| format!("stdin: {e}"))?;
            let words: Vec<&str> = line.split_whitespace().collect();
            let num = |i: usize| -> Result<i64, String> {
                words
                    .get(i)
                    .and_then(|w| w.parse().ok())
                    .ok_or_else(|| format!("bad command: {line}"))
            };
            match words.first().copied() {
                Some("move") => {
                    let (x, y) = (num(1)?, num(2)?);
                    pointer.motion_absolute(now(), x as u32, y as u32, extent.0, extent.1);
                }
                Some(cmd @ ("press" | "release")) => {
                    let state = if cmd == "press" {
                        ButtonState::Pressed
                    } else {
                        ButtonState::Released
                    };
                    pointer.button(now(), num(1)? as u32, state);
                }
                Some("wheel") => {
                    let v120 = num(1)? as i32;
                    pointer.axis_source(AxisSource::Wheel);
                    // One detent is 15 px of scroll distance in libinput terms.
                    pointer.axis_discrete(
                        now(),
                        Axis::VerticalScroll,
                        15.0 * v120 as f64 / 120.0,
                        v120 / 120,
                    );
                }
                Some("sleep") => {
                    std::thread::sleep(Duration::from_millis(num(1)? as u64));
                    continue;
                }
                Some(other) => return Err(format!("unknown command {other}")),
                None => continue,
            }
            pointer.frame();
            // Wall-clock send time, compared with the spike's log timestamps
            // to measure compositor → GDK connection → VideoInput latency.
            let sent = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_micros())
                .unwrap_or(0);
            println!("sent {sent} {line}");
            queue
                .roundtrip(&mut State)
                .map_err(|e| format!("roundtrip: {e}"))?;
        }
        pointer.destroy();
        conn.flush().map_err(|e| format!("flush: {e}"))
    }
}
