//! Opt-in fallback backend: kernel virtual input devices through `/dev/uinput`.
//!
//! Works under any display server, because the desktop sees an ordinary
//! keyboard and mouse. It is opt-in by construction: `/dev/uinput` is
//! `root:root 0600` on a stock system, and only the Settings installer
//! (`crate::uinput_access`, a udev rule giving the `input` group access) makes
//! it writable for the user. [`access`] reads the state that installer leaves
//! behind, so a refusal can say exactly which step is missing.
//!
//! Two devices, as real hardware has: a keyboard with every key of the
//! generated table, and an absolute pointer modelled on the QEMU/VMware USB
//! tablet (`ABS_X`/`ABS_Y` plus mouse buttons and wheels), which udev tags
//! `ID_INPUT_MOUSE` and libinput drives as an absolute pointer. Keeping them
//! apart stops libinput from classifying one combined device as something
//! odd (a keyboard with a tablet attached).
//!
//! Each message is written as one evdev frame ending in `SYN_REPORT`, the unit
//! readers (libinput, the X.org evdev driver) commit atomically.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use input_linux::sys::{BUS_VIRTUAL, KEY_MAX};
use input_linux::{
    AbsoluteAxis, AbsoluteInfo, AbsoluteInfoSetup, EventKind, EventTime, InputEvent, InputId,
    InputProperty, Key, RelativeAxis, SynchronizeKind, UInputHandle,
};
use platform_api::keymap::KeyId;
use platform_api::Rect;
use protocol::Button;

use super::mapping::{self, Point, WheelSteps, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT};
use super::{Backend, DEVICE_NAME};
use crate::uinput_access::{UINPUT_GROUP, UINPUT_RULE_PATH};
use crate::SessionType;

const UINPUT_PATH: &str = "/dev/uinput";

/// The absolute axes span `0..=ABS_MAX`; the desktop scales that onto the whole
/// screen layout, so the resolution is 1/65536 of the layout, finer than any
/// monitor's pixels.
const ABS_MAX: i32 = 0xffff;

/// Whether this user may create uinput devices, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UinputAccess {
    Writable,
    /// The device exists but this user may not open it.
    Denied(String),
    /// No `/dev/uinput` at all (module not loaded) or another failure.
    Missing(String),
}

impl UinputAccess {
    /// What the user can do, for refusals and the permissions report.
    pub(crate) fn explain(&self) -> &str {
        match self {
            Self::Writable => "/dev/uinput is writable",
            Self::Denied(why) | Self::Missing(why) => why,
        }
    }
}

/// Probe `/dev/uinput`. Opening for writing creates nothing; it only proves
/// whether creating devices would be allowed.
pub(crate) fn access() -> UinputAccess {
    let open = OpenOptions::new()
        .write(true)
        .open(UINPUT_PATH)
        .map(drop)
        .map_err(|e| e.kind());
    let rule_installed = Path::new(UINPUT_RULE_PATH).exists();
    let in_group = match (
        std::fs::read_to_string("/etc/group"),
        std::fs::read_to_string("/proc/self/status"),
    ) {
        (Ok(groups), Ok(status)) => {
            group_gid(&groups, UINPUT_GROUP).is_some_and(|gid| process_gids(&status).contains(&gid))
        }
        _ => false,
    };
    assess_access(open, rule_installed, in_group)
}

/// The decision behind [`access`], separated so every branch is tested.
fn assess_access(
    open: Result<(), io::ErrorKind>,
    rule_installed: bool,
    in_group: bool,
) -> UinputAccess {
    const ENABLE: &str = "turn on \"Enable uinput access\" in Settings";
    match open {
        Ok(()) => UinputAccess::Writable,
        Err(io::ErrorKind::NotFound) => UinputAccess::Missing(format!(
            "{UINPUT_PATH} does not exist because the uinput kernel module is not loaded; \
             {ENABLE} (it loads the module now and at every boot), or run `sudo modprobe uinput`"
        )),
        Err(io::ErrorKind::PermissionDenied) if !rule_installed => UinputAccess::Denied(format!(
            "this user may not write {UINPUT_PATH}; {ENABLE}, which installs \
             {UINPUT_RULE_PATH} and adds you to the \"{UINPUT_GROUP}\" group"
        )),
        Err(io::ErrorKind::PermissionDenied) if !in_group => UinputAccess::Denied(format!(
            "uinput access is set up but this login session is not in the \
             \"{UINPUT_GROUP}\" group yet; log out and back in"
        )),
        Err(io::ErrorKind::PermissionDenied) => UinputAccess::Denied(format!(
            "uinput access is set up and you are in \"{UINPUT_GROUP}\", but {UINPUT_PATH} is \
             still not writable; reboot, or run `sudo udevadm control --reload-rules && sudo \
             udevadm trigger --subsystem-match=misc --sysname-match=uinput`"
        )),
        Err(kind) => UinputAccess::Missing(format!(
            "{UINPUT_PATH} cannot be opened ({kind}); check what holds it (`sudo fuser -v \
             {UINPUT_PATH}`) and that the uinput module loaded cleanly (`sudo dmesg | grep \
             uinput`), or reboot"
        )),
    }
}

