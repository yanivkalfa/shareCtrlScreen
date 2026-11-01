//! `ScreenCapture` on ScreenCaptureKit (plan task M1).
//!
//! `SCShareableContent` → `SCContentFilter(display)` → `SCStream` with an
//! output + delegate object on a private serial dispatch queue. The queue
//! pushes each complete frame into a depth-1 newest-wins [`FrameSlot`];
//! [`ScreenCapture::acquire`] waits on it, so the host keeps the pull model it
//! has on Windows. The cursor is not composited (`showsCursor = false`) and is
//! reported out-of-band from [`CursorTracker`], exactly like DXGI's pointer
//! metadata.
//!
//! ## References
//! * SCStreamConfiguration — `width`/`height`, `minimumFrameInterval`,
//!   `queueDepth` ("default three, don't exceed eight"), `pixelFormat`,
//!   `colorMatrix`, `showsCursor`, `scalesToFit`:
//!   <https://developer.apple.com/documentation/screencapturekit/scstreamconfiguration>
//! * SCStreamFrameInfo (`status`, `dirtyRects` — "CGRect in NSValue … in
//!   pixels"): <https://developer.apple.com/documentation/screencapturekit/scstreamframeinfo>
//! * SCFrameStatus (`complete`, `idle`, `blank`, `suspended`, `started`,
//!   `stopped`): <https://developer.apple.com/documentation/screencapturekit/scframestatus>
//! * SCStreamDelegate `stream(_:didStopWithError:)` and SCStreamError codes
//!   (`userDeclined` = -3801, `systemStoppedStream` = -3821):
//!   <https://developer.apple.com/documentation/screencapturekit/scstreamdelegate>,
//!   <https://developer.apple.com/documentation/screencapturekit/scstreamerror/code>
//! * WWDC22 10156 "Meet ScreenCaptureKit" and 10155 "Take ScreenCaptureKit to
//!   the next level" — surfaces come from a fixed pool of `queueDepth`; an app
//!   that holds them longer than `minimumFrameInterval × (queueDepth − 1)`
//!   stalls the stream; idle frames carry no image; dirty rects let encoders
//!   skip unchanged content: <https://developer.apple.com/videos/play/wwdc2022/10155/>
//! * CGPreflightScreenCaptureAccess / CGRequestScreenCaptureAccess (macOS 10.15+):
//!   <https://developer.apple.com/documentation/coregraphics/cgpreflightscreencaptureaccess()>
//! * plans/01-MACOS-FINDINGS.md §2–§3, §8; plans/04-TARGET-ARCHITECTURE.md §3, §7.

use std::any::Any;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{
    DispatchQoS, DispatchQueue, DispatchQueueAttr, DispatchRetained, GlobalQueueIdentifier,
};
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_foundation::{CFArray, CFDictionary, CGRect};
use objc2_core_graphics::{
    kCGColorSpaceSRGB, kCGDisplayStreamYCbCrMatrix_ITU_R_709_2, CGDirectDisplayID, CGMainDisplayID,
    CGPreflightScreenCaptureAccess, CGRectMakeWithDictionaryRepresentation,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSString, NSValue};
use objc2_screen_capture_kit::{
    SCContentFilter, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamErrorDomain, SCStreamFrameInfoDirtyRects, SCStreamFrameInfoStatus,
    SCStreamOutput, SCStreamOutputType,
};
use platform_api::{CaptureError, CapturedFrame, Frame, FrameData, ScreenCapture};

use crate::convert::{capture_size, classify_stop, dirty_rects, retry_delay, RectF};
use crate::cursor::CursorTracker;
use crate::displays;
use crate::gpu::CvPixelBufferFrame;
use crate::slot::{FrameSlot, StopReason, Wait};

/// ScreenCaptureKit caps full-display capture at 60 fps even on 120 Hz panels
/// (plans/01 §3), and the host never encodes faster than 60.
const MAX_FPS: i32 = 60;

