//! State a video surface shares between its UI thread (events, layout), the
//! viewer media thread (presenter) and whichever thread calls the
//! [`platform_api::VideoSurface`] methods. Everything here is lock-free or
//! behind a short mutex, so the UI thread never waits on the render thread.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, MutexGuard, TryLockError};

use platform_api::FileDrop;

use crate::input::InputState;

/// Shared surface state. One per surface, behind an `Arc`.
#[derive(Debug, Default)]
pub struct SurfaceState {
    /// Full surface size in physical pixels, `w << 32 | h`. Written by the UI
    /// thread whenever the layout or backing scale changes; it is what
    /// `client_size` reports and what the presenter sizes its drawable to.
    /// Never reduced by the toolbar reveal (the host's encode size must not
    /// change while the bar is shown).
    size: AtomicU64,
    /// Backing scale factor (`f64` bits; 0 = not known yet → 1.0).
    scale: AtomicU64,
    /// Size of the last presented frame, `w << 32 | h` (0 = none yet). Input
    /// normalization uses it so clicks map over the letterbox rectangle.
    video: AtomicU64,
    /// The surface has the keyboard: visible, and first responder (or the
    /// platform's equivalent) in the key / foreground window. Written by the
    /// UI thread on every focus change; read by the viewer's shortcut hook,
    /// which runs on its own thread inside an OS input callback and must never
    /// wait for the UI thread — hence an atomic, not a windowing-system query.
    key_focus: AtomicBool,
    input: Mutex<InputState>,
    drops: Mutex<Option<Sender<FileDrop>>>,
}

fn pack(w: u32, h: u32) -> u64 {
    (u64::from(w) << 32) | u64::from(h)
}

fn unpack(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

/// Lock, recovering from a poisoned mutex: the guarded data is plain input
/// bookkeeping that stays valid even if a panic interrupted a holder, and
/// refusing to release held keys after a panic would be worse.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl SurfaceState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_size(&self, w: u32, h: u32) {
        self.size.store(pack(w, h), Ordering::Release);
    }

    pub fn size(&self) -> (u32, u32) {
        unpack(self.size.load(Ordering::Acquire))
    }

    pub fn set_scale(&self, scale: f64) {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        self.scale.store(scale.to_bits(), Ordering::Release);
    }

    pub fn scale(&self) -> f64 {
        let s = f64::from_bits(self.scale.load(Ordering::Acquire));
        if s.is_finite() && s > 0.0 {
            s
        } else {
            1.0
        }
    }

    pub fn set_video_size(&self, w: u32, h: u32) {
        self.video.store(pack(w, h), Ordering::Release);
    }

    /// Forget the video size (surface hidden), so the next session never
    /// normalizes with a stale letterbox.
    pub fn clear_video_size(&self) {
        self.video.store(0, Ordering::Release);
    }

    pub fn video_size(&self) -> Option<(u32, u32)> {
        let (w, h) = unpack(self.video.load(Ordering::Acquire));
        (w > 0 && h > 0).then_some((w, h))
    }

    /// Normalize a pointer position in physical pixels of the full surface.
    pub fn normalize(&self, px: f64, py: f64) -> (f64, f64) {
        crate::geometry::normalize(px, py, self.size(), self.video_size())
    }

    pub fn set_key_focus(&self, focused: bool) {
        self.key_focus.store(focused, Ordering::Release);
    }

    /// Whether the surface has the keyboard (see the field docs). `false`
    /// until the UI thread first reports focus.
    pub fn has_key_focus(&self) -> bool {
        self.key_focus.load(Ordering::Acquire)
    }

    pub fn input(&self) -> MutexGuard<'_, InputState> {
        lock(&self.input)
    }

    /// The input state if it is free right now, `None` if another thread
    /// holds it. For callers that must never wait (an OS input callback);
    /// a poisoned lock is recovered as in [`input`](Self::input).
    pub fn try_input(&self) -> Option<MutexGuard<'_, InputState>> {
        match self.input.try_lock() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    pub fn set_drop_sink(&self, sink: Option<Sender<FileDrop>>) {
        *lock(&self.drops) = sink;
    }

    pub fn accepts_drops(&self) -> bool {
        lock(&self.drops).is_some()
    }

    /// Hand a drop to the sink. `false` when drops are refused (no sink, or
    /// the session is gone) so the OS can show the drag as rejected.
    pub fn deliver_drop(&self, drop: FileDrop) -> bool {
        match lock(&self.drops).as_ref() {
            Some(tx) => tx.send(drop).is_ok(),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::mpsc::channel;

    #[test]
    fn sizes_round_trip_and_default_to_unknown() {
        let s = SurfaceState::new();
        assert_eq!(s.size(), (0, 0));
        assert_eq!(s.video_size(), None);
        assert_eq!(s.scale(), 1.0);
        s.set_size(3024, 1890);
        s.set_video_size(1920, 1080);
        s.set_scale(2.0);
        assert_eq!(s.size(), (3024, 1890));
        assert_eq!(s.video_size(), Some((1920, 1080)));
        assert_eq!(s.scale(), 2.0);
        s.clear_video_size();
        assert_eq!(s.video_size(), None);
        s.set_scale(-3.0);
        assert_eq!(s.scale(), 1.0);
    }

    #[test]
    fn normalize_uses_the_published_frame_size() {
        let s = SurfaceState::new();
        s.set_size(1000, 1000);
        assert_eq!(s.normalize(500.0, 250.0), (0.5, 0.25));
        s.set_video_size(1000, 500); // letterboxed to y ∈ [250, 750]
        assert_eq!(s.normalize(500.0, 250.0), (0.5, 0.0));
    }

    #[test]
    fn key_focus_starts_off_and_follows_the_ui_thread() {
        let s = SurfaceState::new();
        assert!(!s.has_key_focus());
        s.set_key_focus(true);
        assert!(s.has_key_focus());
        s.set_key_focus(false);
        assert!(!s.has_key_focus());
    }

    #[test]
    fn try_input_never_waits_for_a_holder() {
        let s = SurfaceState::new();
        s.input().key("MetaLeft", true);
        assert!(s.try_input().is_some_and(|i| i.is_key_held("MetaLeft")));
        let held = s.input();
        assert!(s.try_input().is_none());
        std::mem::drop(held);
        assert!(s.try_input().is_some());
    }

    #[test]
    fn drops_need_a_live_sink() {
        let s = SurfaceState::new();
        let drop = || FileDrop {
            paths: vec![PathBuf::from("/tmp/a.txt")],
            nx: 0.5,
            ny: 0.5,
        };
        assert!(!s.accepts_drops());
        assert!(!s.deliver_drop(drop()));
        let (tx, rx) = channel();
        s.set_drop_sink(Some(tx));
        assert!(s.accepts_drops());
        assert!(s.deliver_drop(drop()));
        assert_eq!(rx.try_recv().map(|d| d.paths.len()).ok(), Some(1));
        std::mem::drop(rx);
        assert!(!s.deliver_drop(drop()));
        s.set_drop_sink(None);
        assert!(!s.accepts_drops());
    }
}
