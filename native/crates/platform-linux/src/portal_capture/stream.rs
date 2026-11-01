//! The PipeWire side of a capture: a thread running a `pw_main_loop` with one input
//! stream connected to the portal's node, copying every new buffer into the [`Slot`].
//!
//! PipeWire objects are bound to the loop that created them, so the context, core and
//! stream live and die on this thread; the capture talks to it only through the slot
//! and a quit message ([`pipewire::channel`], which wakes the loop).
//!
//! Buffers are copied out and queued back inside `process`, never held: the producer
//! renders into a small fixed pool (negotiated in `Buffers`), and a consumer that keeps
//! buffers leaves it nothing to render into, stalling the stream.

use std::os::fd::OwnedFd;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use pipewire as pw;
use platform_api::{CaptureError, Point};
use pw::spa::buffer::meta::{MetaCursor, MetaHeader, MetaHeaderFlags, MetaVideoDamage};
use pw::spa::buffer::{ChunkFlags, DataType};
use pw::spa::param::format::{MediaSubtype, MediaType};
use pw::spa::param::format_utils;
use pw::spa::param::video::VideoInfoRaw;
use pw::spa::param::ParamType;
use pw::spa::pod::Pod;
use pw::spa::utils::Direction;
use pw::stream::{Stream, StreamFlags, StreamState};

use super::damage::Damage;
use super::params;
use super::pixels::{self, CopyError, Layout, Plane};
use super::slot::{CursorMeta, Ended, Picture, Slot, Sprite};

/// How long the loop may take to connect before `start` gives up. Connecting is local
/// IPC; seconds mean PipeWire is wedged.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// A running stream thread. Dropping it stops the loop and joins the thread, after
/// which no callback can touch the slot.
pub(crate) struct PipeWireStream {
    quit: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl PipeWireStream {
    /// Connect to `node` on the portal's PipeWire remote `fd`.
    pub(crate) fn start(fd: OwnedFd, node: u32, slot: Arc<Slot>) -> Result<Self, CaptureError> {
        let (quit, quit_rx) = pw::channel::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("sharectrl-pipewire".into())
            .spawn(move || run(fd, node, slot, quit_rx, ready_tx))
            .map_err(|e| CaptureError::Backend(format!("cannot start the PipeWire thread: {e}")))?;
        let stream = Self {
            quit,
            thread: Some(thread),
        };
        match ready_rx.recv_timeout(CONNECT_TIMEOUT) {
            Ok(Ok(())) => Ok(stream),
            // `stream` drops here: the thread has already returned, the join is quick.
            Ok(Err(message)) => Err(CaptureError::Backend(message)),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(CaptureError::Backend(format!(
                "PipeWire did not connect the screen stream within {} s \
                 (is the PipeWire service running? `systemctl --user status pipewire`)",
                CONNECT_TIMEOUT.as_secs()
            ))),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(CaptureError::Backend(
                "the PipeWire thread exited while connecting".into(),
            )),
        }
    }
}

impl Drop for PipeWireStream {
    fn drop(&mut self) {
        // An error means the loop already ended (its receiver is gone).
        let _ = self.quit.send(());
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                tracing::warn!("pipewire: the stream thread panicked");
            }
        }
    }
}

/// Per-stream state the callbacks share (all on the loop thread).
struct Callbacks {
    slot: Arc<Slot>,
    /// The fixed format: size and channel layout.
    format: Option<((u32, u32), Layout)>,
    /// `update_params` failed: the producer will use its defaults (no metadata).
    warned_params: bool,
    /// Last copy failure logged, so a broken producer logs once, not 60 times a second.
    warned_copy: Option<CopyError>,
    warned_dmabuf: bool,
}