/// Surfaces in the stream's pool. The capture can hold up to three at once —
/// the frame waiting in the slot, the frame being encoded, and the last
/// picture kept for cursor-only frames (see [`SckCapture::picture`]) — so the
/// default of three would leave ScreenCaptureKit nothing to render into. Five
/// leaves two free at all times; Apple says never more than eight.
const QUEUE_DEPTH: isize = 5;

/// How long to wait for ScreenCaptureKit's completion handlers. They normally
/// answer in milliseconds; this bounds a wedged `replayd` (the system capture
/// daemon) so the host thread can report an error instead of hanging.
const SCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest single wait on the frame slot inside `acquire`, so a pointer move
/// on a static screen is reported within one 60 Hz frame even if the caller
/// passes a long timeout.
const CURSOR_POLL: Duration = Duration::from_millis(16);

/// Whether this process may capture the screen right now.
pub(crate) fn screen_capture_granted() -> bool {
    CGPreflightScreenCaptureAccess()
}

// ---- errors from completion handlers -----------------------------------------

/// The parts of an `NSError` we act on, copied out on the callback's thread
/// (an `NSError` is not `Send`).
#[derive(Debug)]
struct NsErr {
    code: isize,
    sck_domain: bool,
    message: String,
}

impl NsErr {
    fn new(e: &NSError) -> Self {
        // SAFETY: `SCStreamErrorDomain` is an immutable framework constant.
        let sck = unsafe { SCStreamErrorDomain };
        Self {
            code: e.code(),
            sck_domain: &*e.domain() == sck,
            message: e.localizedDescription().to_string(),
        }
    }

    fn reason(&self) -> StopReason {
        classify_stop(self.code, self.sck_domain, screen_capture_granted())
    }

    fn into_capture_error(self, what: &str) -> CaptureError {
        match self.reason() {
            StopReason::PermissionDenied => CaptureError::PermissionDenied,
            StopReason::Lost => {
                CaptureError::Backend(format!("{what}: {} (code {})", self.message, self.code))
            }
        }
    }
}

fn timed_out(what: &str) -> CaptureError {
    CaptureError::Backend(format!(
        "{what}: ScreenCaptureKit did not answer within {} s (is replayd hung? \
         `killall replayd` restarts it)",
        SCK_TIMEOUT.as_secs()
    ))
}

/// Run an `SCStream` call that reports through a `completionHandler(NSError?)`
/// and wait for it.
fn wait_for_completion(
    what: &str,
    call: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>),
) -> Result<(), CaptureError> {
    let (tx, rx) = mpsc::sync_channel::<Option<NsErr>>(1);
    let block = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: ScreenCaptureKit passes nil or a valid NSError that lives for
        // the duration of the handler.
        let error = unsafe { error.as_ref() }.map(NsErr::new);
        // A second call (never expected) or a caller that timed out and left:
        // nothing to deliver to, and nothing to do about it.
        let _ = tx.try_send(error);
    });
    call(&block);
    match rx.recv_timeout(SCK_TIMEOUT) {
        Ok(None) => Ok(()),
        Ok(Some(e)) => Err(e.into_capture_error(what)),
        Err(_) => Err(timed_out(what)),
    }
}

/// `SCShareableContent` handed back from its completion handler's thread.
struct SendContent(Retained<SCShareableContent>);

// SAFETY: SCShareableContent is an immutable snapshot (Swift declares it
// `Sendable`); it is only read after the hand-off.
unsafe impl Send for SendContent {}

/// The displays and windows this process may capture. The first call ever is
/// what shows the Screen Recording prompt.
fn shareable_content() -> Result<Retained<SCShareableContent>, CaptureError> {
    const WHAT: &str = "list shareable content";
    let (tx, rx) = mpsc::sync_channel::<Result<SendContent, NsErr>>(1);
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: exactly one of the two is non-nil and valid for the
            // duration of the handler; `retain` takes our own reference.
            let result = match unsafe { Retained::retain(content) } {
                Some(c) => Ok(SendContent(c)),
                None => Err(unsafe { error.as_ref() }.map(NsErr::new).unwrap_or(NsErr {
                    code: 0,
                    sck_domain: false,
                    message: "no content and no error".into(),
                })),
            };
            let _ = tx.try_send(result);
        },
    );
    // Desktop/off-screen windows are irrelevant to a display filter; leaving
    // them out keeps the snapshot small.
    // SAFETY: the block lives until this function returns, and SCK copies it.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, true, &block,
        );
    }
    match rx.recv_timeout(SCK_TIMEOUT) {
        Ok(Ok(c)) => Ok(c.0),
        Ok(Err(e)) => Err(e.into_capture_error(WHAT)),
        Err(_) => Err(timed_out(WHAT)),
    }
}

