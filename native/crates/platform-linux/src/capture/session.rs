//! One connected X11 capture session: the connection, the extension objects
//! (Damage, an XFixes region, the MIT-SHM segment), the picture buffer and the
//! event thread. Built whole by [`Session::open`] and dropped whole on
//! `reinit` — a RandR change alters the segment size, the monitor rectangle
//! and possibly the server itself, so nothing is worth patching in place.
//!
//! Server-side objects are never freed one by one: the X server frees every
//! resource of a client when its connection closes (X11 protocol, "Connection
//! Close"), which is what dropping the session does.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use platform_api::{CaptureError, CursorShape, CursorUpdate, Point, Rect};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::damage::{self, ConnectionExt as _, ReportLevel};
use x11rb::protocol::randr::{self, ConnectionExt as _, NotifyMask};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _, CursorNotifyMask};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt as _, CreateWindowAux,
    EventMask, ImageFormat, ImageOrder, Setup, VisualClass, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::{COPY_DEPTH_FROM_PARENT, NONE};

use super::events::{self, Pump, Signals, Watch};
use super::geometry::{merge_dirty, row_bytes, to_local};
use super::monitors::{self, Monitor, XWAYLAND_OUTPUT_PREFIX};
use super::pixels::{copy_opaque, cursor_bgra, is_blank};

/// Name of the extension only Xwayland initialises (xwaylandproto: "if the
/// extension is present, the X server is Xwayland").
const XWAYLAND_EXTENSION: &str = "XWAYLAND";

/// Largest cursor image reported. Real cursors are 24–128 px (256 with
/// accessibility sizes); anything bigger is an application drawing a picture
/// as its cursor, which the viewer could not draw sensibly anyway.
const MAX_CURSOR_SIDE: u16 = 256;

/// The image layout of the root window this capture accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Layout {
    pub bits_per_pixel: u8,
    pub scanline_pad: u8,
}

/// What `check_layout` needs to know about the root visual.
#[derive(Debug, Clone, Copy)]
pub(super) struct VisualFacts {
    pub depth: u8,
    pub bits_per_pixel: u8,
    pub scanline_pad: u8,
    pub lsb_first: bool,
    pub true_color: bool,
    pub masks: (u32, u32, u32),
}

/// Accept only root visuals whose Z-format pixels are B,G,R,x bytes in
/// memory: 32 bits per pixel, red/green/blue masks `ff0000`/`ff00`/`ff`,
/// least significant byte first. That is every depth-24 desktop and the
/// common depth-32 one; it is what the trait's `Bgra8` is, so frames need no
/// per-pixel shuffling. Deep colour (depth 30), 16-bit and big-endian servers
/// are refused with the reason rather than captured with wrong colours.
pub(super) fn check_layout(v: VisualFacts) -> Result<Layout, String> {
    let ok = matches!(v.depth, 24 | 32)
        && v.bits_per_pixel == 32
        && v.lsb_first
        && v.true_color
        && v.masks == (0x00ff_0000, 0x0000_ff00, 0x0000_00ff);
    if ok {
        Ok(Layout {
            bits_per_pixel: v.bits_per_pixel,
            scanline_pad: v.scanline_pad,
        })
    } else {
        Err(format!(
            "the X server's screen format is not supported (depth {}, {} bits per pixel, \
             masks {:06x}/{:06x}/{:06x}, {}). Only 24-bit colour is supported: set the \
             display to 24-bit colour (Xorg `DefaultDepth 24`).",
            v.depth,
            v.bits_per_pixel,
            v.masks.0,
            v.masks.1,
            v.masks.2,
            if v.lsb_first {
                "little-endian"
            } else {
                "big-endian"
            },
        ))
    }
}