fn run(
    fd: OwnedFd,
    node: u32,
    slot: Arc<Slot>,
    quit: pw::channel::Receiver<()>,
    ready: mpsc::SyncSender<Result<(), String>>,
) {
    // The capture waits on `ready` for at most CONNECT_TIMEOUT; after that nobody
    // listens, which is fine.
    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(l) => l,
        Err(e) => {
            let _ = ready.send(Err(format!("cannot create a PipeWire loop: {e}")));
            return;
        }
    };
    match connect(&mainloop, fd, node, &slot) {
        Ok(connected) => {
            let quit_loop = mainloop.clone();
            let _quit = quit.attach(mainloop.loop_(), move |()| quit_loop.quit());
            let _ = ready.send(Ok(()));
            mainloop.run();
            let stats = slot.stats();
            tracing::info!(
                "pipewire: stream closed — {} frames delivered, {} superseded before the encoder took them",
                stats.delivered,
                stats.superseded
            );
            // Disconnect before the listeners and core go (reverse of creation).
            if let Err(e) = connected.stream.disconnect() {
                tracing::debug!("pipewire: disconnect: {e}");
            }
            drop(connected);
        }
        Err(message) => {
            let _ = ready.send(Err(message));
        }
    }
    // Whatever ended the loop, the consumer must not wait for frames that never come.
    slot.close(Ended::Stream);
}

/// Everything that must stay alive while the loop runs, in drop order.
struct Connected {
    _listener: pw::stream::StreamListener<Callbacks>,
    stream: pw::stream::StreamRc,
    _core_listener: pw::core::Listener,
    _core: pw::core::CoreRc,
    _context: pw::context::ContextRc,
}

fn connect(
    mainloop: &pw::main_loop::MainLoopRc,
    fd: OwnedFd,
    node: u32,
    slot: &Arc<Slot>,
) -> Result<Connected, String> {
    let context = pw::context::ContextRc::new(mainloop, None)
        .map_err(|e| format!("cannot create a PipeWire context: {e}"))?;
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|e| format!("cannot connect to the portal's PipeWire remote: {e}"))?;
    let core_listener = core
        .add_listener_local()
        .error({
            let slot = Arc::clone(slot);
            let quit_loop = mainloop.clone();
            move |id, _seq, res, message| {
                // id 0 is the core itself: the connection to PipeWire is gone (daemon
                // restart, the portal revoking the remote). Errors on other objects
                // are reported on the stream's own state.
                tracing::warn!("pipewire: error on object {id}: {message} ({res})");
                if id == pw::core::PW_ID_CORE {
                    slot.close(Ended::Stream);
                    quit_loop.quit();
                }
            }
        })
        .register();

    let stream = pw::stream::StreamRc::new(
        core.clone(),
        "sharectrl-screen",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| format!("cannot create a PipeWire stream: {e}"))?;

    let callbacks = Callbacks {
        slot: Arc::clone(slot),
        format: None,
        warned_params: false,
        warned_copy: None,
        warned_dmabuf: false,
    };
    let quit_loop = mainloop.clone();
    let state_slot = Arc::clone(slot);
    let listener = stream
        .add_local_listener_with_user_data(callbacks)
        .state_changed(move |_, _, old, new| on_state(&state_slot, &quit_loop, old, new))
        .param_changed(on_param)
        .process(on_process)
        .register()
        .map_err(|e| format!("cannot listen to the PipeWire stream: {e}"))?;

    let format = params::enum_format()?;
    let pod = Pod::from_bytes(&format).ok_or("the format parameter is not a valid pod")?;
    stream
        .connect(
            Direction::Input,
            Some(node),
            // MAP_BUFFERS: PipeWire mmaps MemFd blocks so `data()` is readable.
            // DONT_RECONNECT: when the portal's node goes away the stream must end,
            // not wander to another node.
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::DONT_RECONNECT,
            &mut [pod],
        )
        .map_err(|e| format!("cannot connect the PipeWire stream to node {node}: {e}"))?;

    Ok(Connected {
        _listener: listener,
        stream,
        _core_listener: core_listener,
        _core: core,
        _context: context,
    })
}

fn on_state(slot: &Slot, mainloop: &pw::main_loop::MainLoopRc, old: StreamState, new: StreamState) {
    tracing::debug!("pipewire: stream {old:?} → {new:?}");
    match new {
        StreamState::Streaming => slot.set_streaming(true),
        StreamState::Paused | StreamState::Connecting => slot.set_streaming(false),
        // The node went away (the user stopped sharing, the monitor was unplugged) or
        // the stream failed: nothing more will arrive on this stream.
        StreamState::Unconnected | StreamState::Error(_) => {
            if let StreamState::Error(e) = &new {
                tracing::warn!("pipewire: stream error: {e}");
            }
            slot.close(Ended::Stream);
            mainloop.quit();
        }
    }
}