// ---- the stream's output + delegate object -----------------------------------

/// A frame as delivered, before the capture thread looks at it.
struct Delivered {
    frame: CvPixelBufferFrame,
    dirty: Vec<RectF>,
}

struct SinkIvars {
    slot: Arc<FrameSlot<Delivered>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and the class adds no
    // `dealloc`. ScreenCaptureKit calls it on our serial queue only.
    #[unsafe(super(NSObject))]
    #[name = "ShareCtrlScreenStreamSink"]
    #[ivars = SinkIvars]
    struct StreamSink;

    unsafe impl NSObjectProtocol for StreamSink {}

    unsafe impl SCStreamOutput for StreamSink {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind == SCStreamOutputType::Screen {
                on_sample(&self.ivars().slot, sample);
            }
        }
    }

    unsafe impl SCStreamDelegate for StreamSink {
        #[unsafe(method(stream:didStopWithError:))]
        fn stream_did_stop(&self, _stream: &SCStream, error: &NSError) {
            let e = NsErr::new(error);
            let reason = e.reason();
            tracing::warn!(
                "sck: stream stopped: {} (code {}) → {reason:?}",
                e.message,
                e.code
            );
            self.ivars().slot.stop(reason);
        }
    }
);

impl StreamSink {
    fn new(slot: Arc<FrameSlot<Delivered>>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(SinkIvars { slot });
        // SAFETY: NSObject's designated initialiser on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    }
}

/// Read the per-frame info dictionary ScreenCaptureKit attaches to a sample.
fn frame_info(sample: &CMSampleBuffer) -> Option<Retained<NSDictionary<NSString, AnyObject>>> {
    // SAFETY: `sample` is valid for the duration of the callback.
    let attachments = unsafe { sample.sample_attachments_array(false) }?;
    // SAFETY: CFArray is toll-free bridged to NSArray; SCK documents the
    // elements as CFDictionaries keyed by SCStreamFrameInfo strings, which
    // are toll-free bridged to NSDictionary<NSString, id>.
    let array: &NSArray<NSDictionary<NSString, AnyObject>> = unsafe {
        &*(&*attachments as *const CFArray).cast::<NSArray<NSDictionary<NSString, AnyObject>>>()
    };
    array.firstObject()
}

/// One dirty rect: documented as `NSValue(CGRect)`; a CGRect dictionary (the
/// CoreFoundation encoding used by other per-frame keys) is accepted too so a
/// change of encoding degrades to "no damage info", never to a wrong rect.
fn dirty_rect(value: &AnyObject) -> Option<RectF> {
    if let Some(v) = value.downcast_ref::<NSValue>() {
        // `rectValue` on an NSValue of another type (an NSNumber is an NSValue
        // too) is undefined, so check the boxed type's encoding first.
        // SAFETY: `objCType` returns a NUL-terminated string owned by `v`.
        let ty = unsafe { std::ffi::CStr::from_ptr(v.objCType().as_ptr()) };
        if !ty.to_bytes().starts_with(b"{CGRect=") {
            return None;
        }
        // SAFETY: the value holds a CGRect (checked above).
        return Some(displays::rect_f(unsafe { v.rectValue() }));
    }
    let dict = value.downcast_ref::<NSDictionary>()?;
    let mut r = CGRect::default();
    // SAFETY: NSDictionary is toll-free bridged to CFDictionary; `r` is a
    // valid out pointer. Returns false if the dictionary is not a CGRect.
    let ok = unsafe {
        CGRectMakeWithDictionaryRepresentation(
            Some(&*(dict as *const NSDictionary).cast::<CFDictionary>()),
            &mut r,
        )
    };
    ok.then(|| displays::rect_f(r))
}

/// The delivery-queue side: classify the sample and hand complete frames to
/// the capture thread. Runs once per frame, so it only copies a few numbers.
fn on_sample(slot: &FrameSlot<Delivered>, sample: &CMSampleBuffer) {
    let Some(info) = frame_info(sample) else {
        return;
    };
    // SAFETY: framework constant.
    let status_key = unsafe { SCStreamFrameInfoStatus };
    let status = info
        .objectForKey(status_key)
        .and_then(|v| v.downcast::<NSNumber>().ok())
        .map(|n| SCFrameStatus(n.integerValue()));
    match status {
        Some(SCFrameStatus::Complete) => {}
        // The stream ended without (or before) a delegate callback.
        Some(SCFrameStatus::Stopped) => {
            slot.stop(StopReason::Lost);
            return;
        }
        // Idle (nothing changed — maps to `acquire() == Ok(None)`), Blank,
        // Suspended, Started: no new picture.
        _ => return,
    }
    // SAFETY: `sample` is valid for the duration of the callback; the buffer
    // is retained by `image_buffer`.
    let Some(image) = (unsafe { sample.image_buffer() }) else {
        return;
    };
    // SAFETY: framework constant.
    let dirty_key = unsafe { SCStreamFrameInfoDirtyRects };
    let dirty = info
        .objectForKey(dirty_key)
        .and_then(|v| v.downcast::<NSArray>().ok())
        .map(|a| a.iter().filter_map(|v| dirty_rect(&v)).collect())
        .unwrap_or_default();
    slot.put(Delivered {
        frame: CvPixelBufferFrame::new(image),
        dirty,
    });
}

// ---- one running stream ------------------------------------------------------

fn configuration(size: (u32, u32)) -> Retained<SCStreamConfiguration> {
    // SAFETY: plain property setters on a fresh configuration (all available
    // since macOS 12.3; the floor is 13). The two CFString constants are
    // framework statics, so "must be kept alive" holds for the process.
    unsafe {
        let c = SCStreamConfiguration::new();
        // The compositor scales to this size for free; it is what `dimensions`
        // reports and therefore what the encoder is built at.
        c.setWidth(size.0 as usize);
        c.setHeight(size.1 as usize);
        c.setScalesToFit(true);
        c.setMinimumFrameInterval(CMTime::new(1, MAX_FPS));
        c.setQueueDepth(QUEUE_DEPTH);
        // NV12 ("420v", video range) lets VideoToolbox encode without a colour
        // conversion. BT.709 is what the encoder signals and what every viewer
        // decoder assumes for HD content; sRGB output keeps P3 displays from
        // looking oversaturated on a viewer that cannot know the source gamut.
        c.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
        c.setColorMatrix(kCGDisplayStreamYCbCrMatrix_ITU_R_709_2);
        c.setColorSpaceName(kCGColorSpaceSRGB);
        // The cursor travels out-of-band (CursorTracker), as on Windows: a
        // pointer move must not cost a video frame.
        c.setShowsCursor(false);
        c
    }
}

struct Session {
    stream: Retained<SCStream>,
    sink: Retained<StreamSink>,
    slot: Arc<FrameSlot<Delivered>>,
    /// Kept alive for as long as the stream may call into it.
    _queue: DispatchRetained<DispatchQueue>,
}

impl Session {
    // Every entry point that talks to Objective-C from the host media thread
    // runs inside an autorelease pool: that thread is a plain std thread with
    // no pool of its own, so autoreleased temporaries would otherwise pile up
    // until the thread exits (i.e. for the whole session, every rebuild).
    fn start(display: CGDirectDisplayID, size: (u32, u32)) -> Result<Self, CaptureError> {
        autoreleasepool(|_| Self::start_in_pool(display, size))
    }

