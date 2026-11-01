//! The monitors of an X11 screen, and their stable ids for `shareDisplayId`.
//!
//! An X11 screen is one root window spanning every monitor; RandR 1.5
//! `GetMonitors` says which rectangle of it each monitor shows. Ids are
//! `"x11:<monitor name>"`. For the monitors the server creates itself the name
//! is the output's connector (`DP-1`, `HDMI-A-0`, `eDP-1`), which is stable
//! across restarts while the cabling is unchanged — the same promise the
//! Windows `dxgi:<n>` and macOS `cg:<edid>` ids make. A server without RandR
//! 1.5 has one monitor, the whole root window, `"x11:root"`.
//!
//! X11 has no per-monitor scale: the desktop sets one `Xft.dpi` for every
//! toolkit, so every monitor reports that scale.

use platform_api::{DisplayInfo, Rect};
use x11rb::errors::ReplyError;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Window};
use x11rb::rust_connection::RustConnection;

const PREFIX: &str = "x11:";
const ROOT_NAME: &str = "root";

/// Prefix of the outputs (and so of the monitors) of every Xwayland server.
pub(super) const XWAYLAND_OUTPUT_PREFIX: &str = "XWAYLAND";

/// One monitor in root-window coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Monitor {
    pub id: String,
    pub name: String,
    pub rect: Rect,
    pub primary: bool,
}

/// Make exactly one monitor primary. RandR's primary flag is optional (Xvfb
/// and many minimal sessions never set one) but the engine treats the primary
/// display specially (`None` shares it; others get an input offset), so fall
/// back to the monitor at the origin, then to the first.
pub(super) fn settle_primary(monitors: &mut [Monitor]) {
    let chosen = monitors
        .iter()
        .position(|m| m.primary)
        .or_else(|| monitors.iter().position(|m| m.rect.x == 0 && m.rect.y == 0))
        .unwrap_or(0);
    for (i, m) in monitors.iter_mut().enumerate() {
        m.primary = i == chosen;
    }
}

/// The monitor to capture: `id` = a [`DisplayInfo::id`], `None` = the primary.
/// `None` result: that monitor does not exist (unplugged, or an id from
/// another OS such as `dxgi:0`).
pub(super) fn pick<'a>(monitors: &'a [Monitor], id: Option<&str>) -> Option<&'a Monitor> {
    match id {
        None => monitors.iter().find(|m| m.primary),
        Some(id) => monitors.iter().find(|m| m.id == id),
    }
}

/// Clip a monitor rectangle to the root window. RandR keeps monitors inside
/// the screen, but a configuration change in flight can briefly report one
/// past the edge, and capturing outside the root is a `BadMatch`.
fn clip_to_root(r: Rect, root: (u16, u16)) -> Option<Rect> {
    crate::capture::geometry::to_local(
        r,
        Rect {
            x: 0,
            y: 0,
            w: root.0 as u32,
            h: root.1 as u32,
        },
    )
}

/// The monitors of the screen whose root is `root`, primary settled.
/// `randr_monitors`: the server speaks RandR ≥ 1.5.
pub(super) fn query(
    conn: &RustConnection,
    root: Window,
    randr_monitors: bool,
) -> Result<Vec<Monitor>, ReplyError> {
    let geometry = conn.get_geometry(root)?.reply()?;
    let root_size = (geometry.width, geometry.height);
    let mut out = Vec::new();
    if randr_monitors {
        // `get_active`: only monitors that show something (RandR spec:
        // "should always be set by toolkits").
        let reply = conn.randr_get_monitors(root, true)?.reply()?;
        // Pipeline the name lookups: one round trip for all of them.
        let names = reply
            .monitors
            .iter()
            .map(|m| conn.get_atom_name(m.name))
            .collect::<Result<Vec<_>, _>>()?;
        for (m, name) in reply.monitors.iter().zip(names) {
            let name = String::from_utf8_lossy(&name.reply()?.name).into_owned();
            let rect = Rect {
                x: m.x as i32,
                y: m.y as i32,
                w: m.width as u32,
                h: m.height as u32,
            };
            if let Some(local) = clip_to_root(rect, root_size) {
                out.push(Monitor {
                    id: format!("{PREFIX}{name}"),
                    name,
                    rect: local,
                    primary: m.primary,
                });
            }
        }
    }
    if out.is_empty() {
        out.push(Monitor {
            id: format!("{PREFIX}{ROOT_NAME}"),
            name: "Screen".into(),
            rect: Rect {
                x: 0,
                y: 0,
                w: root_size.0 as u32,
                h: root_size.1 as u32,
            },
            primary: true,
        });
    }
    settle_primary(&mut out);
    Ok(out)
}