/// The gid of group `name` in an `/etc/group` text. Groups served only by NSS
/// (LDAP, SSSD) are not in the file; the caller then reports "not in the
/// group", whose advice (log in again) is harmless.
fn group_gid(etc_group: &str, name: &str) -> Option<u32> {
    etc_group.lines().find_map(|line| {
        let mut fields = line.split(':');
        (fields.next()? == name).then_some(())?;
        fields.nth(1)?.trim().parse().ok()
    })
}

/// Every gid this process holds (real/effective from `Gid:`, supplementary
/// from `Groups:`) in a `/proc/self/status` text. Group membership is fixed at
/// login, which is why a fresh `usermod -aG` needs a new session.
fn process_gids(status: &str) -> Vec<u32> {
    status
        .lines()
        .filter_map(|line| {
            line.strip_prefix("Gid:")
                .or_else(|| line.strip_prefix("Groups:"))
        })
        .flat_map(str::split_whitespace)
        .filter_map(|g| g.parse().ok())
        .collect()
}

/// One evdev event before encoding: type, code, value.
type Ev = (EventKind, u16, i32);

fn syn() -> Ev {
    (EventKind::Synchronize, SynchronizeKind::Report as u16, 0)
}

fn key_frame(code: u16, down: bool) -> Vec<Ev> {
    vec![(EventKind::Key, code, i32::from(down)), syn()]
}

fn motion_frame(x: i32, y: i32) -> Vec<Ev> {
    vec![
        (EventKind::Absolute, AbsoluteAxis::X as u16, x),
        (EventKind::Absolute, AbsoluteAxis::Y as u16, y),
        syn(),
    ]
}

/// `REL_WHEEL`/`REL_HWHEEL` in detents and the `_HI_RES` twins in 1/120
/// detents, in one frame: the kernel requires a hi-res device to send both,
/// and readers use whichever they understand (event-codes.rst). The protocol's
/// signs (up / right positive) are already evdev's. Empty when nothing moves.
fn wheel_frame(steps: WheelSteps) -> Vec<Ev> {
    let mut frame = Vec::with_capacity(5);
    let mut push = |axis: RelativeAxis, value: i32| {
        if value != 0 {
            frame.push((EventKind::Relative, axis as u16, value));
        }
    };
    push(RelativeAxis::Wheel, steps.notches_y);
    push(RelativeAxis::WheelHiRes, steps.v120_y);
    push(RelativeAxis::HorizontalWheel, steps.notches_x);
    push(RelativeAxis::HorizontalWheelHiRes, steps.v120_x);
    if !frame.is_empty() {
        frame.push(syn());
    }
    frame
}

/// A fraction of the layout as an absolute-axis value.
fn abs_value(fraction: f64) -> i32 {
    // `fraction` is clamped to [0,1], so the product fits.
    (fraction * f64::from(ABS_MAX)).round() as i32
}

pub(super) struct Uinput {
    keyboard: UInputHandle<File>,
    pointer: UInputHandle<File>,
    /// The bounding box of every monitor, which the absolute axes span.
    layout: Option<Rect>,
    /// What `None` targets map onto.
    primary: Option<Rect>,
    dead: bool,
}

fn uinput_error(what: &str, e: io::Error) -> String {
    format!("uinput: {what} failed: {e}")
}

fn open_uinput() -> Result<UInputHandle<File>, String> {
    OpenOptions::new()
        .write(true)
        .open(UINPUT_PATH)
        .map(UInputHandle::new)
        .map_err(|e| match e.kind() {
            io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound => {
                format!("uinput: {}", access().explain())
            }
            _ => uinput_error("opening /dev/uinput", e),
        })
}

fn device_id(product: u16) -> InputId {
    InputId {
        bustype: BUS_VIRTUAL,
        vendor: 0,
        product,
        version: 1,
    }
}

fn create_keyboard() -> Result<UInputHandle<File>, String> {
    let dev = open_uinput()?;
    let err = |what| move |e| uinput_error(what, e);
    dev.set_evbit(EventKind::Key).map_err(err("UI_SET_EVBIT"))?;
    // Every key the protocol can name, and nothing else: a device claiming
    // buttons would be classified as a mouse too.
    for code in 1..=KEY_MAX as u16 {
        if KeyId::from_evdev(code).is_some() {
            let key = Key::from_code(code)
                .map_err(|_| format!("uinput: evdev code {code} is not a key"))?;
            dev.set_keybit(key).map_err(err("UI_SET_KEYBIT"))?;
        }
    }
    let name = format!("{DEVICE_NAME} keyboard");
    dev.create(&device_id(1), name.as_bytes(), 0, &[])
        .map_err(err("creating the keyboard"))?;
    Ok(dev)
}