    fn start_in_pool(display: CGDirectDisplayID, size: (u32, u32)) -> Result<Self, CaptureError> {
        let content = shareable_content()?;
        // SAFETY: reading an immutable snapshot.
        let sc_display = unsafe { content.displays() }
            .iter()
            .find(|d| unsafe { d.displayID() } == display)
            .ok_or(CaptureError::NoOutput)?;
        // Whole display, nothing excluded: the remote user sees what the local
        // user sees, including our own window (AnyDesk does the same).
        // SAFETY: valid display and an empty array.
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &sc_display,
                &NSArray::new(),
            )
        };
        let config = configuration(size);
        // Each stream gets its own slot: callbacks still in flight from a
        // stream being torn down land in its orphaned slot, never in the new one.
        let slot = Arc::new(FrameSlot::new());
        let sink = StreamSink::new(Arc::clone(&slot));
        // SAFETY: valid filter, configuration and delegate; SCK keeps a weak
        // reference to the delegate, which `Session` keeps alive.
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*sink)),
            )
        };
        // Serial (frames in order, one callback at a time) and user-interactive
        // (frame delivery is on the latency path of every remote keystroke).
        let target = DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(
            DispatchQoS::UserInteractive,
        ));
        let queue = DispatchQueue::new_with_target(
            "dev.netameta.sharectrl.capture",
            DispatchQueueAttr::SERIAL,
            Some(&target),
        );
        // SAFETY: the sink and queue outlive the stream (both owned by Session).
        unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*sink),
                SCStreamOutputType::Screen,
                Some(&queue),
            )
        }
        .map_err(|e| NsErr::new(&e).into_capture_error("add stream output"))?;
        // SAFETY: the block outlives the call; SCK copies it.
        wait_for_completion("start capture", |done| unsafe {
            stream.startCaptureWithCompletionHandler(Some(done));
        })?;
        Ok(Self {
            stream,
            sink,
            slot,
            _queue: queue,
        })
    }

    /// Change the output size of the running stream in place (no restart, no
    /// new permission check).
    fn reconfigure(&self, size: (u32, u32)) -> Result<(), CaptureError> {
        autoreleasepool(|_| {
            let config = configuration(size);
            // SAFETY: valid configuration; the block outlives the call.
            wait_for_completion("update configuration", |done| unsafe {
                self.stream
                    .updateConfiguration_completionHandler(&config, Some(done));
            })
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        autoreleasepool(|_| self.stop());
    }
}

impl Session {
    fn stop(&self) {
        let stats = self.slot.stats();
        tracing::info!(
            "sck: stream closed — {} frames delivered, {} superseded before the encoder took them",
            stats.delivered,
            stats.superseded
        );
        // Stop and wait, so no callback runs after the sink's last owner is
        // gone. A stream the system already stopped answers with an error
        // (`attemptToStopStreamState`); that is the expected outcome then.
        // SAFETY: the block outlives the call.
        if let Err(e) = wait_for_completion("stop capture", |done| unsafe {
            self.stream.stopCaptureWithCompletionHandler(Some(done));
        }) {
            tracing::debug!("sck: {e}");
        }
        // SAFETY: the sink was added as the screen output in `start`.
        if let Err(e) = unsafe {
            self.stream.removeStreamOutput_type_error(
                ProtocolObject::from_ref(&*self.sink),
                SCStreamOutputType::Screen,
            )
        } {
            tracing::debug!("sck: remove stream output: {}", NsErr::new(&e).message);
        }
    }
}

// ---- the ScreenCapture -------------------------------------------------------

pub struct SckCapture {
    /// `DisplayInfo::id` to share (`None` = main display). Re-resolved on
    /// every rebuild because the `CGDirectDisplayID` can change across a
    /// sleep or re-plug.
    share_id: Option<String>,
    display: CGDirectDisplayID,
    /// The viewer's requested size (`request_output_size`), if any.
    requested: Option<(u32, u32)>,
    /// Configured capture size = `dimensions()`.
    size: (u32, u32),
    /// The newest complete picture. Kept after `release` while nothing newer
    /// has arrived, because a cursor-only frame must still carry a valid
    /// picture: the host encodes it when a keyframe is due on a static screen
    /// (DXGI hands out the unchanged desktop in that case too). Holding one
    /// surface is accounted for in [`QUEUE_DEPTH`]. Declared before `session`
    /// so it is released before the stream stops.
    picture: Option<CvPixelBufferFrame>,
    session: Option<Session>,
    /// Why the last session ended, while `session` is `None`.
    dead: StopReason,
    /// Consecutive failed rebuilds, and when the next one may run.
    failures: u32,
    retry_at: Instant,
    cursor: CursorTracker,
    /// Size of the last mismatched frame warned about (log once per size).
    warned_size: Option<(u32, u32)>,
}

fn resolve_display(share_id: Option<&str>) -> Result<CGDirectDisplayID, CaptureError> {
    match share_id {
        None => Ok(CGMainDisplayID()),
        Some(id) => displays::resolve(id).ok_or(CaptureError::NoOutput),
    }
}

impl SckCapture {
    pub(crate) fn new(share_id: Option<&str>) -> Result<Self, CaptureError> {
        if let Some(id) = share_id {
            if !crate::display_id::is_mac_id(id) {
                tracing::warn!("display id {id:?} was not saved by the macOS build");
                return Err(CaptureError::NoOutput);
            }
        }
        let display = resolve_display(share_id)?;
        let native = displays::native_pixels(display).ok_or(CaptureError::NoOutput)?;
        let size = capture_size(native, None);
        let session = Session::start(display, size)?;
        // (`display` is a tracing keyword, so the log names it differently.)
        let cg_id = display;
        tracing::info!(
            "sck: capturing {} (CGDirectDisplayID {cg_id}) at {}x{} (native {}x{}), 420v, ≤{MAX_FPS} fps",
            share_id.unwrap_or("main display"),
            size.0,
            size.1,
            native.0,
            native.1
        );
        Ok(Self {
            share_id: share_id.map(str::to_owned),
            display,
            requested: None,
            size,
            picture: None,
            session: Some(session),
            dead: StopReason::Lost,
            failures: 0,
            retry_at: Instant::now(),
            cursor: CursorTracker::new(display),
            warned_size: None,
        })
    }

    fn end_session(&mut self, reason: StopReason) {
        self.picture = None;
        self.session = None;
        self.dead = reason;
        self.retry_at = Instant::now();
    }

    /// A borrowed frame over [`Self::picture`].
    fn lend(
        &self,
        dirty: Vec<platform_api::Rect>,
        pointer_only: bool,
        cursor: Option<platform_api::CursorUpdate>,
    ) -> Option<CapturedFrame<'_>> {
        let picture: &dyn Any = self.picture.as_ref()?;
        Some(CapturedFrame {
            frame: Frame {
                width: self.size.0,
                height: self.size.1,
                data: FrameData::Native(picture),
            },
            dirty,
            moves: Vec::new(),
            pointer_only,
            cursor,
        })
    }

    /// `acquire` while no stream is running: report the loss again once the
    /// back-off has elapsed (the host then calls `reinit`), idle until then.
    fn acquire_while_down(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<CapturedFrame<'_>>, CaptureError> {
        if self.dead == StopReason::PermissionDenied {
            return Err(CaptureError::PermissionDenied);
        }
        let now = Instant::now();
        if now >= self.retry_at {
            return Err(CaptureError::Lost);
        }
        std::thread::sleep(timeout.min(self.retry_at - now));
        Ok(None)
    }

    fn rebuild(&mut self) -> Result<(), CaptureError> {
        let display = resolve_display(self.share_id.as_deref())?;
        let native = displays::native_pixels(display).ok_or(CaptureError::NoOutput)?;
        let size = capture_size(native, self.requested);
        let session = Session::start(display, size)?;
        if display != self.display {
            self.cursor = CursorTracker::new(display);
        } else {
            self.cursor.reset();
        }
        self.display = display;
        self.size = size;
        self.session = Some(session);
        self.warned_size = None;
        Ok(())
    }
}