/// The desktop's scale factor from `Xft.dpi` in the root window's
/// `RESOURCE_MANAGER` property (what `xrdb -query` prints and what GTK and Qt
/// read on X11). 1.0 when unset or unreadable.
pub(super) fn desktop_scale(conn: &RustConnection, root: Window) -> f64 {
    let resources = conn
        .get_property(
            false,
            root,
            AtomEnum::RESOURCE_MANAGER,
            AtomEnum::STRING,
            0,
            // In 4-byte units: 256 KiB, far more than any resource database.
            1 << 16,
        )
        .ok()
        .and_then(|c| c.reply().ok());
    resources
        .map(|r| scale_from_resources(&String::from_utf8_lossy(&r.value)))
        .unwrap_or(1.0)
}

/// Parse `Xft.dpi` out of a resource database string (`Xft.dpi:\t192`).
fn scale_from_resources(db: &str) -> f64 {
    db.lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim() == "Xft.dpi")
        .and_then(|(_, v)| v.trim().parse::<f64>().ok())
        .filter(|dpi| dpi.is_finite() && *dpi > 0.0)
        .map(|dpi| dpi / 96.0)
        .unwrap_or(1.0)
}

pub(super) fn to_display_info(m: &Monitor, scale: f64) -> DisplayInfo {
    DisplayInfo {
        id: m.id.clone(),
        name: m.name.clone(),
        x: m.rect.x,
        y: m.rect.y,
        width: m.rect.w,
        height: m.rect.h,
        scale,
        primary: m.primary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(name: &str, x: i32, primary: bool) -> Monitor {
        Monitor {
            id: format!("{PREFIX}{name}"),
            name: name.into(),
            rect: Rect {
                x,
                y: 0,
                w: 1920,
                h: 1080,
            },
            primary,
        }
    }

    #[test]
    fn randr_primary_wins_then_origin_then_first() {
        let mut m = vec![mon("HDMI-1", 0, false), mon("DP-1", 1920, true)];
        settle_primary(&mut m);
        assert!(!m[0].primary && m[1].primary);

        let mut m = vec![mon("DP-1", 1920, false), mon("HDMI-1", 0, false)];
        settle_primary(&mut m);
        assert!(!m[0].primary && m[1].primary, "the monitor at the origin");

        let mut m = vec![mon("DP-1", 1920, false), mon("DP-2", 3840, false)];
        settle_primary(&mut m);
        assert!(m[0].primary && !m[1].primary);
    }

    #[test]
    fn pick_by_id_or_primary_and_reject_foreign_ids() {
        let mut m = vec![mon("HDMI-1", 0, false), mon("DP-1", 1920, false)];
        settle_primary(&mut m);
        assert_eq!(pick(&m, None).map(|m| m.name.as_str()), Some("HDMI-1"));
        assert_eq!(
            pick(&m, Some("x11:DP-1")).map(|m| m.name.as_str()),
            Some("DP-1")
        );
        assert_eq!(pick(&m, Some("x11:DP-9")), None, "unplugged");
        assert_eq!(pick(&m, Some("dxgi:0")), None, "another OS's id");
        assert_eq!(pick(&m, Some("DP-1")), None, "bare connector name");
    }

    #[test]
    fn monitors_are_clipped_to_the_root() {
        let r = Rect {
            x: 1800,
            y: 0,
            w: 400,
            h: 2000,
        };
        assert_eq!(
            clip_to_root(r, (1920, 1080)),
            Some(Rect {
                x: 1800,
                y: 0,
                w: 120,
                h: 1080
            })
        );
        assert_eq!(clip_to_root(Rect { x: 1920, ..r }, (1920, 1080)), None);
    }

    #[test]
    fn xft_dpi_gives_the_scale() {
        assert_eq!(
            scale_from_resources("Xft.antialias:\t1\nXft.dpi:\t192\n"),
            2.0
        );
        assert_eq!(scale_from_resources("Xft.dpi: 144"), 1.5);
        assert_eq!(scale_from_resources("Xcursor.size:\t24\n"), 1.0);
        assert_eq!(scale_from_resources("Xft.dpi:\tlots"), 1.0);
        assert_eq!(scale_from_resources("Xft.dpi:\t0"), 1.0);
        assert_eq!(scale_from_resources(""), 1.0);
    }
}
