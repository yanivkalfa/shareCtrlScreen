//! The `NSView` subclass that shows the remote screen: layer-hosting with a
//! `CAMetalLayer`, inserted as the top subview of the window's content view
//! (above the `WKWebView`), capturing mouse, wheel, keyboard and file drops.
//!
//! Everything here runs on the main thread (AppKit views are main-thread
//! only); state other threads need lives in the shared [`SurfaceState`].
//!
//! ## Toolbar reveal
//! The view's frame is shrunk from the top by the bar height, so the strip is
//! outside the view: AppKit hit-tests it to the web view underneath (the bar's
//! buttons work) and nothing of ours paints there. The Metal drawable keeps the
//! **full** surface size and the layer's `contentsGravity` pins it to the
//! bottom edge, so the picture is cropped, not moved or rescaled — and that
//! holds even when no new frame arrives (a static remote screen sends none).
//!
//! ## References
//! * `NSView`: <https://developer.apple.com/documentation/appkit/nsview>
//! * Layer-hosting vs layer-backed views (`setLayer:` before `setWantsLayer:`):
//!   <https://developer.apple.com/documentation/appkit/nsview/wantslayer>
//! * `CALayer.contentsGravity` ("bottom" = minimum Y in layer space):
//!   <https://developer.apple.com/documentation/quartzcore/calayer/contentsgravity>
//! * `contentsAreFlipped`: <https://developer.apple.com/documentation/quartzcore/calayer/contentsareflipped()>
//! * `NSTrackingArea` (`inVisibleRect`, `activeAlways`):
//!   <https://developer.apple.com/documentation/appkit/nstrackingarea>
//! * `NSEvent` (`keyCode`, `modifierFlags`, `scrollingDeltaY`,
//!   `hasPreciseScrollingDeltas`): <https://developer.apple.com/documentation/appkit/nsevent>
//! * Local event monitors: <https://developer.apple.com/documentation/appkit/nsevent/addlocalmonitorforevents(matching:handler:)>
//! * Key-up swallowed while Command is down (why keys use a local monitor):
//!   <https://developer.apple.com/documentation/appkit/nsapplication/sendevent(_:)>
//! * `NSDraggingDestination`: <https://developer.apple.com/documentation/appkit/nsdraggingdestination>
//! * `NSPasteboard.readObjects(forClasses:options:)` with
//!   `urlReadingFileURLsOnly`: <https://developer.apple.com/documentation/appkit/nspasteboard/readobjects(forclasses:options:)>
//! * `NSWindow.didResignKeyNotification`:
//!   <https://developer.apple.com/documentation/appkit/nswindow/didresignkeynotification>

use std::cell::Cell;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::Arc;

use block2::RcBlock;
use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{define_class, msg_send, sel, AnyThread, ClassType, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSDragOperation, NSDraggingDestination, NSDraggingInfo, NSEvent,
    NSEventMask, NSEventPhase, NSEventType, NSPasteboardTypeFileURL,
    NSPasteboardURLReadingFileURLsOnlyKey, NSResponder, NSTrackingArea, NSTrackingAreaOptions,
    NSView, NSWindow, NSWindowDidResignKeyNotification, NSWindowOrderingMode,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSDictionary, NSNotification, NSNotificationCenter, NSNumber,
    NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL,
};
use objc2_quartz_core::{kCAGravityBottom, kCAGravityTop, CAMetalLayer, CATransaction};
use platform_api::{FileDrop, SurfaceError};
use surface_wgpu::input::WheelAccumulator;
use surface_wgpu::reveal::{Reveal, RevealChange};
use surface_wgpu::SurfaceState;

use crate::keymap;

/// Windows wheel units per Cocoa scroll line (one classic mouse-wheel notch).
const UNITS_PER_LINE: f64 = 120.0;
/// Windows wheel units per pixel of precise (trackpad) scrolling: one notch
/// per 40 px, the pixel distance Chromium/WebKit scroll per Cocoa line, so a
/// trackpad swipe moves the remote about as far as it moves a local page.
const UNITS_PER_PIXEL: f64 = 3.0;