impl ScreenCapture for SckCapture {
    fn dimensions(&self) -> (u32, u32) {
        self.size
    }

    fn acquire(&mut self, timeout: Duration) -> Result<Option<CapturedFrame<'_>>, CaptureError> {
        let Some(slot) = self.session.as_ref().map(|s| Arc::clone(&s.slot)) else {
            return self.acquire_while_down(timeout);
        };
        let start = Instant::now();
        loop {
            let left = timeout.saturating_sub(start.elapsed());
            match slot.wait(left.min(CURSOR_POLL)) {
                Wait::Frame(d) => {
                    let got = d.frame.size();
                    if got != self.size {
                        // A frame rendered before a reconfiguration took
                        // effect. Encoding it would feed the encoder a size
                        // it was not built for (invariant 1), so skip it.
                        if self.warned_size != Some(got) {
                            self.warned_size = Some(got);
                            tracing::warn!(
                                "sck: dropping {}x{} frame (capture is {}x{})",
                                got.0,
                                got.1,
                                self.size.0,
                                self.size.1
                            );
                        }
                        continue;
                    }
                    self.picture = Some(d.frame);
                    let dirty = dirty_rects(&d.dirty, self.size);
                    let cursor = self.cursor.poll(self.size);
                    return Ok(self.lend(dirty, false, cursor));
                }
                Wait::Stopped(reason) => {
                    self.end_session(reason);
                    return Err(match reason {
                        StopReason::Lost => CaptureError::Lost,
                        StopReason::PermissionDenied => CaptureError::PermissionDenied,
                    });
                }
                Wait::Timeout => {
                    // Cursor-only frames need a picture to point at; before the
                    // first frame there is nothing to report against.
                    if self.picture.is_some() {
                        if let Some(cursor) = self.cursor.poll(self.size) {
                            return Ok(self.lend(Vec::new(), true, Some(cursor)));
                        }
                    }
                    if start.elapsed() >= timeout {
                        return Ok(None);
                    }
                }
            }
        }
    }

    fn release(&mut self) {
        // The borrow handed out by `acquire` ends here. The picture itself is
        // returned to ScreenCaptureKit's pool as soon as a newer frame is
        // waiting — exactly when the stream is producing and surfaces matter;
        // on a static screen it is kept for cursor-only frames.
        if self.session.as_ref().is_some_and(|s| s.slot.has_pending()) {
            self.picture = None;
        }
    }

    fn reinit(&mut self) -> Result<(), CaptureError> {
        self.picture = None;
        self.session = None;
        match self.rebuild() {
            Ok(()) => {
                tracing::info!(
                    "sck: stream rebuilt at {}x{} after {} failed attempt(s)",
                    self.size.0,
                    self.size.1,
                    self.failures
                );
                self.failures = 0;
                Ok(())
            }
            Err(e) => {
                self.failures += 1;
                self.dead = match e {
                    CaptureError::PermissionDenied => StopReason::PermissionDenied,
                    _ => StopReason::Lost,
                };
                // Display sleep can last all night: back off so the host's
                // retry loop does not hammer replayd (and the log) at 4 Hz.
                self.retry_at = Instant::now() + retry_delay(self.failures);
                Err(e)
            }
        }
    }

    fn device(&self) -> Option<&dyn Any> {
        // VideoToolbox needs no shared device for IOSurface-backed buffers
        // (see gpu.rs).
        None
    }

    fn request_output_size(&mut self, size: (u32, u32)) -> bool {
        let requested = (size.0 > 0 && size.1 > 0).then_some(size);
        let Some(native) = displays::native_pixels(self.display) else {
            return false;
        };
        let target = capture_size(native, requested);
        if target == self.size {
            self.requested = requested;
            return true;
        }
        if let Some(session) = &self.session {
            if let Err(e) = session.reconfigure(target) {
                tracing::warn!("sck: resize to {}x{} failed: {e}", target.0, target.1);
                return false;
            }
            // The kept picture has the old size and can no longer be lent.
            self.picture = None;
        }
        tracing::info!(
            "sck: capture size {}x{} → {}x{}",
            self.size.0,
            self.size.1,
            target.0,
            target.1
        );
        self.requested = requested;
        self.size = target;
        true
    }
}
