//! The pointer, sent out-of-band like on Windows: the stream is configured with
//! `showsCursor = false`, and the host reports position and shape so the viewer
//! draws the sprite itself (a pointer move then costs no video frame).
//!
//! Position: `NSEvent.mouseLocation`. Shape: `NSCursor.currentSystemCursor`
//! rendered to straight-alpha BGRA. Both are polled on the capture thread.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSCursor, NSEvent};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    kCGColorSpaceSRGB, CGBitmapContextCreate, CGColorSpace, CGContext, CGDirectDisplayID,
    CGImageAlphaInfo, CGImageByteOrderInfo, CGInterpolationQuality, CGMainDisplayID,
};
use objc2_foundation::NSRect;
use platform_api::{CursorShape, CursorUpdate, Point};

use crate::convert::{cursor_in_frame, cursor_pixels, hotspot_pixels, unpremultiply_bgra};
use crate::displays;

/// How often the shape is re-rendered and compared. Rendering is cheap (a
/// 32–64 px bitmap) but not free, and the position poll runs at frame rate;
/// 20 Hz keeps a shape change (I-beam over text, resize arrows) visually
/// immediate while staying far under the capture thread's 1 % CPU budget.
const SHAPE_POLL: Duration = Duration::from_millis(50);

pub(crate) struct CursorTracker {
    display: CGDirectDisplayID,
    /// Last reported position + visibility (`None` = nothing reported yet).
    last: Option<(Point, bool)>,
    /// Fingerprint of the last reported shape.
    last_shape: Option<u64>,
    next_shape_poll: Instant,
}

impl CursorTracker {
    pub(crate) fn new(display: CGDirectDisplayID) -> Self {
        Self {
            display,
            last: None,
            last_shape: None,
            next_shape_poll: Instant::now(),
        }
    }

    /// Forget what was reported, so the next poll re-sends everything (after a
    /// stream rebuild the host forces a keyframe and the viewer may have
    /// restarted its cursor state).
    pub(crate) fn reset(&mut self) {
        self.last = None;
        self.last_shape = None;
        self.next_shape_poll = Instant::now();
    }

    /// The cursor change since the last call, in pixels of a `frame`-sized
    /// capture of this display; `None` if nothing changed (the contract: report
    /// only on change).
    pub(crate) fn poll(&mut self, frame: (u32, u32)) -> Option<CursorUpdate> {
        // The capture thread is a plain std thread without an autorelease
        // pool and this runs every frame, so AppKit's temporaries (the cursor
        // image and its CGImage) must be drained here, not at thread exit.
        autoreleasepool(|_| self.poll_in_pool(frame))
    }

    fn poll_in_pool(&mut self, frame: (u32, u32)) -> Option<CursorUpdate> {
        let display = displays::bounds(self.display);
        let main_height = displays::bounds(CGMainDisplayID()).h;
        let mouse = NSEvent::mouseLocation();
        let (position, visible) = cursor_in_frame((mouse.x, mouse.y), main_height, display, frame);

        let shape = if Instant::now() >= self.next_shape_poll {
            self.next_shape_poll = Instant::now() + SHAPE_POLL;
            let scale = if display.w > 0.0 {
                frame.0 as f64 / display.w
            } else {
                1.0
            };
            current_shape(scale).filter(|s| {
                let print = fingerprint(s);
                let changed = self.last_shape != Some(print);
                self.last_shape = Some(print);
                changed
            })
        } else {
            None
        };

        let moved = self.last != Some((position, visible));
        if !moved && shape.is_none() {
            return None;
        }
        self.last = Some((position, visible));
        Some(CursorUpdate {
            visible,
            position,
            shape,
        })
    }
}

fn fingerprint(s: &CursorShape) -> u64 {
    let mut h = DefaultHasher::new();
    s.width.hash(&mut h);
    s.height.hash(&mut h);
    s.hotspot.x.hash(&mut h);
    s.hotspot.y.hash(&mut h);
    s.bgra.hash(&mut h);
    h.finish()
}

/// The system-wide cursor rendered at `scale` frame pixels per point.
///
/// `NSCursor.currentSystemCursor` is deprecated in the macOS 15 SDK ("will
/// always be nil in a future version") but is still the only public API that
/// returns *another* app's cursor image. When it returns nil the shape is
/// simply not reported and the viewer keeps drawing its default arrow;
/// position tracking is unaffected.
fn current_shape(scale: f64) -> Option<CursorShape> {
    #[allow(deprecated)]
    let cursor = NSCursor::currentSystemCursor()?;
    let image = cursor.image();
    let points = image.size();
    let hot = cursor.hotSpot();
    let (w, h) = cursor_pixels((points.width, points.height), scale);
    let mut rect = NSRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64));
    // SAFETY: `rect` is a valid, live NSRect; nil context and hints ask AppKit
    // for the representation best matching the rect's size.
    let cg = unsafe { image.CGImageForProposedRect_context_hints(&mut rect, None, None) }?;

    let stride = w as usize * 4;
    let mut bgra = vec![0u8; stride * h as usize];
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
    // B,G,R,A byte order in memory = 32-bit little-endian, alpha first.
    let info = CGImageAlphaInfo::PremultipliedFirst.0 | CGImageByteOrderInfo::Order32Little.0;
    // SAFETY: `bgra` outlives `ctx` (dropped at the end of this function) and
    // is exactly `stride * h` bytes, matching the dimensions passed.
    let ctx = unsafe {
        CGBitmapContextCreate(
            bgra.as_mut_ptr().cast(),
            w as usize,
            h as usize,
            8,
            stride,
            Some(&space),
            info,
        )
    }?;
    let full = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64));
    CGContext::set_interpolation_quality(Some(&ctx), CGInterpolationQuality::High);
    CGContext::clear_rect(Some(&ctx), full);
    CGContext::draw_image(Some(&ctx), full, Some(&cg));
    drop(ctx);

    unpremultiply_bgra(&mut bgra);
    Some(CursorShape {
        width: w,
        height: h,
        hotspot: hotspot_pixels((hot.x, hot.y), scale, (w, h)),
        bgra,
    })
}