pub(crate) struct ViewIvars {
    state: Arc<SurfaceState>,
    layer: Retained<CAMetalLayer>,
    reveal: Cell<Reveal>,
    wheel: Cell<WheelAccumulator>,
}

define_class!(
    // SAFETY:
    // - NSView may be subclassed; we only override methods with their exact
    //   AppKit signatures and always forward to `super` where AppKit expects it.
    // - `VideoView` does not implement `Drop`; ivars drop normally.
    #[unsafe(super(NSView, NSResponder))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShareCtrlVideoView"]
    #[ivars = ViewIvars]
    pub(crate) struct VideoView;

    impl VideoView {
        /// Top-left origin, like the web page and every other OS surface, so
        /// pointer maths never has to flip.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        /// The first click on an inactive window reaches the remote too, the
        /// way it does on the Windows build.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        /// Focus moved to another view: its key-ups will never reach us.
        #[unsafe(method(resignFirstResponder))]
        fn resign_first_responder(&self) -> bool {
            self.ivars().state.input().release_all();
            // SAFETY: forwarding the same selector to NSResponder.
            unsafe { msg_send![super(self), resignFirstResponder] }
        }

        /// AppKit resizes us with the window (autoresizing); keep the size
        /// other threads read in step, without waiting for the next `fit`.
        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: NSSize) {
            // SAFETY: forwarding the same selector to NSView.
            let () = unsafe { msg_send![super(self), setFrameSize: size] };
            self.publish_metrics();
        }

        /// Moved to a display with another backing scale (Retina ↔ non-Retina).
        #[unsafe(method(viewDidChangeBackingProperties))]
        fn view_did_change_backing_properties(&self) {
            // SAFETY: forwarding the same selector to NSView.
            let () = unsafe { msg_send![super(self), viewDidChangeBackingProperties] };
            self.apply_layout();
        }

        #[unsafe(method(viewWillMoveToWindow:))]
        fn view_will_move_to_window(&self, new_window: Option<&NSWindow>) {
            if let Some(old) = self.window() {
                // SAFETY: `self` was registered for exactly this name/object.
                unsafe {
                    NSNotificationCenter::defaultCenter().removeObserver_name_object(
                        self,
                        Some(NSWindowDidResignKeyNotification),
                        Some(&old),
                    );
                }
            }
            if let Some(new) = new_window {
                // SAFETY: `windowDidResignKey:` is defined below with the
                // signature NSNotificationCenter calls (one NSNotification).
                unsafe {
                    NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
                        self,
                        sel!(windowDidResignKey:),
                        Some(NSWindowDidResignKeyNotification),
                        Some(new),
                    );
                }
            }
            // SAFETY: forwarding the same selector to NSView.
            let () = unsafe { msg_send![super(self), viewWillMoveToWindow: new_window] };
        }

        /// Cmd+Tab away or a click on another app: the first responder does
        /// not change, but key-ups now go to the other app.
        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            self.ivars().state.input().release_all();
        }

        /// Keys reach these only when the key monitor let them through (no
        /// session with control): they go nowhere, like on the Windows
        /// surface, instead of travelling up the responder chain to the
        /// "unhandled key" beep.
        #[unsafe(method(keyDown:))]
        fn key_down(&self, _event: &NSEvent) {}

        #[unsafe(method(keyUp:))]
        fn key_up(&self, _event: &NSEvent) {}

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let (px, py) = self.pointer_px(event);
            self.feed_reveal(py);
            let (nx, ny) = self.ivars().state.normalize(px, py);
            self.ivars().state.input().moved(nx, ny);
        }

        /// The pointer left the view. Leaving through the top edge counts as
        /// reaching it: a quick flick upwards can skip the top 2 px between
        /// two coalesced move events.
        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, event: &NSEvent) {
            let (_, py) = self.pointer_px(event);
            if py < 0.0 {
                self.feed_reveal(py);
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.drag(event);
        }

        #[unsafe(method(rightMouseDragged:))]
        fn right_mouse_dragged(&self, event: &NSEvent) {
            self.drag(event);
        }

        #[unsafe(method(otherMouseDragged:))]
        fn other_mouse_dragged(&self, event: &NSEvent) {
            self.drag(event);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.button(event, 0, true);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.button(event, 0, false);
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            self.button(event, 2, true);
        }

        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            self.button(event, 2, false);
        }

        /// AppKit numbers buttons 0 left, 1 right, 2 middle, 3+ extra; only
        /// the middle one exists in the protocol (DOM button 1).
        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, event: &NSEvent) {
            if event.buttonNumber() == 2 {
                self.button(event, 1, true);
            }
        }

        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, event: &NSEvent) {
            if event.buttonNumber() == 2 {
                self.button(event, 1, false);
            }
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let ivars = self.ivars();
            let mut wheel = ivars.wheel.get();
            if event.phase() == NSEventPhase::Began {
                // A new trackpad gesture: the previous one's remainder is stale.
                wheel.reset();
            }
            let per = if event.hasPreciseScrollingDeltas() {
                UNITS_PER_PIXEL
            } else {
                UNITS_PER_LINE
            };
            // Cocoa: +Y moves content down (scroll up), +X moves content right
            // (scroll left). Windows: +dy = wheel away = scroll up, +dx = scroll
            // right. Both deltas already include the user's natural-scrolling
            // choice, so the remote scrolls the way local pages do.
            let (dx, dy) = wheel.add(-event.scrollingDeltaX() * per, event.scrollingDeltaY() * per);
            ivars.wheel.set(wheel);
            ivars.state.input().wheel(dx, dy);
        }
    }

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for VideoView {}

    // SAFETY: the method signatures match the protocol.
    unsafe impl NSDraggingDestination for VideoView {
        #[unsafe(method(draggingEntered:))]
        fn dragging_entered(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            if self.ivars().state.accepts_drops() && has_file_urls(sender) {
                NSDragOperation::Copy
            } else {
                // No session with file transfer: refuse visibly instead of
                // letting the drop vanish.
                NSDragOperation::None
            }
        }

        #[unsafe(method(performDragOperation:))]
        fn perform_drag_operation(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> Bool {
            let paths = file_paths(sender);
            if paths.is_empty() {
                return Bool::NO;
            }
            // Where the user let go, so the host pastes into the window that
            // was aimed at (same as the Windows surface).
            let p = self.convertPoint_fromView(sender.draggingLocation(), None);
            let (px, py) = self.view_to_px(p);
            let (nx, ny) = self.ivars().state.normalize(px, py);
            Bool::new(self.ivars().state.deliver_drop(FileDrop { paths, nx, ny }))
        }
    }
);