fn visual_facts(setup: &Setup, screen: usize) -> Option<VisualFacts> {
    let s = setup.roots.get(screen)?;
    let format = setup
        .pixmap_formats
        .iter()
        .find(|f| f.depth == s.root_depth)?;
    let visual = s
        .allowed_depths
        .iter()
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.visual_id == s.root_visual)?;
    Some(VisualFacts {
        depth: s.root_depth,
        bits_per_pixel: format.bits_per_pixel,
        scanline_pad: format.scanline_pad,
        lsb_first: setup.image_byte_order == ImageOrder::LSB_FIRST,
        true_color: matches!(
            visual.class,
            VisualClass::TRUE_COLOR | VisualClass::DIRECT_COLOR
        ),
        masks: (visual.red_mask, visual.green_mask, visual.blue_mask),
    })
}

/// Which request copies pixels out of the server.
enum Grabber {
    /// MIT-SHM 1.2: the server allocates the segment and passes its fd over
    /// the socket (`ShmCreateSegment`); `ShmGetImage` writes into it and we
    /// read it back with `pread`. Reading through the fd instead of mapping it
    /// keeps the crate free of `mmap` (and of `unsafe`) at the cost of one
    /// kernel copy — the same single copy a mapping would need anyway, since
    /// the picture must persist across partial updates.
    Shm {
        seg: shm::Seg,
        file: File,
        capacity: usize,
        scratch: Vec<u8>,
    },
    /// Core `GetImage`: every pixel crosses the socket. Used when MIT-SHM is
    /// missing or older than 1.2, or cannot pass fds (a TCP `DISPLAY`).
    Core,
}

/// Cursor state between polls.
#[derive(Default)]
struct CursorState {
    /// Serial of the last image fetched; `None` = fetch on the next poll.
    serial: Option<u32>,
    /// The last image is fully transparent (the pointer is hidden).
    blank: bool,
    /// Last reported position + visibility.
    last: Option<(Point, bool)>,
}

/// Outcome of one grab.
pub(super) struct Grab {
    /// Changed rectangles in picture coordinates; empty = nothing on this
    /// monitor changed.
    pub dirty: Vec<Rect>,
}

pub(super) struct Session {
    conn: Arc<RustConnection>,
    root: Window,
    monitor: Monitor,
    /// Captures "the primary display" (asked for, or fallen back to), so a
    /// primary change is a reconfiguration.
    follows_primary: bool,
    randr_monitors: bool,
    layout: Layout,
    damage: damage::Damage,
    region: xfixes::Region,
    grabber: Grabber,
    /// The monitor's current picture, tightly packed BGRA (stride = width × 4).
    picture: Vec<u8>,
    /// Whether `picture` holds a real frame yet (the first grab is full).
    filled: bool,
    cursor: CursorState,
    pump: Arc<Pump>,
    wake: Window,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    /// Connect to `display` and set up capture of monitor `want` (`None` =
    /// primary). With `or_primary`, a monitor that no longer exists falls back
    /// to the primary one (used by `reinit`: after an unplug, sharing the main
    /// screen is what the user expects, not a dead session).
    pub(super) fn open(
        display: &str,
        want: Option<&str>,
        or_primary: bool,
    ) -> Result<Self, CaptureError> {
        let fail = |what: &str, e: &dyn std::fmt::Display| {
            CaptureError::Backend(format!("X11 capture on {display}: {what} failed: {e}"))
        };
        let (conn, screen) = RustConnection::connect(Some(display)).map_err(|e| {
            CaptureError::Backend(format!(
                "cannot connect to the X server at {display} ({e}). Is the desktop session \
                 still running, and is DISPLAY={display} the right one?"
            ))
        })?;
        let conn = Arc::new(conn);
        let described =
            visual_facts(conn.setup(), screen).zip(conn.setup().roots.get(screen).map(|s| s.root));
        let (facts, root) = described.ok_or_else(|| {
            CaptureError::Backend(format!(
                "X11 capture on {display}: the server described no format for its root window"
            ))
        })?;
        let layout = check_layout(facts).map_err(CaptureError::Backend)?;

        // Every extension must be version-negotiated before its first other
        // request (Damage spec §8, XFixes §5, RandR §1.x "QueryVersion").
        let has = |name: &'static str| -> Result<bool, CaptureError> {
            Ok(conn
                .extension_information(name)
                .map_err(|e| fail("listing extensions", &e))?
                .is_some())
        };
        if has(XWAYLAND_EXTENSION)? {
            return Err(xwayland_refusal());
        }
        for (name, purpose) in [
            (damage::X11_EXTENSION_NAME, "change tracking"),
            (xfixes::X11_EXTENSION_NAME, "change regions and the cursor"),
        ] {
            if !has(name)? {
                return Err(CaptureError::Backend(format!(
                    "the X server at {display} lacks the {name} extension (needed for \
                     {purpose}); every Xorg since 2004 has it — is this a minimal or remote X \
                     server?"
                )));
            }
        }
        let xfixes = conn
            .xfixes_query_version(5, 0)
            .map_err(|e| fail("XFixes version", &e))?
            .reply()
            .map_err(|e| fail("XFixes version", &e))?;
        if xfixes.major_version < 2 {
            return Err(CaptureError::Backend(format!(
                "the X server at {display} has XFixes {}.{}; regions need 2.0 or newer",
                xfixes.major_version, xfixes.minor_version
            )));
        }
        conn.damage_query_version(1, 1)
            .map_err(|e| fail("Damage version", &e))?
            .reply()
            .map_err(|e| fail("Damage version", &e))?;
        let randr = if has(randr::X11_EXTENSION_NAME)? {
            let v = conn
                .randr_query_version(1, 5)
                .map_err(|e| fail("RandR version", &e))?
                .reply()
                .map_err(|e| fail("RandR version", &e))?;
            Some((v.major_version, v.minor_version))
        } else {
            None
        };
        let randr_monitors = randr.is_some_and(|v| v >= (1, 5));