fn create_pointer() -> Result<UInputHandle<File>, String> {
    let dev = open_uinput()?;
    let err = |what| move |e| uinput_error(what, e);
    for kind in [EventKind::Key, EventKind::Absolute, EventKind::Relative] {
        dev.set_evbit(kind).map_err(err("UI_SET_EVBIT"))?;
    }
    for code in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE] {
        let key = Key::from_code(code)
            .map_err(|_| format!("uinput: evdev code {code} is not a button"))?;
        dev.set_keybit(key).map_err(err("UI_SET_KEYBIT"))?;
    }
    for axis in [
        RelativeAxis::Wheel,
        RelativeAxis::WheelHiRes,
        RelativeAxis::HorizontalWheel,
        RelativeAxis::HorizontalWheelHiRes,
    ] {
        dev.set_relbit(axis).map_err(err("UI_SET_RELBIT"))?;
    }
    for axis in [AbsoluteAxis::X, AbsoluteAxis::Y] {
        dev.set_absbit(axis).map_err(err("UI_SET_ABSBIT"))?;
    }
    // "Moves a cursor" rather than "is a touchscreen": an absolute device
    // without this would be mapped as a direct-touch surface by libinput.
    dev.set_propbit(InputProperty::Pointer)
        .map_err(err("UI_SET_PROPBIT"))?;
    let axis = |axis| AbsoluteInfoSetup {
        axis,
        info: AbsoluteInfo {
            maximum: ABS_MAX,
            ..AbsoluteInfo::default()
        },
    };
    let name = format!("{DEVICE_NAME} pointer");
    dev.create(
        &device_id(2),
        name.as_bytes(),
        0,
        &[axis(AbsoluteAxis::X), axis(AbsoluteAxis::Y)],
    )
    .map_err(err("creating the pointer"))?;
    Ok(dev)
}

impl Uinput {
    /// Create both devices. The desktop needs a moment to pick new devices
    /// up (the kernel documentation's example waits a second); the injector
    /// is created at session start, well before the first input arrives, so
    /// nothing waits here.
    pub(super) fn create(session: SessionType) -> Result<Self, String> {
        let (layout, primary) = match session {
            SessionType::X11 => match super::xtest::desktop_geometry(None) {
                Some((root, primary)) => (Some(root), Some(primary)),
                None => (None, None),
            },
            SessionType::Wayland => {
                let outputs = super::wlr::output_rects();
                (mapping::bounding_box(&outputs), outputs.first().copied())
            }
        };
        Self::with_geometry(layout, primary)
    }

    /// Create both devices for a known layout (`None` = unknown: the target
    /// is then taken to be the whole layout).
    pub(super) fn with_geometry(
        layout: Option<Rect>,
        primary: Option<Rect>,
    ) -> Result<Self, String> {
        Ok(Self {
            keyboard: create_keyboard()?,
            pointer: create_pointer()?,
            layout,
            primary,
            dead: false,
        })
    }

    fn write(&mut self, to_pointer: bool, frame: &[Ev]) {
        if self.dead || frame.is_empty() {
            return;
        }
        let raw: Vec<_> = frame
            .iter()
            .map(|&(kind, code, value)| {
                // The kernel stamps uinput events itself; zero is ignored.
                InputEvent {
                    time: EventTime::new(0, 0),
                    kind,
                    code,
                    value,
                }
                .into_raw()
            })
            .collect();
        let dev = if to_pointer {
            &self.pointer
        } else {
            &self.keyboard
        };
        match dev.write(&raw) {
            Ok(n) if n == raw.len() => {}
            Ok(n) => tracing::warn!("uinput: wrote {n} of {} events", raw.len()),
            Err(e) => {
                tracing::warn!("uinput: writing events failed: {e}; remote input stops");
                self.dead = true;
            }
        }
    }

    /// The evdev nodes of (keyboard, pointer), for tests that read back.
    #[cfg(test)]
    pub(super) fn evdev_paths(&self) -> io::Result<(std::path::PathBuf, std::path::PathBuf)> {
        Ok((self.keyboard.evdev_path()?, self.pointer.evdev_path()?))
    }
}

impl Backend for Uinput {
    fn pointer(&mut self, at: Point) {
        let at = mapping::on_primary(at, self.primary);
        let (fx, fy) = mapping::layout_fraction(at, self.layout);
        self.write(true, &motion_frame(abs_value(fx), abs_value(fy)));
    }