fn file_url_options() -> Retained<NSDictionary<NSString, AnyObject>> {
    let yes = NSNumber::new_bool(true);
    // SAFETY: the key is an AppKit constant, valid for the process lifetime.
    let key = unsafe { NSPasteboardURLReadingFileURLsOnlyKey };
    let value: &AnyObject = &yes;
    NSDictionary::from_slices(&[key], &[value])
}

fn has_file_urls(sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
    let classes = NSArray::from_slice(&[NSURL::class()]);
    let options = file_url_options();
    // SAFETY: the class array holds NSURL (which implements
    // NSPasteboardReading) and the options dictionary uses a documented key
    // with an NSNumber value.
    unsafe {
        sender
            .draggingPasteboard()
            .canReadObjectForClasses_options(&classes, Some(&options))
    }
}

fn file_paths(sender: &ProtocolObject<dyn NSDraggingInfo>) -> Vec<PathBuf> {
    let classes = NSArray::from_slice(&[NSURL::class()]);
    let options = file_url_options();
    // SAFETY: as in `has_file_urls`.
    let objects = unsafe {
        sender
            .draggingPasteboard()
            .readObjectsForClasses_options(&classes, Some(&options))
    };
    let Some(objects) = objects else {
        return Vec::new();
    };
    objects
        .iter()
        .filter_map(|obj| obj.downcast::<NSURL>().ok())
        .filter(|url| url.isFileURL())
        .filter_map(|url| url.path())
        .map(|path| PathBuf::from(path.to_string()))
        .collect()
}

