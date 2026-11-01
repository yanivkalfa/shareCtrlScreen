//! The hand-off between the PipeWire thread (producer) and the host media thread
//! (consumer, `ScreenCapture::acquire`): a depth-1, newest-wins picture slot plus the
//! latest pointer state and the stream's health.
//!
//! Same design as the macOS capture's slot (platform-macos `slot.rs`): the host
//! encodes the *current* screen, never a backlog, so a picture that arrives while the
//! previous one still waits replaces it. Two additions, because here the PipeWire
//! buffer is copied out and returned at once instead of being lent:
//! * the damage of a replaced picture is merged into its replacement (its pixels are
//!   in the newer picture, but the region would otherwise never be reported), and
//! * pixel vectors are recycled through a small pool, so a steady stream allocates
//!   nothing.
//!
//! The pointer travels beside the pictures, never inside them: a pointer move wakes
//! the consumer without a new picture (the host then sends a cursor-only update, as
//! with DXGI's pointer metadata).

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use platform_api::{CursorShape, Point};

use super::damage::Damage;
use super::pixels;

/// Pixel vectors kept for reuse: one being filled, one waiting, one held by the
/// consumer. A fourth would only ever be idle memory (8 MiB at 1080p).
const POOL: usize = 3;

/// One copied frame: packed BGRA, `width × 4` bytes per row.
#[derive(Debug)]
pub(crate) struct Picture {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub damage: Damage,
}

/// What a buffer's cursor metadata says about the sprite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Sprite {
    /// No new bitmap (`bitmap_offset` 0): only the position moved. The pointer shows
    /// the current sprite, if there is a visible one.
    Unchanged,
    /// A bitmap header without an image (format 0, or "an offset of 0 means no image
    /// data (invisible)", spa/buffer/meta.h). Mutter's "empty sprite" zeroes the whole
    /// bitmap header this way. The sprite is gone until a new one arrives.
    Hidden,
    New(CursorShape),
}

/// One buffer's `spa_meta_cursor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CursorMeta {
    /// The meta is present with id 0. Mutter 46 writes this on every buffer while the
    /// pointer is hidden or off the shared monitor (`unset_cursor_metadata`), and OBS
    /// reads it the same way: the pointer is not in the stream.
    Absent,
    At(Point, Sprite),
}

/// Why a slot was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ended {
    /// The PipeWire stream ended (node gone, PipeWire restarted, core error) while the
    /// portal session may still stand: a rebuild from the restore token recovers.
    Stream,
    /// The desktop closed the portal session: the user pressed "Stop sharing" in the
    /// desktop's indicator (or revoked the grant). Terminal: reopening it silently
    /// would override an explicit choice.
    ByDesktop,
}

/// The pointer as last reported by the compositor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Pointer {
    pub visible: bool,
    pub position: Point,
    pub shape: Option<Arc<CursorShape>>,
    /// Advances whenever `shape` is replaced, so the consumer sends a shape only when
    /// it changed.
    pub shape_serial: u64,
}

/// Result of [`Slot::wait`].
#[derive(Debug)]
pub(crate) enum Wait {
    Picture(Picture),
    /// The pointer changed (no new picture).
    Pointer,
    Timeout,
    /// Paused after streaming, or the negotiated size changed: report `Lost`; the
    /// consumer's `reinit` waits for the stream to come back ([`Slot::wait_ready`]).
    Interrupted,
    /// The stream or the portal session is gone for good.
    Closed(Ended),
}

/// Result of [`Slot::wait_ready`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Ready {
    /// Streaming at this size.
    Streaming((u32, u32)),
    Closed(Ended),
    TimedOut,
}

/// Delivery counters, logged when a stream ends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SlotStats {
    pub delivered: u64,
    pub superseded: u64,
}

#[derive(Default)]
struct State {
    pending: Option<Picture>,
    pool: Vec<Vec<u8>>,
    pointer: Pointer,
    pointer_serial: u64,
    /// The negotiated frame size; `None` while (re)negotiating.
    format: Option<(u32, u32)>,
    /// The last size the consumer was told about (by `wait_ready`).
    acknowledged: Option<(u32, u32)>,
    streaming: bool,
    has_streamed: bool,
    interrupted: bool,
    closed: Option<Ended>,
    /// A picture-bearing buffer or picture was discarded since the last accepted
    /// picture. Compositors report damage "since the last recorded frame" and never
    /// again, so the next picture must be treated as fully changed.
    damage_lost: bool,
    stats: SlotStats,
}

#[derive(Default)]
pub(crate) struct Slot {
    state: Mutex<State>,
    changed: Condvar,
}

