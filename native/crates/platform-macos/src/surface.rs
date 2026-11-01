//! `impl VideoSurface` for macOS: owns the [`VideoView`] (main-thread only)
//! and the shared state every other thread uses.
//!
//! ## Threading
//! The trait's methods may be called from any thread; AppKit work must run on
//! the main thread.
//!
//! * Called **on** the main thread (Tauri runs synchronous commands there, so
//!   the UI's `set_video_visible(false)` → `hide` lands here): the work runs
//!   immediately, so `hide` has taken effect before the UI paints its modal.
//! * Called **off** the main thread (`show` from the viewer media thread,
//!   `hide`/`show` from the transport thread on link trouble): the work is
//!   queued on the main queue and waited for **with a bound**. WHY bounded:
//!   `Engine::end_session` runs on the main thread and joins the session
//!   threads; an unbounded synchronous hop from one of those threads would
//!   deadlock the whole app. If the main thread does not answer in time the
//!   work still runs, just later.
//! * `fit` never waits (it is a 100 ms poll from the render loop), and
//!   `client_size` and the sinks never touch AppKit at all.
//!
//! ## References
//! * `DispatchQueue::main().exec_async`, `MainThreadBound`:
//!   <https://docs.rs/dispatch2/0.3.1/dispatch2/>
//! * Tauri: synchronous commands run on the main thread:
//!   <https://v2.tauri.app/develop/calling-rust/#async-commands>
//! * `CATransaction.flush`: <https://developer.apple.com/documentation/quartzcore/catransaction/flush()>
//! * `NSEvent.removeMonitor`: <https://developer.apple.com/documentation/appkit/nsevent/removemonitor(_:)>
//! * raw-window-handle `AppKitWindowHandle` (`ns_view` = the window's
//!   content view, as tao/Tauri hand it out):
//!   <https://docs.rs/raw-window-handle/0.6/raw_window_handle/struct.AppKitWindowHandle.html>

use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use dispatch2::{DispatchQueue, MainThreadBound};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSEvent, NSView};
use objc2_quartz_core::CATransaction;
use platform_api::{FileDrop, Presenter, RawWindowHandle, SurfaceError, VideoInput, VideoSurface};
use surface_wgpu::SurfaceState;

use crate::presenter::{MetalPresenter, SharedLayer};
use crate::view::VideoView;

/// How long an off-main caller waits for the main thread. Long enough for a
/// busy-but-alive UI (a layout pass, a resize), short enough that a main
/// thread blocked on joining the caller costs a barely noticeable pause.
const MAIN_WAIT: Duration = Duration::from_millis(500);

/// Main-thread-only parts of the surface.
struct MainParts {
    view: Retained<VideoView>,
    /// The keyboard monitor token from `addLocalMonitorForEvents`.
    monitor: Retained<AnyObject>,
}

type Main = Arc<MainThreadBound<MainParts>>;

pub(crate) struct MacVideoSurface {
    /// `Option` only so `Drop` can move it to the main thread.
    main: Option<Main>,
    state: Arc<SurfaceState>,
    layer: SharedLayer,
    /// Coalesces `fit` polls: at most one main-thread layout pass queued.
    fit_queued: Arc<AtomicBool>,
}

impl MacVideoSurface {
    /// Create the surface on the main thread under `window`'s content view.
    pub(crate) fn create(window: RawWindowHandle) -> Result<Arc<Self>, SurfaceError> {
        let RawWindowHandle::AppKit(handle) = window else {
            return Err(SurfaceError::UnsupportedHandle);
        };
        let mtm = MainThreadMarker::new().ok_or_else(|| {
            SurfaceError::Backend("the video view must be created on the main (UI) thread".into())
        })?;
        // SAFETY: an AppKit window handle's `ns_view` is a valid NSView for as
        // long as the window exists (raw-window-handle contract), and the app's
        // main window outlives the engine.
        let parent: &NSView = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
        let state = Arc::new(SurfaceState::new());
        let (view, layer) = VideoView::create(parent, state.clone(), mtm)?;
        let monitor = match VideoView::install_key_monitor(&view) {
            Ok(m) => m,
            Err(e) => {
                view.removeFromSuperview();
                return Err(e);
            }
        };
        let (w, h) = state.size();
        tracing::info!("video view: created ({w}x{h} px)");
        Ok(Arc::new(Self {
            main: Some(Arc::new(MainThreadBound::new(
                MainParts { view, monitor },
                mtm,
            ))),
            state,
            layer: SharedLayer::new(layer),
            fit_queued: Arc::new(AtomicBool::new(false)),
        }))
    }