/// Run `f` with Core Animation's implicit animations off: a layer-hosting
/// view's layer would otherwise animate every frame/scale change over 0.25 s,
/// so the toolbar reveal would slide instead of snapping like on Windows.
fn without_animation<R>(f: impl FnOnce() -> R) -> R {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    let r = f();
    CATransaction::commit();
    r
}

impl VideoView {
    /// Create the view as the topmost subview of `parent`, hidden, filling it.
    pub(crate) fn create(
        parent: &NSView,
        state: Arc<SurfaceState>,
        mtm: MainThreadMarker,
    ) -> Result<(Retained<Self>, Retained<CAMetalLayer>), SurfaceError> {
        let layer = CAMetalLayer::new();
        // Low latency: present as soon as the GPU is done, outside the Core
        // Animation transaction. The presenter's surface configuration sets
        // the rest (`maximumDrawableCount = 2`, `displaySyncEnabled = false`).
        layer.setPresentsWithTransaction(false);
        layer.setOpaque(true);
        // Without this the drawable (full surface size) would paint over the
        // revealed toolbar strip, outside the shrunk bounds.
        layer.setMasksToBounds(true);
        let this = Self::alloc(mtm).set_ivars(ViewIvars {
            state,
            layer: layer.clone(),
            reveal: Cell::new(Reveal::default()),
            wheel: Cell::new(WheelAccumulator::default()),
        });
        // SAFETY: NSView's designated initializer.
        let view: Retained<Self> =
            unsafe { msg_send![super(this), initWithFrame: parent.bounds()] };
        // Layer-hosting: assign the layer *before* `wantsLayer`, so AppKit
        // uses ours as-is instead of creating and managing its own.
        view.setLayer(Some(&layer));
        view.setWantsLayer(true);
        view.setHidden(true);
        // Fixed margins: AppKit then tracks window resizes live and keeps the
        // revealed strip's inset while doing so.
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        let tracking = NSTrackingAreaOptions::MouseMoved
            | NSTrackingAreaOptions::MouseEnteredAndExited
            // Like the Windows child window: the remote sees the pointer even
            // while our window is not frontmost.
            | NSTrackingAreaOptions::ActiveAlways
            // Follows the visible rect, so the reveal needs no area update.
            | NSTrackingAreaOptions::InVisibleRect;
        // SAFETY: the owner is the view that adds the area; AppKit does not
        // retain the owner, and the area is removed with the view.
        let area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                NSRect::ZERO,
                tracking,
                Some(&view),
                None,
            )
        };
        view.addTrackingArea(&area);
        // SAFETY: the type constant is an AppKit static.
        let file_url = unsafe { NSPasteboardTypeFileURL };
        view.registerForDraggedTypes(&NSArray::from_slice(&[file_url]));
        parent.addSubview_positioned_relativeTo(&view, NSWindowOrderingMode::Above, None);
        view.apply_layout();
        Ok((view, layer))
    }

    /// Install the keyboard monitor. WHY a local monitor and not
    /// `keyDown:`/`keyUp:`: AppKit routes Command chords through the menu
    /// (`performKeyEquivalent:`, so Cmd+Q would quit us instead of reaching
    /// the remote) and `-[NSApplication sendEvent:]` drops the key-up of any
    /// key released while Command is held — the host would keep that key down.
    /// A local monitor sees every key event before `sendEvent:`.
    pub(crate) fn install_key_monitor(
        view: &Retained<Self>,
    ) -> Result<Retained<AnyObject>, SurfaceError> {
        let weak = Weak::from_retained(view);
        let handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: AppKit passes a valid event for the duration of the call.
            let ev = unsafe { event.as_ref() };
            match weak.load() {
                Some(view) if view.handle_key(ev) => std::ptr::null_mut(),
                _ => event.as_ptr(),
            }
        });
        let mask = NSEventMask::KeyDown | NSEventMask::KeyUp | NSEventMask::FlagsChanged;
        // SAFETY: the handler returns either the event it was given or null
        // (consumed), as the API requires.
        unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &handler) }.ok_or_else(
            || {
                SurfaceError::Backend(
                    "macOS refused the keyboard monitor for the remote screen; restart the app"
                        .into(),
                )
            },
        )
    }

    /// Keyboard for the remote. Consumes the event (returns `true`) only
    /// while it is meant for the remote: we are visible, focused in the key
    /// window, and a session with control has installed an input sink.
    /// Otherwise local shortcuts (Cmd+Q, Cmd+,) keep working.
    fn handle_key(&self, event: &NSEvent) -> bool {
        if self.isHidden() || !self.ivars().state.input().has_sink() {
            return false;
        }
        let Some(window) = self.window() else {
            return false;
        };
        let ours = event
            .window(self.mtm())
            .is_some_and(|w| Retained::as_ptr(&w) == Retained::as_ptr(&window));
        let focused = window
            .firstResponder()
            .is_some_and(|r| std::ptr::eq(Retained::as_ptr(&r), self.as_responder()));
        if !ours || !focused || !window.isKeyWindow() {
            return false;
        }
        let key_code = event.keyCode();
        let mut input = self.ivars().state.input();
        match event.r#type() {
            NSEventType::KeyDown | NSEventType::KeyUp => {
                // Unmapped keys (Fn) are consumed too: they mean nothing
                // locally while the remote has focus.
                if let Some(code) = keymap::dom_code(key_code) {
                    input.key(code, event.r#type() == NSEventType::KeyDown);
                }
            }
            NSEventType::FlagsChanged => {
                let Some(code) = keymap::dom_code(key_code) else {
                    return true;
                };
                if key_code == keymap::CAPS_LOCK {
                    // Each flagsChanged is one lock toggle: send it as a tap.
                    input.key(code, true);
                    input.key(code, false);
                } else if let Some(down) =
                    keymap::modifier_is_down(key_code, event.modifierFlags().0)
                {
                    input.key(code, down);
                }
            }
            _ => return false,
        }
        true
    }

    fn as_responder(&self) -> *const NSResponder {
        let responder: &NSResponder = self;
        responder
    }

    fn drag(&self, event: &NSEvent) {
        let (px, py) = self.pointer_px(event);
        let (nx, ny) = self.ivars().state.normalize(px, py);
        self.ivars().state.input().moved(nx, ny);
    }

    fn button(&self, event: &NSEvent, button: u8, down: bool) {
        if down {
            // Take keyboard focus so key events come to us (Windows: SetFocus).
            if let Some(window) = self.window() {
                window.makeFirstResponder(Some(self));
            }
        }
        let (px, py) = self.pointer_px(event);
        let (nx, ny) = self.ivars().state.normalize(px, py);
        self.ivars().state.input().button(button, down, nx, ny);
    }

    /// Event location in physical pixels of the **full** surface (top-left
    /// origin), whatever the current reveal inset.
    fn pointer_px(&self, event: &NSEvent) -> (f64, f64) {
        let p = self.convertPoint_fromView(event.locationInWindow(), None);
        self.view_to_px(p)
    }

    fn view_to_px(&self, p: NSPoint) -> (f64, f64) {
        let scale = self.ivars().state.scale();
        // Our own coordinates start below the revealed strip; the full
        // surface starts at the strip's top.
        let inset_pt = self.ivars().reveal.get().inset_px(scale) / scale;
        (p.x * scale, (p.y + inset_pt) * scale)
    }

    fn feed_reveal(&self, y_px: f64) {
        let mut reveal = self.ivars().reveal.get();
        let change = reveal.on_pointer(y_px, self.ivars().state.scale());
        self.ivars().reveal.set(reveal);
        if change.is_some() {
            self.apply_layout();
        }
    }

    /// Forget the reveal (show / hide) and restore the full frame if the bar
    /// was up.
    pub(crate) fn reset_reveal(&self) {
        let mut reveal = self.ivars().reveal.get();
        let change = reveal.reset();
        self.ivars().reveal.set(reveal);
        if change == Some(RevealChange::Hide) {
            self.apply_layout();
        }
    }

    /// Move to the top of the parent's subviews if something (a re-created
    /// web view) was added above us since — otherwise we would present,
    /// perfectly and invisibly, under the page (the Windows "airspace" bug).
    pub(crate) fn raise(&self) {
        // SAFETY: reading the superview has no preconditions.
        let Some(parent) = (unsafe { self.superview() }) else {
            return;
        };
        let on_top = parent
            .subviews()
            .lastObject()
            .is_some_and(|top| std::ptr::eq(Retained::as_ptr(&top), self.as_view()));
        if !on_top {
            parent.addSubview_positioned_relativeTo(self, NSWindowOrderingMode::Above, None);
        }
    }

    /// Give keyboard focus back to the page (the view below us) when hiding,
    /// so the Settings dialog's fields can be typed into straight away.
    pub(crate) fn yield_focus(&self) {
        let Some(window) = self.window() else {
            return;
        };
        let focused = window
            .firstResponder()
            .is_some_and(|r| std::ptr::eq(Retained::as_ptr(&r), self.as_responder()));
        if !focused {
            return;
        }
        // SAFETY: reading the superview has no preconditions.
        let below = unsafe { self.superview() }.and_then(|parent| {
            parent.subviews().to_vec().into_iter().rev().find(|v| {
                !std::ptr::eq(Retained::as_ptr(v), self.as_view()) && v.acceptsFirstResponder()
            })
        });
        match below {
            Some(view) => {
                window.makeFirstResponder(Some(&view));
            }
            None => {
                window.makeFirstResponder(None);
            }
        }
    }

    fn as_view(&self) -> *const NSView {
        let view: &NSView = self;
        view
    }

    /// Size the frame to the parent minus the revealed strip, keep the layer's
    /// scale and gravity right, and publish the full size.
    pub(crate) fn apply_layout(&self) {
        // SAFETY: reading the superview has no preconditions.
        let Some(parent) = (unsafe { self.superview() }) else {
            return;
        };
        let bounds = parent.bounds();
        let scale = self.backing_scale();
        let inset = (self.ivars().reveal.get().inset_px(scale) / scale).min(bounds.size.height);
        let origin_y = if parent.isFlipped() {
            // Flipped parent: y grows downwards, so the top strip is skipped
            // by moving the origin down.
            bounds.origin.y + inset
        } else {
            // Unflipped (AppKit default): the origin is the bottom edge,
            // which stays; only the height shrinks.
            bounds.origin.y
        };
        let frame = NSRect::new(
            NSPoint::new(bounds.origin.x, origin_y),
            NSSize::new(bounds.size.width, bounds.size.height - inset),
        );
        let layer = &self.ivars().layer;
        without_animation(|| {
            if self.frame() != frame {
                self.setFrame(frame);
            }
            layer.setContentsScale(scale);
            // "bottom" is the layer's minimum Y. Unflipped layer geometry
            // (the macOS default) puts that at the visual bottom; if anything
            // above us flips the geometry it is the visual top, so pick the
            // constant that pins the picture to the *visual* bottom edge.
            // SAFETY: Core Animation constants, valid for the process lifetime.
            let gravity = unsafe {
                if layer.contentsAreFlipped() {
                    kCAGravityTop
                } else {
                    kCAGravityBottom
                }
            };
            layer.setContentsGravity(gravity);
        });
        self.publish_metrics();
    }

    /// Publish the full surface size and scale for `client_size` and the
    /// presenter. Derived from the parent, not our (possibly shrunk) frame.
    fn publish_metrics(&self) {
        // SAFETY: reading the superview has no preconditions.
        let Some(parent) = (unsafe { self.superview() }) else {
            return;
        };
        let size = parent.bounds().size;
        let scale = self.backing_scale();
        let px = |pt: f64| (pt * scale).round().clamp(0.0, f64::from(u32::MAX)) as u32;
        let state = &self.ivars().state;
        state.set_scale(scale);
        state.set_size(px(size.width), px(size.height));
    }

    fn backing_scale(&self) -> f64 {
        self.window().map(|w| w.backingScaleFactor()).unwrap_or(1.0)
    }
}