    fn button(&mut self, button: Button, down: bool) {
        self.write(true, &key_frame(mapping::evdev_button(button), down));
    }

    fn wheel(&mut self, steps: WheelSteps) {
        self.write(true, &wheel_frame(steps));
    }

    fn key(&mut self, evdev: u16, down: bool) {
        self.write(false, &key_frame(evdev, down));
    }

    fn flush(&mut self) {
        // Every frame was written whole in its own `write`.
    }
}

impl Drop for Uinput {
    fn drop(&mut self) {
        // Closing the fd destroys the device too; asking explicitly removes it
        // from the desktop at once rather than when the last reference goes.
        for dev in [&self.keyboard, &self.pointer] {
            if let Err(e) = dev.dev_destroy() {
                tracing::debug!("uinput: UI_DEV_DESTROY failed: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writable_device_is_writable() {
        assert_eq!(assess_access(Ok(()), false, false), UinputAccess::Writable);
    }

    #[test]
    fn missing_module_says_how_to_load_it() {
        let a = assess_access(Err(io::ErrorKind::NotFound), true, true);
        assert!(matches!(&a, UinputAccess::Missing(m) if m.contains("modprobe uinput")));
        assert!(a.explain().contains("Enable uinput access"));
    }

    #[test]
    fn each_install_step_has_its_own_advice() {
        let denied = Err(io::ErrorKind::PermissionDenied);
        let no_rule = assess_access(denied, false, false);
        assert!(matches!(&no_rule, UinputAccess::Denied(m) if m.contains(UINPUT_RULE_PATH)));
        let no_group = assess_access(denied, true, false);
        assert!(matches!(&no_group, UinputAccess::Denied(m) if m.contains("log out")));
        let stale = assess_access(denied, true, true);
        assert!(matches!(&stale, UinputAccess::Denied(m) if m.contains("udevadm")));
        let odd = assess_access(Err(io::ErrorKind::Other), false, false);
        assert!(matches!(odd, UinputAccess::Missing(_)));
    }

    #[test]
    fn group_and_process_gids_are_parsed() {
        let etc_group = "root:x:0:\ninputs:x:7:\ninput:x:105:alice,bob\n";
        assert_eq!(group_gid(etc_group, "input"), Some(105));
        assert_eq!(group_gid(etc_group, "video"), None);
        let status = "Name:\tsh\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\n\
                      Groups:\t4 24 105 \n";
        let gids = process_gids(status);
        assert!(gids.contains(&105) && gids.contains(&1000) && !gids.contains(&0));
        assert!(process_gids("Groups:\n").is_empty());
    }

    #[test]
    fn every_frame_ends_in_syn_report() {
        let frames = [
            key_frame(30, true),
            key_frame(30, false),
            motion_frame(1, 2),
            wheel_frame(WheelSteps {
                v120_x: 0,
                v120_y: 120,
                notches_x: 0,
                notches_y: 1,
            }),
        ];
        for f in &frames {
            assert_eq!(f.last(), Some(&syn()), "{f:?}");
            assert_eq!(f.iter().filter(|e| **e == syn()).count(), 1);
        }
        assert_eq!(key_frame(30, true)[0], (EventKind::Key, 30, 1));
        assert_eq!(key_frame(30, false)[0], (EventKind::Key, 30, 0));
    }

    #[test]
    fn wheel_frame_sends_detents_and_hi_res_together() {
        let f = wheel_frame(WheelSteps {
            v120_x: -60,
            v120_y: 240,
            notches_x: 0,
            notches_y: 2,
        });
        assert_eq!(
            f,
            vec![
                (EventKind::Relative, RelativeAxis::Wheel as u16, 2),
                (EventKind::Relative, RelativeAxis::WheelHiRes as u16, 240),
                (
                    EventKind::Relative,
                    RelativeAxis::HorizontalWheelHiRes as u16,
                    -60
                ),
                syn(),
            ]
        );
        assert!(wheel_frame(WheelSteps::default()).is_empty());
    }

    #[test]
    fn abs_values_span_the_axis() {
        assert_eq!(abs_value(0.0), 0);
        assert_eq!(abs_value(1.0), ABS_MAX);
        assert_eq!(abs_value(0.5), 32768);
    }

    #[test]
    fn the_keyboard_claims_only_table_keys() {
        let claimed = (1..=KEY_MAX as u16)
            .filter(|c| KeyId::from_evdev(*c).is_some())
            .count();
        assert!(claimed > 100, "the generated table has the full keyboard");
        assert!(
            KeyId::from_evdev(BTN_LEFT).is_none(),
            "no buttons on the keyboard"
        );
    }
}