fn on_param(stream: &Stream, cb: &mut Callbacks, id: u32, param: Option<&Pod>) {
    if id != ParamType::Format.as_raw() {
        return;
    }
    // `None` = the format was cleared for renegotiation.
    let Some(param) = param else {
        cb.format = None;
        cb.slot.set_format(None);
        return;
    };
    match format_utils::parse_format(param) {
        Ok((MediaType::Video, MediaSubtype::Raw)) => {}
        _ => return,
    }
    let mut info = VideoInfoRaw::new();
    if let Err(e) = info.parse(param) {
        tracing::warn!("pipewire: unreadable video format: {e}");
        return;
    }
    let size = (info.size().width, info.size().height);
    // Only formats we offered can be fixed; anything else is a producer bug, and
    // reading it as BGRA would show garbage.
    let Some(layout) = pixels::layout(info.format()) else {
        tracing::warn!(
            "pipewire: producer fixed format {:?}, which was not offered",
            info.format()
        );
        return;
    };
    if size.0 == 0 || size.1 == 0 || size.0 > params::MAX_SIDE || size.1 > params::MAX_SIDE {
        tracing::warn!(
            "pipewire: producer fixed size {}x{}, outside the offer",
            size.0,
            size.1
        );
        return;
    }
    tracing::info!(
        "pipewire: format {:?} {}x{} @ {}/{}",
        info.format(),
        size.0,
        size.1,
        info.framerate().num,
        info.framerate().denom
    );
    cb.format = Some((size, layout));
    cb.slot.set_format(Some(size));

    match params::buffers_and_meta() {
        Ok(bytes) => {
            let mut pods: Vec<&Pod> = bytes.iter().filter_map(|b| Pod::from_bytes(b)).collect();
            if let Err(e) = stream.update_params(&mut pods) {
                warn_params(cb, &e.to_string());
            }
        }
        Err(e) => warn_params(cb, &e),
    }
}

fn warn_params(cb: &mut Callbacks, error: &str) {
    if !cb.warned_params {
        cb.warned_params = true;
        tracing::warn!(
            "pipewire: cannot request buffer metadata ({error}); \
             the pointer and damage regions may be missing"
        );
    }
}

fn on_process(stream: &Stream, cb: &mut Callbacks) {
    let Some((size, layout)) = cb.format else {
        // Buffers before a fixed format carry nothing we can interpret; dropping them
        // queues them straight back.
        while stream.dequeue_buffer().is_some() {}
        return;
    };
    // Drain everything queued so only the newest picture is copied, but read the
    // pointer and damage of every buffer: a skipped buffer's changes are in the newer
    // picture, and its pointer update is still news.
    let mut damage: Option<Damage> = None;
    // A picture-bearing buffer was skipped: its damage is lost for good, since
    // compositors report damage "since the last recorded frame" only once.
    let mut lost = false;
    let mut newest = None;
    while let Some(mut buffer) = stream.dequeue_buffer() {
        if let Some(cursor) = buffer.find_meta::<MetaCursor>() {
            cb.slot.set_pointer(read_cursor(cursor));
        }
        let header_corrupted = buffer
            .find_meta::<MetaHeader>()
            .is_some_and(|h| h.flags().contains(MetaHeaderFlags::CORRUPTED));
        let datas = buffer.datas_mut();
        // Size 0: a cursor-only buffer (Mutter sends these when only the pointer
        // moved). No picture, nothing to copy.
        let Some(first) = datas.first().filter(|d| d.chunk().size() > 0) else {
            continue;
        };
        if header_corrupted || first.chunk().flags().contains(ChunkFlags::CORRUPTED) {
            lost = true;
            continue;
        }
        // Mutter writes no terminating entry in the damage array, and libspa's
        // iterator stops at the first empty region, so a reused buffer can still hold
        // stale regions past this frame's count. That only over-reports damage (a
        // region re-sent needlessly), never hides a change.
        let this = buffer
            .find_meta::<MetaVideoDamage>()
            .map(|d| {
                Damage::from_regions(
                    d.iter().map(|r| {
                        (
                            r.position().x,
                            r.position().y,
                            r.size().width,
                            r.size().height,
                        )
                    }),
                    size,
                )
            })
            .unwrap_or(Damage::Full);
        damage = Some(match damage.take() {
            Some(older) => older.union(this, size),
            None => this,
        });
        // Assigning drops (re-queues) the previous newest buffer.
        newest = Some(buffer);
    }
    let (Some(mut buffer), Some(damage)) = (newest, damage) else {
        if lost {
            cb.slot.lose_damage();
        }
        return;
    };
    let damage = if lost { Damage::Full } else { damage };
    if !copy_newest(cb, &mut buffer, size, layout, damage) {
        cb.slot.lose_damage();
    }
}