    /// Run `f` on the main thread: at once when already there, otherwise
    /// queued and waited for up to [`MAIN_WAIT`] (see the module docs).
    fn on_main(&self, what: &'static str, f: impl FnOnce(&VideoView) + Send + 'static) {
        let Some(main) = &self.main else {
            return;
        };
        if let Some(mtm) = MainThreadMarker::new() {
            f(&main.get(mtm).view);
            return;
        }
        let main = main.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        DispatchQueue::main().exec_async(move || {
            // SAFETY: blocks submitted to the main queue run on the main thread.
            let mtm = unsafe { MainThreadMarker::new_unchecked() };
            f(&main.get(mtm).view);
            // The waiter may have given up already; nothing to do then.
            let _ = done_tx.send(());
        });
        if done_rx.recv_timeout(MAIN_WAIT).is_err() {
            tracing::warn!("video view: main thread busy, {what} will apply late");
        }
    }
}

impl VideoSurface for MacVideoSurface {
    fn show(&self) {
        self.on_main("show", |view| {
            view.reset_reveal();
            view.raise();
            view.apply_layout();
            view.setHidden(false);
        });
    }

    fn hide(&self) {
        self.on_main("hide", |view| {
            view.setHidden(true);
            // The UI paints its modal right after this returns: push the
            // change to the window server now rather than at the end of this
            // run-loop turn, so the dialog never appears behind the video.
            CATransaction::flush();
            view.yield_focus();
            view.reset_reveal();
        });
        self.state.clear_video_size();
    }

    fn fit(&self) {
        // AppKit autoresizing already tracks live resizes; this pass only
        // repairs drift (a parent that does not autoresize, a scale change
        // missed while hidden). Never block the render loop on it.
        if self.fit_queued.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(main) = self.main.clone() else {
            return;
        };
        let queued = self.fit_queued.clone();
        DispatchQueue::main().exec_async(move || {
            // SAFETY: blocks submitted to the main queue run on the main thread.
            let mtm = unsafe { MainThreadMarker::new_unchecked() };
            main.get(mtm).view.apply_layout();
            queued.store(false, Ordering::Release);
        });
    }

    fn client_size(&self) -> (u32, u32) {
        self.state.size()
    }

    fn set_input_sink(&self, sink: Option<Sender<VideoInput>>) {
        self.state.input().set_sink(sink);
    }

    fn set_drop_sink(&self, sink: Option<Sender<FileDrop>>) {
        self.state.set_drop_sink(sink);
    }

    fn create_presenter(&self) -> Result<Box<dyn Presenter>, SurfaceError> {
        Ok(Box::new(MetalPresenter::new(
            &self.layer,
            self.state.clone(),
        )?))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Drop for MacVideoSurface {
    fn drop(&mut self) {
        // Release anything still held so the host is not left with a stuck key.
        self.state.input().set_sink(None);
        let Some(main) = self.main.take() else {
            return;
        };
        let teardown = move |mtm: MainThreadMarker| {
            let parts = main.get(mtm);
            // SAFETY: the token came from `addLocalMonitorForEvents` and is
            // removed exactly once.
            unsafe { NSEvent::removeMonitor(&parts.monitor) };
            parts.view.removeFromSuperview();
        };
        match MainThreadMarker::new() {
            Some(mtm) => teardown(mtm),
            // Asynchronous on purpose: at process exit the main run loop may
            // already be gone, and a synchronous hop would hang the exit. If it
            // never runs, the process is ending and AppKit cleans up anyway.
            None => DispatchQueue::main().exec_async(move || {
                // SAFETY: blocks submitted to the main queue run on the main thread.
                teardown(unsafe { MainThreadMarker::new_unchecked() });
            }),
        }
    }
}