impl Slot {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    // Poisoning only means a thread panicked while holding the lock; every update
    // below leaves the state consistent, so the data is still valid and capture keeps
    // working instead of cascading the panic.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.lock());
        self.changed.notify_all();
    }

    // ---- producer (PipeWire thread) ----------------------------------------------

    /// A vector to copy the next frame into (recycled when possible).
    pub(crate) fn take_buffer(&self) -> Vec<u8> {
        self.lock().pool.pop().unwrap_or_default()
    }

    /// Return a vector to the pool.
    pub(crate) fn recycle(&self, pixels: Vec<u8>) {
        let mut s = self.lock();
        if s.pool.len() < POOL {
            s.pool.push(pixels);
        }
    }

    /// Offer a new picture, replacing any picture still waiting. Ignored once closed,
    /// and for a picture of another size than the negotiated one (it was copied
    /// across a renegotiation and must not reach an encoder built for the old size).
    pub(crate) fn put(&self, mut picture: Picture) {
        self.update(|s| {
            let size = (picture.width, picture.height);
            if s.closed.is_some() || s.format != Some(size) {
                recycle_into(&mut s.pool, picture.pixels);
                s.damage_lost = true;
                return;
            }
            if std::mem::take(&mut s.damage_lost) {
                picture.damage = Damage::Full;
            }
            s.stats.delivered += 1;
            if let Some(old) = s.pending.take() {
                s.stats.superseded += 1;
                picture.damage = old.damage.union(picture.damage, size);
                recycle_into(&mut s.pool, old.pixels);
            }
            s.pending = Some(picture);
        });
    }

    /// A `Format` was fixed (`Some`) or cleared for renegotiation (`None`). A new size
    /// after the consumer has started is an interruption: its encoder was built for
    /// the old size (trait invariant 1).
    pub(crate) fn set_format(&self, size: Option<(u32, u32)>) {
        self.update(|s| {
            if size.is_some() && s.acknowledged.is_some() && size != s.acknowledged {
                s.interrupted = true;
            }
            if size != s.format {
                if let Some(old) = s.pending.take() {
                    recycle_into(&mut s.pool, old.pixels);
                    s.damage_lost = true;
                }
            }
            s.format = size;
        });
    }

    /// The producer skipped a buffer that carried a picture (corrupted, unreadable, or
    /// not copyable): its damage is gone, so the next picture counts as fully changed.
    pub(crate) fn lose_damage(&self) {
        self.lock().damage_lost = true;
    }

    /// The stream entered (`true`) or left (`false`) the streaming state. Leaving it
    /// after having streamed is an interruption; the initial negotiation is not.
    pub(crate) fn set_streaming(&self, streaming: bool) {
        self.update(|s| {
            if !streaming && s.streaming && s.has_streamed {
                s.interrupted = true;
            }
            s.streaming = streaming;
            s.has_streamed |= streaming;
        });
    }

    /// The stream or session is gone; wake everyone. The desktop closing the session
    /// outranks a stream failure: "Stop sharing" usually removes the node (stream
    /// failure) a moment before the portal's `Closed` signal arrives, and the user's
    /// choice must win.
    pub(crate) fn close(&self, why: Ended) {
        self.update(|s| {
            if s.closed != Some(Ended::ByDesktop) {
                s.closed = Some(why);
            }
            if let Some(old) = s.pending.take() {
                recycle_into(&mut s.pool, old.pixels);
            }
        });
    }

    /// Apply one buffer's cursor metadata. Notifies only on a real change, so a
    /// compositor that repeats the metadata on every frame does not wake the consumer.
    pub(crate) fn set_pointer(&self, meta: CursorMeta) {
        let mut s = self.lock();
        let mut next = s.pointer.clone();
        match meta {
            CursorMeta::Absent => next.visible = false,
            CursorMeta::At(position, sprite) => {
                next.position = position;
                match sprite {
                    Sprite::Unchanged => {}
                    Sprite::Hidden => {
                        if next.shape.take().is_some() {
                            next.shape_serial += 1;
                        }
                    }
                    Sprite::New(shape) => {
                        if next.shape.as_deref() != Some(&shape) {
                            next.shape_serial += 1;
                            next.shape = Some(Arc::new(shape));
                        }
                    }
                }
                // A pointer in the stream shows its current sprite; a fully
                // transparent one shows nothing either way.
                next.visible = next.shape.as_deref().is_some_and(pixels::is_visible);
            }
        }
        if next != s.pointer {
            s.pointer = next;
            s.pointer_serial += 1;
            drop(s);
            self.changed.notify_all();
        }
    }

    // ---- consumer (host media thread) --------------------------------------------

    /// The pointer and its serial.
    pub(crate) fn pointer(&self) -> (u64, Pointer) {
        let s = self.lock();
        (s.pointer_serial, s.pointer.clone())
    }

    pub(crate) fn stats(&self) -> SlotStats {
        self.lock().stats
    }

    /// Whether a picture is waiting to be taken.
    pub(crate) fn has_pending(&self) -> bool {
        self.lock().pending.is_some()
    }

    /// Wait up to `timeout` for a picture, or for a pointer change after
    /// `seen_pointer` (a serial from [`pointer`](Self::pointer)). Health comes first:
    /// once closed or interrupted, a picture left behind is not worth encoding.
    pub(crate) fn wait(&self, timeout: Duration, seen_pointer: u64) -> Wait {
        let deadline = Instant::now().checked_add(timeout);
        let mut s = self.lock();
        loop {
            if let Some(why) = s.closed {
                return Wait::Closed(why);
            }
            if s.interrupted {
                return Wait::Interrupted;
            }
            if let Some(picture) = s.pending.take() {
                return Wait::Picture(picture);
            }
            if s.pointer_serial != seen_pointer {
                return Wait::Pointer;
            }
            match remaining(deadline) {
                Some(Some(left)) => {
                    s = self
                        .changed
                        .wait_timeout(s, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
                Some(None) => return Wait::Timeout,
                None => s = self.changed.wait(s).unwrap_or_else(PoisonError::into_inner),
            }
        }
    }

    /// Wait up to `timeout` for the stream to be streaming with a fixed format, then
    /// clear any interruption and adopt that size. Checking and clearing under one lock
    /// means an interruption that arrives afterwards is never lost.
    pub(crate) fn wait_ready(&self, timeout: Duration) -> Ready {
        let deadline = Instant::now().checked_add(timeout);
        let mut s = self.lock();
        loop {
            if let Some(why) = s.closed {
                return Ready::Closed(why);
            }
            if let (true, Some(size)) = (s.streaming, s.format) {
                s.interrupted = false;
                s.acknowledged = Some(size);
                return Ready::Streaming(size);
            }
            match remaining(deadline) {
                Some(Some(left)) => {
                    s = self
                        .changed
                        .wait_timeout(s, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
                Some(None) => return Ready::TimedOut,
                None => s = self.changed.wait(s).unwrap_or_else(PoisonError::into_inner),
            }
        }
    }

    /// After a stream failure: wait up to `timeout` for the desktop's `Closed` signal,
    /// which follows "Stop sharing" a moment after the node disappears. `true` = the
    /// desktop ended the session.
    pub(crate) fn wait_ended_by_desktop(&self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let mut s = self.lock();
        loop {
            if s.closed == Some(Ended::ByDesktop) {
                return true;
            }
            match remaining(deadline) {
                Some(Some(left)) => {
                    s = self
                        .changed
                        .wait_timeout(s, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
                Some(None) => return false,
                None => s = self.changed.wait(s).unwrap_or_else(PoisonError::into_inner),
            }
        }
    }
}

fn recycle_into(pool: &mut Vec<Vec<u8>>, pixels: Vec<u8>) {
    if pool.len() < POOL {
        pool.push(pixels);
    }
}

/// `None` = no deadline (a timeout too large to represent: wait forever);
/// `Some(None)` = the deadline has passed; `Some(Some(d))` = `d` left.
fn remaining(deadline: Option<Instant>) -> Option<Option<Duration>> {
    let d = deadline?;
    Some(
        d.checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::Rect;

    const SIZE: (u32, u32) = (4, 2);

    fn picture(tag: u8, damage: Damage) -> Picture {
        Picture {
            width: SIZE.0,
            height: SIZE.1,
            pixels: vec![tag; 32],
            damage,
        }
    }

    fn rects(r: &[(i32, i32, u32, u32)]) -> Damage {
        Damage::from_regions(r.iter().copied(), SIZE)
    }

    fn streaming() -> Slot {
        let slot = Slot::new();
        slot.set_format(Some(SIZE));
        slot.set_streaming(true);
        assert_eq!(slot.wait_ready(Duration::ZERO), Ready::Streaming(SIZE));
        slot
    }

    fn shape(alpha: u8) -> CursorShape {
        CursorShape {
            width: 1,
            height: 1,
            hotspot: Point::default(),
            bgra: vec![1, 2, 3, alpha],
        }
    }

    #[test]
    fn newest_picture_wins_and_keeps_the_superseded_damage() {
        let slot = streaming();
        slot.put(picture(1, rects(&[(0, 0, 1, 1)])));
        slot.put(picture(2, rects(&[(3, 1, 1, 1)])));
        let Wait::Picture(p) = slot.wait(Duration::ZERO, 0) else {
            panic!("no picture");
        };
        assert_eq!(p.pixels[0], 2);
        assert_eq!(
            p.damage.into_dirty(),
            vec![
                Rect {
                    x: 0,
                    y: 0,
                    w: 1,
                    h: 1
                },
                Rect {
                    x: 3,
                    y: 1,
                    w: 1,
                    h: 1
                }
            ]
        );
        assert!(matches!(slot.wait(Duration::ZERO, 0), Wait::Timeout));
        assert_eq!(
            slot.stats(),
            SlotStats {
                delivered: 2,
                superseded: 1
            }
        );
        // The superseded vector went back to the pool.
        assert_eq!(slot.take_buffer(), vec![1u8; 32]);
    }

    #[test]
    fn the_pool_is_bounded() {
        let slot = Slot::new();
        for _ in 0..10 {
            slot.recycle(vec![0; 8]);
        }
        let taken: Vec<_> = (0..10).map(|_| slot.take_buffer()).collect();
        assert_eq!(taken.iter().filter(|v| !v.is_empty()).count(), POOL);
    }

    #[test]
    fn a_picture_of_another_size_never_reaches_the_consumer() {
        let slot = streaming();
        let mut p = picture(1, Damage::Full);
        p.width = 8;
        slot.put(p);
        assert!(matches!(slot.wait(Duration::ZERO, 0), Wait::Timeout));
    }

    #[test]
    fn a_resize_interrupts_until_the_consumer_adopts_the_new_size() {
        let slot = streaming();
        slot.put(picture(1, Damage::Full));
        slot.set_format(Some((8, 8)));
        assert!(matches!(slot.wait(Duration::ZERO, 0), Wait::Interrupted));
        // Still interrupted: only `wait_ready` acknowledges.
        assert!(matches!(slot.wait(Duration::ZERO, 0), Wait::Interrupted));
        assert_eq!(slot.wait_ready(Duration::ZERO), Ready::Streaming((8, 8)));
        // The old-size picture was dropped with the renegotiation.
        assert!(matches!(slot.wait(Duration::ZERO, 0), Wait::Timeout));
    }

    #[test]
    fn the_initial_negotiation_is_not_an_interruption_but_a_pause_later_is() {
        let slot = Slot::new();
        slot.set_streaming(false);
        slot.set_format(Some(SIZE));
        slot.set_streaming(true);
        assert!(matches!(slot.wait(Duration::ZERO, 0), Wait::Timeout));
        slot.set_streaming(false);
        assert!(matches!(slot.wait(Duration::ZERO, 0), Wait::Interrupted));
        assert_eq!(slot.wait_ready(Duration::from_millis(10)), Ready::TimedOut);
        slot.set_streaming(true);
        assert_eq!(slot.wait_ready(Duration::ZERO), Ready::Streaming(SIZE));
        assert!(matches!(slot.wait(Duration::ZERO, 0), Wait::Timeout));
    }

    fn at(x: i32, sprite: Sprite) -> CursorMeta {
        CursorMeta::At(Point { x, y: 0 }, sprite)
    }

    fn next_damage(slot: &Slot) -> Damage {
        slot.put(picture(9, rects(&[(0, 0, 1, 1)])));
        match slot.wait(Duration::ZERO, 0) {
            Wait::Picture(p) => p.damage,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn closing_wakes_a_waiter_and_refuses_late_pictures() {
        let slot = Arc::new(streaming());
        let closer = Arc::clone(&slot);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            closer.close(Ended::Stream);
        });
        assert!(matches!(
            slot.wait(Duration::from_secs(5), 0),
            Wait::Closed(Ended::Stream)
        ));
        t.join().unwrap();
        slot.put(picture(1, Damage::Full));
        assert!(!slot.has_pending());
        assert_eq!(
            slot.wait_ready(Duration::from_secs(5)),
            Ready::Closed(Ended::Stream)
        );
    }

    #[test]
    fn the_desktop_closing_the_session_outranks_a_stream_failure() {
        let slot = Arc::new(streaming());
        slot.close(Ended::Stream);
        assert!(!slot.wait_ended_by_desktop(Duration::from_millis(10)));
        let portal = Arc::clone(&slot);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            portal.close(Ended::ByDesktop);
        });
        assert!(slot.wait_ended_by_desktop(Duration::from_secs(5)));
        t.join().unwrap();
        // A later stream failure does not downgrade it.
        slot.close(Ended::Stream);
        assert!(matches!(
            slot.wait(Duration::ZERO, 0),
            Wait::Closed(Ended::ByDesktop)
        ));
    }

    #[test]
    fn a_discarded_picture_makes_the_next_one_full() {
        // The producer skipped a corrupted or unreadable buffer.
        let slot = streaming();
        slot.lose_damage();
        assert_eq!(next_damage(&slot), Damage::Full);
        // Once only: the picture after that carries its own damage again.
        assert_ne!(next_damage(&slot), Damage::Full);

        // A pending picture dropped by renegotiating to the same size.
        slot.put(picture(1, rects(&[(0, 0, 1, 1)])));
        slot.set_format(None);
        slot.set_format(Some(SIZE));
        assert_eq!(next_damage(&slot), Damage::Full);

        // A picture refused for its size.
        let mut wrong = picture(1, rects(&[(0, 0, 1, 1)]));
        wrong.width = 8;
        slot.put(wrong);
        assert_eq!(next_damage(&slot), Damage::Full);
    }

    #[test]
    fn a_picture_from_another_thread_wakes_the_waiter() {
        let slot = Arc::new(streaming());
        let producer = Arc::clone(&slot);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            producer.put(picture(7, Damage::Full));
        });
        assert!(matches!(
            slot.wait(Duration::from_secs(5), 0),
            Wait::Picture(Picture { .. })
        ));
        t.join().unwrap();
    }

    #[test]
    fn pointer_changes_wake_the_consumer_only_when_something_changed() {
        let slot = streaming();
        let (seen, _) = slot.pointer();
        // A position without any sprite yet: tracked, but nothing to show.
        slot.set_pointer(at(1, Sprite::Unchanged));
        assert!(matches!(slot.wait(Duration::ZERO, seen), Wait::Pointer));
        let (seen, p) = slot.pointer();
        assert!(!p.visible);

        slot.set_pointer(at(1, Sprite::New(shape(255))));
        let (seen2, p) = slot.pointer();
        assert_ne!(seen2, seen);
        assert!(p.visible);
        assert_eq!(p.shape_serial, 1);

        // Same metadata again (compositors repeat it every frame): no wake-up.
        slot.set_pointer(at(1, Sprite::New(shape(255))));
        slot.set_pointer(at(1, Sprite::Unchanged));
        assert!(matches!(slot.wait(Duration::ZERO, seen2), Wait::Timeout));
        assert_eq!(slot.pointer().1.shape_serial, 1);
    }

    #[test]
    fn a_pointer_outside_the_stream_hides_and_comes_back_with_its_sprite() {
        // Mutter 46: id 0 on every buffer while the pointer is off the monitor, then
        // position-only metadata when it returns.
        let slot = streaming();
        slot.set_pointer(at(0, Sprite::New(shape(255))));
        slot.set_pointer(CursorMeta::Absent);
        let p = slot.pointer().1;
        assert!(!p.visible);
        assert_eq!(p.position, Point { x: 0, y: 0 });
        slot.set_pointer(at(3, Sprite::Unchanged));
        let p = slot.pointer().1;
        assert!(p.visible);
        assert_eq!(p.position, Point { x: 3, y: 0 });
        assert_eq!(p.shape_serial, 1);
    }

    #[test]
    fn an_empty_or_transparent_sprite_hides_the_pointer_until_a_new_one() {
        let slot = streaming();
        slot.set_pointer(at(0, Sprite::New(shape(255))));
        // Mutter's empty sprite: a zeroed bitmap header.
        slot.set_pointer(at(0, Sprite::Hidden));
        let p = slot.pointer().1;
        assert!(!p.visible);
        assert_eq!(p.shape, None);
        // A position without a new bitmap does not bring the old sprite back...
        slot.set_pointer(at(2, Sprite::Unchanged));
        assert!(!slot.pointer().1.visible);
        // ...the next sprite does.
        slot.set_pointer(at(2, Sprite::New(shape(255))));
        assert!(slot.pointer().1.visible);
        // A fully transparent sprite shows nothing.
        slot.set_pointer(at(2, Sprite::New(shape(0))));
        assert!(!slot.pointer().1.visible);
    }

    #[test]
    fn huge_timeouts_do_not_overflow() {
        let slot = streaming();
        slot.put(picture(3, Damage::Full));
        assert!(matches!(slot.wait(Duration::MAX, 0), Wait::Picture(_)));
        assert_eq!(slot.wait_ready(Duration::MAX), Ready::Streaming(SIZE));
    }
}