        let monitors = monitors::query(&conn, root, randr_monitors)
            .map_err(|e| fail("listing monitors", &e))?;
        // Older Xwayland (before the XWAYLAND extension, 2022) is still
        // recognisable by its output names.
        if monitors
            .iter()
            .any(|m| m.name.starts_with(XWAYLAND_OUTPUT_PREFIX))
        {
            return Err(xwayland_refusal());
        }
        let (monitor, follows_primary) = match monitors::pick(&monitors, want) {
            Some(m) => (m.clone(), want.is_none()),
            None if or_primary => {
                let primary = monitors::pick(&monitors, None).ok_or(CaptureError::NoOutput)?;
                tracing::warn!(
                    "x11 capture: display {want:?} is gone, sharing the primary display {}",
                    primary.id
                );
                (primary.clone(), true)
            }
            None => return Err(CaptureError::NoOutput),
        };

        // Event selection. StructureNotify on the root reports a root resize
        // even without RandR; RandR's masks add monitor/CRTC changes (CRTC and
        // output masks are RandR 1.2).
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        )
        .map_err(|e| fail("selecting root events", &e))?
        .check()
        .map_err(|e| fail("selecting root events", &e))?;
        if let Some(v) = randr {
            let mut mask = NotifyMask::SCREEN_CHANGE;
            if v >= (1, 2) {
                mask = mask | NotifyMask::CRTC_CHANGE | NotifyMask::OUTPUT_CHANGE;
            }
            conn.randr_select_input(root, mask)
                .map_err(|e| fail("selecting RandR events", &e))?
                .check()
                .map_err(|e| fail("selecting RandR events", &e))?;
        }
        conn.xfixes_select_cursor_input(root, CursorNotifyMask::DISPLAY_CURSOR)
            .map_err(|e| fail("selecting cursor events", &e))?
            .check()
            .map_err(|e| fail("selecting cursor events", &e))?;

        // NonEmpty: one notification when damage appears, then silence until
        // we subtract it — one wake-up per frame however much is drawn. The
        // rectangles themselves are fetched from the region at grab time.
        let damage = conn.generate_id().map_err(|e| fail("allocating ids", &e))?;
        conn.damage_create(damage, root, ReportLevel::NON_EMPTY)
            .map_err(|e| fail("creating the damage tracker", &e))?
            .check()
            .map_err(|e| fail("creating the damage tracker", &e))?;
        let region = conn.generate_id().map_err(|e| fail("allocating ids", &e))?;
        conn.xfixes_create_region(region, &[])
            .map_err(|e| fail("creating a region", &e))?
            .check()
            .map_err(|e| fail("creating a region", &e))?;

        // An unmapped InputOnly window: the target of the shutdown wake-up
        // message (a SendEvent with an empty mask goes to the window's
        // creator, i.e. to our own event thread).
        let wake = conn.generate_id().map_err(|e| fail("allocating ids", &e))?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            wake,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new(),
        )
        .map_err(|e| fail("creating a helper window", &e))?
        .check()
        .map_err(|e| fail("creating a helper window", &e))?;

        let stride = row_bytes(monitor.rect.w, layout.bits_per_pixel, layout.scanline_pad);
        let capacity = stride * monitor.rect.h as usize;
        let grabber = shm_grabber(&conn, display, capacity);
        let picture = vec![0u8; monitor.rect.w as usize * 4 * monitor.rect.h as usize];

        let pump = Arc::new(Pump::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        let watch = Watch {
            conn: conn.clone(),
            root,
            damage,
            pump: pump.clone(),
            shutdown: shutdown.clone(),
        };
        let thread = std::thread::Builder::new()
            .name("x11-capture-events".into())
            .spawn(move || events::run(watch))
            .map_err(|e| fail("starting the event thread", &e))?;
        // tracing's macros bring their own `display` helper into scope, so
        // the display name is logged under another binding.
        let server = display;
        tracing::info!(
            "x11 capture: {server} monitor {} ({}x{}+{}+{}), {}",
            monitor.id,
            monitor.rect.w,
            monitor.rect.h,
            monitor.rect.x,
            monitor.rect.y,
            match grabber {
                Grabber::Shm { .. } => "MIT-SHM",
                Grabber::Core => "core GetImage (no MIT-SHM 1.2)",
            }
        );
        Ok(Self {
            conn,
            root,
            monitor,
            follows_primary,
            randr_monitors,
            layout,
            damage,
            region,
            grabber,
            picture,
            filled: false,
            cursor: CursorState::default(),
            pump,
            wake,
            shutdown,
            thread: Some(thread),
        })
    }

    pub(super) fn size(&self) -> (u32, u32) {
        (self.monitor.rect.w, self.monitor.rect.h)
    }

    pub(super) fn monitor_id(&self) -> &str {
        &self.monitor.id
    }

    pub(super) fn filled(&self) -> bool {
        self.filled
    }

    pub(super) fn picture(&self) -> (&[u8], usize) {
        (&self.picture, self.monitor.rect.w as usize * 4)
    }

    pub(super) fn wait(&self, timeout: std::time::Duration) -> Signals {
        self.pump.wait(timeout)
    }

    /// Read what changed into the picture. The first grab of a session reads
    /// everything (the encoder starts with a keyframe of the whole screen).
    pub(super) fn grab(&mut self) -> Result<Grab, String> {
        let full = Rect {
            x: 0,
            y: 0,
            w: self.monitor.rect.w,
            h: self.monitor.rect.h,
        };
        // Subtract BEFORE reading pixels: whatever is drawn after this point
        // lands in the next damage and is read again next time. The other
        // order could read stale pixels and then throw their damage away.
        self.conn
            .damage_subtract(self.damage, NONE, self.region)
            .map_err(|e| format!("DamageSubtract: {e}"))?;
        let dirty = if self.filled {
            let region = self
                .conn
                .xfixes_fetch_region(self.region)
                .map_err(|e| format!("FetchRegion: {e}"))?
                .reply()
                .map_err(|e| format!("FetchRegion: {e}"))?;
            merge_dirty(
                region
                    .rectangles
                    .iter()
                    .filter_map(|r| {
                        let r = Rect {
                            x: r.x as i32,
                            y: r.y as i32,
                            w: r.width as u32,
                            h: r.height as u32,
                        };
                        to_local(r, self.monitor.rect)
                    })
                    .collect(),
            )
        } else {
            vec![full]
        };
        if dirty.is_empty() {
            return Ok(Grab { dirty });
        }
        self.read(&dirty)?;
        self.filled = true;
        Ok(Grab { dirty })
    }

    /// Copy `rects` (picture coordinates, pairwise disjoint) from the root
    /// window into the picture.
    fn read(&mut self, rects: &[Rect]) -> Result<(), String> {
        let Session {
            conn,
            root,
            monitor,
            layout,
            grabber,
            picture,
            ..
        } = self;
        let (conn, root, origin, layout) = (&**conn, *root, monitor.rect, *layout);
        let row = |w: u32| row_bytes(w, layout.bits_per_pixel, layout.scanline_pad);
        let stride = monitor.rect.w as usize * 4;
        match grabber {
            Grabber::Shm {
                seg,
                file,
                capacity,
                scratch,
            } => {
                let mut rest = rects;
                while !rest.is_empty() {
                    // Pack as many rectangles as fit into the segment, one
                    // request each, all in flight before the first reply.
                    let mut batch = Vec::new();
                    let mut used = 0usize;
                    for r in rest {
                        let size = row(r.w) * r.h as usize;
                        if used + size > *capacity {
                            break;
                        }
                        batch.push((*r, used, size));
                        used += size;
                    }
                    if batch.is_empty() {
                        return Err(format!(
                            "a {}x{} region does not fit the {capacity}-byte segment",
                            rest[0].w, rest[0].h
                        ));
                    }
                    let mut cookies = Vec::with_capacity(batch.len());
                    for (r, offset, _) in &batch {
                        let (x, y, w, h) = request_rect(origin, r)?;
                        let offset = u32::try_from(*offset)
                            .map_err(|_| "segment offset out of range".to_string())?;
                        cookies.push(
                            conn.shm_get_image(
                                root,
                                x,
                                y,
                                w,
                                h,
                                !0,
                                ImageFormat::Z_PIXMAP.into(),
                                *seg,
                                offset,
                            )
                            .map_err(|e| format!("ShmGetImage: {e}"))?,
                        );
                    }
                    for ((_, _, size), cookie) in batch.iter().zip(cookies) {
                        let reply = cookie.reply().map_err(|e| format!("ShmGetImage: {e}"))?;
                        if reply.size as usize != *size {
                            return Err(format!(
                                "ShmGetImage wrote {} bytes, expected {size}",
                                reply.size
                            ));
                        }
                    }
                    scratch.resize(used, 0);
                    file.read_exact_at(&mut scratch[..used], 0)
                        .map_err(|e| format!("reading the shared segment: {e}"))?;
                    for (r, offset, size) in &batch {
                        if !copy_opaque(
                            &scratch[*offset..*offset + *size],
                            row(r.w),
                            picture,
                            stride,
                            *r,
                        ) {
                            return Err(format!("image for {r:?} does not fit the picture"));
                        }
                    }
                    rest = &rest[batch.len()..];
                }
            }
            Grabber::Core => {
                let mut cookies = Vec::with_capacity(rects.len());
                for r in rects {
                    let (x, y, w, h) = request_rect(origin, r)?;
                    cookies.push(
                        conn.get_image(ImageFormat::Z_PIXMAP, root, x, y, w, h, !0)
                            .map_err(|e| format!("GetImage: {e}"))?,
                    );
                }
                for (r, cookie) in rects.iter().zip(cookies) {
                    let reply = cookie.reply().map_err(|e| format!("GetImage: {e}"))?;
                    if !copy_opaque(&reply.data, row(r.w), picture, stride, *r) {
                        return Err(format!(
                            "GetImage returned {} bytes for {r:?}",
                            reply.data.len()
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// The cursor change since the last poll (`None` = nothing changed), in
    /// picture pixels. `shape_changed`: XFixes said the image changed.
    ///
    /// Position is the pointer's hotspot (`QueryPointer`), as on macOS; the
    /// shape's hotspot says where the image sits relative to it. Off this
    /// monitor, or with a fully transparent image, the cursor is reported
    /// invisible — the viewer then draws nothing rather than its default arrow.
    pub(super) fn poll_cursor(
        &mut self,
        shape_changed: bool,
    ) -> Result<Option<CursorUpdate>, String> {
        let image = if shape_changed || self.cursor.serial.is_none() {
            Some(
                self.conn
                    .xfixes_get_cursor_image()
                    .map_err(|e| format!("GetCursorImage: {e}"))?,
            )
        } else {
            None
        };
        let pointer = self
            .conn
            .query_pointer(self.root)
            .map_err(|e| format!("QueryPointer: {e}"))?;

        let mut shape = None;
        if let Some(cookie) = image {
            let img = cookie.reply().map_err(|e| format!("GetCursorImage: {e}"))?;
            if self.cursor.serial != Some(img.cursor_serial) {
                self.cursor.serial = Some(img.cursor_serial);
                match cursor_bgra(&img.cursor_image, img.width, img.height) {
                    Some(bgra) => {
                        self.cursor.blank = is_blank(&bgra);
                        if img.width <= MAX_CURSOR_SIDE && img.height <= MAX_CURSOR_SIDE {
                            shape = Some(CursorShape {
                                width: img.width as u32,
                                height: img.height as u32,
                                hotspot: Point {
                                    x: img.xhot as i32,
                                    y: img.yhot as i32,
                                },
                                bgra,
                            });
                        }
                    }
                    None => {
                        tracing::debug!(
                            "x11 capture: cursor image {}x{} came with {} pixels",
                            img.width,
                            img.height,
                            img.cursor_image.len()
                        );
                    }
                }
            }
        }

        let p = pointer.reply().map_err(|e| format!("QueryPointer: {e}"))?;
        let m = self.monitor.rect;
        let position = Point {
            x: p.root_x as i32 - m.x,
            y: p.root_y as i32 - m.y,
        };
        let inside = p.same_screen
            && position.x >= 0
            && position.y >= 0
            && (position.x as i64) < m.w as i64
            && (position.y as i64) < m.h as i64;
        let visible = inside && !self.cursor.blank;
        if self.cursor.last == Some((position, visible)) && shape.is_none() {
            return Ok(None);
        }
        self.cursor.last = Some((position, visible));
        Ok(Some(CursorUpdate {
            visible,
            position,
            shape,
        }))
    }

    /// Whether the screen configuration changed in a way that invalidates
    /// this session: the captured monitor moved, resized or vanished, or —
    /// when sharing "the primary display" — another monitor became primary.
    /// RandR also notifies when nothing relevant changed (and once right after
    /// `RRSelectInput`, by spec), so the geometry is compared, not assumed.
    pub(super) fn reconfigured(&self) -> Result<bool, String> {
        let monitors = monitors::query(&self.conn, self.root, self.randr_monitors)
            .map_err(|e| format!("listing monitors: {e}"))?;
        Ok(match monitors::pick(&monitors, Some(&self.monitor.id)) {
            Some(m) => m.rect != self.monitor.rect || (self.follows_primary && !m.primary),
            None => true,
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Wake the event thread out of its blocking read; it checks the flag
        // on every event. If the connection is already broken the thread has
        // seen the error and exited, so a failed send is fine.
        let wake = ClientMessageEvent::new(32, self.wake, AtomEnum::NONE, [0u32; 5]);
        let _ = self
            .conn
            .send_event(false, self.wake, EventMask::NO_EVENT, wake);
        let _ = self.conn.flush();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A picture-coordinate rectangle as the root-window request arguments.
fn request_rect(origin: Rect, r: &Rect) -> Result<(i16, i16, u16, u16), String> {
    let out = (
        i16::try_from(origin.x + r.x),
        i16::try_from(origin.y + r.y),
        u16::try_from(r.w),
        u16::try_from(r.h),
    );
    match out {
        (Ok(x), Ok(y), Ok(w), Ok(h)) => Ok((x, y, w, h)),
        _ => Err(format!("{r:?} is outside the X11 coordinate range")),
    }
}

/// MIT-SHM if the server can give us a segment, else the core path.
fn shm_grabber(conn: &RustConnection, display: &str, capacity: usize) -> Grabber {
    let attempt = || -> Result<Grabber, String> {
        if conn
            .extension_information(shm::X11_EXTENSION_NAME)
            .map_err(|e| e.to_string())?
            .is_none()
        {
            return Err("no MIT-SHM extension".into());
        }
        let v = conn
            .shm_query_version()
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        if (v.major_version, v.minor_version) < (1, 2) {
            return Err(format!(
                "MIT-SHM {}.{} cannot pass segments as fds (needs 1.2)",
                v.major_version, v.minor_version
            ));
        }
        let size =
            u32::try_from(capacity).map_err(|_| "screen too large for one segment".to_string())?;
        let seg = conn.generate_id().map_err(|e| e.to_string())?;
        let reply = conn
            .shm_create_segment(seg, size, false)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        Ok(Grabber::Shm {
            seg,
            file: File::from(reply.shm_fd),
            capacity,
            scratch: Vec::with_capacity(capacity),
        })
    };
    let server = display;
    attempt().unwrap_or_else(|why| {
        tracing::info!("x11 capture on {server}: MIT-SHM unavailable ({why}); using GetImage",);
        Grabber::Core
    })
}

fn xwayland_refusal() -> CaptureError {
    CaptureError::Backend(
        "this X server is Xwayland: it only shows X11 applications, not the Wayland desktop. \
         Screen sharing on Wayland goes through the desktop's screen-sharing portal."
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const BGRX: VisualFacts = VisualFacts {
        depth: 24,
        bits_per_pixel: 32,
        scanline_pad: 32,
        lsb_first: true,
        true_color: true,
        masks: (0xff0000, 0xff00, 0xff),
    };

    #[test]
    fn bgrx_visuals_are_accepted() {
        assert_eq!(
            check_layout(BGRX),
            Ok(Layout {
                bits_per_pixel: 32,
                scanline_pad: 32
            })
        );
        assert!(check_layout(VisualFacts { depth: 32, ..BGRX }).is_ok());
    }

    #[test]
    fn other_visuals_are_refused_with_the_reason() {
        let deep = check_layout(VisualFacts {
            depth: 30,
            masks: (0x3ff0_0000, 0xffc00, 0x3ff),
            ..BGRX
        });
        assert!(deep.is_err_and(|e| e.contains("depth 30") && e.contains("24-bit")));
        let rgb16 = VisualFacts {
            depth: 16,
            bits_per_pixel: 16,
            masks: (0xf800, 0x7e0, 0x1f),
            ..BGRX
        };
        assert!(check_layout(rgb16).is_err());
        let big_endian = check_layout(VisualFacts {
            lsb_first: false,
            ..BGRX
        });
        assert!(big_endian.is_err_and(|e| e.contains("big-endian")));
        let rgbx = VisualFacts {
            masks: (0xff, 0xff00, 0xff0000),
            ..BGRX
        };
        assert!(check_layout(rgbx).is_err());
        assert!(check_layout(VisualFacts {
            true_color: false,
            ..BGRX
        })
        .is_err());
    }

    #[test]
    fn request_rects_are_root_coordinates() {
        let origin = Rect {
            x: 1920,
            y: 0,
            w: 1280,
            h: 1024,
        };
        let r = Rect {
            x: 10,
            y: 20,
            w: 30,
            h: 40,
        };
        assert_eq!(request_rect(origin, &r), Ok((1930, 20, 30, 40)));
        let far = Rect { x: 40_000, ..r };
        assert!(request_rect(origin, &far).is_err());
    }
}