/// Copy the newest picture into the slot. `false` = it could not be read, and its
/// damage (and that of the buffers drained before it) is lost.
fn copy_newest(
    cb: &mut Callbacks,
    buffer: &mut pw::buffer::Buffer<'_>,
    size: (u32, u32),
    layout: Layout,
    damage: Damage,
) -> bool {
    let Some(data) = buffer.datas_mut().first_mut() else {
        return false;
    };
    if data.type_() == DataType::DmaBuf {
        // Never offered (see params.rs); a producer that sends one anyway gets logged.
        if !cb.warned_dmabuf {
            cb.warned_dmabuf = true;
            tracing::warn!("pipewire: producer sent a DMA-BUF that was not negotiated; skipping");
        }
        return false;
    }
    let plane = Plane {
        offset: data.chunk().offset(),
        stride: data.chunk().stride(),
    };
    let Some(src) = data.data() else {
        return false;
    };
    let mut pixels = cb.slot.take_buffer();
    match pixels::copy_frame(src, plane, size, layout, &mut pixels) {
        Ok(()) => {
            cb.warned_copy = None;
            cb.slot.put(Picture {
                width: size.0,
                height: size.1,
                pixels,
                damage,
            });
            true
        }
        Err(e) => {
            if cb.warned_copy != Some(e) {
                cb.warned_copy = Some(e);
                tracing::warn!("pipewire: skipping a {}x{} frame: {e}", size.0, size.1);
            }
            cb.slot.recycle(pixels);
            false
        }
    }
}

/// A buffer's `spa_meta_cursor` (spa/buffer/meta.h), read the way Mutter 46 writes it
/// and OBS reads it.
fn read_cursor(cursor: &MetaCursor) -> CursorMeta {
    // id 0: Mutter writes this on every buffer while the pointer is hidden or off the
    // shared monitor (`unset_cursor_metadata`). The header calls it "no new cursor
    // data"; the producer means "no pointer in this stream".
    if !cursor.is_valid() {
        return CursorMeta::Absent;
    }
    let position = Point {
        x: cursor.position().x,
        y: cursor.position().y,
    };
    let sprite = match cursor.bitmap() {
        // bitmap_offset 0: no new bitmap information.
        None => Sprite::Unchanged,
        // Format 0 or no image data: Mutter's "empty sprite" zeroes the whole bitmap
        // header; the header's "offset 0 means … invisible" says the same.
        Some(bitmap) if !bitmap.is_valid() => Sprite::Hidden,
        Some(bitmap) => {
            let size = (bitmap.size().width, bitmap.size().height);
            match bitmap.bitmap_data() {
                None => Sprite::Hidden,
                Some(_) if size.0 == 0 || size.1 == 0 => Sprite::Hidden,
                Some(data) => {
                    let hotspot = Point {
                        x: cursor.hotspot().x,
                        y: cursor.hotspot().y,
                    };
                    match pixels::cursor_shape(
                        bitmap.format(),
                        size,
                        bitmap.stride(),
                        data,
                        hotspot,
                    ) {
                        Some(shape) => Sprite::New(shape),
                        None => {
                            tracing::debug!(
                                "pipewire: unreadable cursor bitmap {:?} {}x{}",
                                bitmap.format(),
                                size.0,
                                size.1
                            );
                            Sprite::Unchanged
                        }
                    }
                }
            }
        }
    };
    CursorMeta::At(position, sprite)
}
