//! In-memory implementations of every trait (feature `mock`).
//!
//! Designed for the loopback harness and per-crate unit tests:
//! * [`MockCapture`] draws a moving bar and stamps a frame sequence number into
//!   the first 8 bytes of the picture.
//! * [`MockCodecFactory`] emits **valid Annex-B H.264 NAL framing** (SPS+PPS+IDR
//!   on keyframes, a non-IDR slice otherwise) whose payload is hex text, so it
//!   survives the real RTP packetizer/depacketizer and str0m's keyframe
//!   detection, and it carries the sequence number through to the decoder.
//! * [`MockSurface`] / its presenter record every presented sequence number, so a
//!   test can assert end-to-end ordering and identity.
//! * Injector, clipboard, secret store and autostart record what happened.
//!
//! All shared state is behind `Arc<Mutex<_>>` so the test thread can inspect it
//! while engine threads use the objects.

use std::any::Any;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use protocol::{Button, InputMsg};
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};

use crate::capture::{CaptureError, CapturedFrame, CursorShape, CursorUpdate, ScreenCapture};
use crate::codec::{
    Codec, CodecError, CodecFactory, EncodedUnit, EncoderConfig, VideoDecoder, VideoEncoder,
};
use crate::frame::{Frame, FrameData, PixelFormat, Point, Rect};
use crate::input::{InputError, InputInjector, ShortcutHook};
use crate::os::{Autostart, Clipboard, Paths, SecretStore, MAX_TEXT_BYTES};
use crate::platform::{
    DisplayInfo, OsPermission, PermissionReport, PermissionState, Platform, PlatformInfo,
};
use crate::surface::{FileDrop, Presenter, SurfaceError, VideoInput, VideoSurface};

/// Lock, recovering from poisoning (a panicking test thread must not cascade).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Read the little-endian sequence stamp from the first 8 bytes of a plane.
pub fn read_stamp(data: &[u8]) -> Option<u64> {
    data.get(..8)
        .map(|b| u64::from_le_bytes(b.try_into().expect("8 bytes")))
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

/// Behaviour knobs for [`MockCapture`].
#[derive(Debug, Clone)]
pub struct MockCaptureConfig {
    pub width: u32,
    pub height: u32,
    /// Frames per second the "screen" changes at.
    pub fps: u32,
    /// After this many frames the screen stops changing (`acquire` → `Ok(None)`).
    pub static_after: Option<u64>,
    /// Return [`CaptureError::Lost`] once when this frame number is reached.
    pub lose_at: Option<u64>,
}

impl Default for MockCaptureConfig {
    fn default() -> Self {
        Self {
            width: 640,
            height: 360,
            fps: 30,
            static_after: None,
            lose_at: None,
        }
    }
}

const BAR_W: u32 = 16;

/// Deterministic synthetic screen: a vertical bar moving 8 px per frame over a
/// gradient, a cursor moving along the diagonal.
pub struct MockCapture {
    cfg: MockCaptureConfig,
    buf: Vec<u8>,
    seq: u64,
    holding: bool,
    next_due: Instant,
    bar_x: Option<u32>,
    lost_fired: bool,
    /// Counts `release` calls that actually released a held frame (tests use it
    /// to check the release-on-every-path invariant).
    pub releases: u64,
    pub reinits: u64,
}

impl MockCapture {
    pub fn new(cfg: MockCaptureConfig) -> Self {
        let mut c = Self {
            buf: Vec::new(),
            seq: 0,
            holding: false,
            next_due: Instant::now(),
            bar_x: None,
            lost_fired: false,
            releases: 0,
            reinits: 0,
            cfg,
        };
        c.paint_background();
        c
    }

    fn stride(&self) -> usize {
        self.cfg.width as usize * 4
    }

    fn paint_background(&mut self) {
        let (w, h) = (self.cfg.width as usize, self.cfg.height as usize);
        self.buf = vec![0; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                self.buf[i] = (x * 255 / w.max(1)) as u8;
                self.buf[i + 1] = (y * 255 / h.max(1)) as u8;
                self.buf[i + 2] = 64;
                self.buf[i + 3] = 255;
            }
        }
    }

    fn paint_column(&mut self, x0: u32, bar: bool) {
        let (w, h) = (self.cfg.width as usize, self.cfg.height as usize);
        for y in 0..h {
            for x in x0 as usize..(x0 + BAR_W).min(self.cfg.width) as usize {
                let i = (y * w + x) * 4;
                if bar {
                    self.buf[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
                } else {
                    self.buf[i] = (x * 255 / w.max(1)) as u8;
                    self.buf[i + 1] = (y * 255 / h.max(1)) as u8;
                    self.buf[i + 2] = 64;
                    self.buf[i + 3] = 255;
                }
            }
        }
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(1) / self.cfg.fps.max(1)
    }
}

impl ScreenCapture for MockCapture {
    fn dimensions(&self) -> (u32, u32) {
        (self.cfg.width, self.cfg.height)
    }

    fn acquire(&mut self, timeout: Duration) -> Result<Option<CapturedFrame<'_>>, CaptureError> {
        self.release();
        if let Some(n) = self.cfg.lose_at {
            if self.seq == n && !self.lost_fired {
                self.lost_fired = true;
                return Err(CaptureError::Lost);
            }
        }
        if self.cfg.static_after.is_some_and(|n| self.seq >= n) {
            std::thread::sleep(timeout);
            return Ok(None);
        }
        let now = Instant::now();
        if self.next_due > now {
            let wait = self.next_due - now;
            if wait > timeout {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            std::thread::sleep(wait);
        }
        self.next_due = Instant::now() + self.interval();

        let w = self.cfg.width;
        let mut dirty = Vec::with_capacity(2);
        if let Some(old) = self.bar_x {
            self.paint_column(old, false);
            dirty.push(Rect {
                x: old as i32,
                y: 0,
                w: BAR_W.min(w - old),
                h: self.cfg.height,
            });
        }
        let span = w.saturating_sub(BAR_W).max(1);
        let x = ((self.seq * 8) % span as u64) as u32;
        self.paint_column(x, true);
        self.bar_x = Some(x);
        dirty.push(Rect {
            x: x as i32,
            y: 0,
            w: BAR_W.min(w - x),
            h: self.cfg.height,
        });
        // Stamp after painting so the bar never overwrites it.
        if self.buf.len() >= 8 {
            self.buf[..8].copy_from_slice(&self.seq.to_le_bytes());
        }

        let cursor = Some(CursorUpdate {
            visible: true,
            position: Point {
                x: (self.seq % w as u64) as i32,
                y: (self.seq % self.cfg.height as u64) as i32,
            },
            shape: (self.seq == 0).then(|| CursorShape {
                width: 1,
                height: 1,
                hotspot: Point::default(),
                bgra: vec![0, 0, 0, 255],
            }),
        });
        self.seq += 1;
        self.holding = true;
        let stride = self.stride();
        Ok(Some(CapturedFrame {
            frame: Frame::bgra(w, self.cfg.height, &self.buf, stride),
            dirty,
            moves: Vec::new(),
            pointer_only: false,
            cursor,
        }))
    }

    fn release(&mut self) {
        if self.holding {
            self.holding = false;
            self.releases += 1;
        }
    }

    fn reinit(&mut self) -> Result<(), CaptureError> {
        self.release();
        self.reinits += 1;
        self.paint_background();
        self.bar_x = None;
        Ok(())
    }

    fn device(&self) -> Option<&dyn Any> {
        None
    }
}

// ---------------------------------------------------------------------------
// Codec
// ---------------------------------------------------------------------------

const START: [u8; 4] = [0, 0, 0, 1];
const NAL_SPS: u8 = 0x67;
const NAL_PPS: u8 = 0x68;
const NAL_IDR: u8 = 0x65;
const NAL_SLICE: u8 = 0x41;
const MAGIC: &str = "MOCK1";

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn unhex(s: &[u8]) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    s.chunks(2)
        .map(|p| Some(digit(p[0])? << 4 | digit(p[1])?))
        .collect()
}

/// Split an Annex-B stream into NAL units (3- or 4-byte start codes).
pub fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(data.len(), |&n| n - 3);
        // A 4-byte start code leaves one extra zero before the next NAL.
        while end > s && data[end - 1] == 0 {
            end -= 1;
        }
        if end > s {
            out.push(&data[s..end]);
        }
    }
    out
}

/// Mock encoder: a fixed-cost "codec" whose output size tracks the bitrate so
/// the transport's fragmentation and pacing are exercised realistically.
pub struct MockEncoder {
    cfg: EncoderConfig,
    force_key: bool,
    encoded: u64,
}

impl MockEncoder {
    fn payload_len(&self, key: bool) -> usize {
        let fps = (self.cfg.fps_num / self.cfg.fps_den.max(1)).max(1) as usize;
        let per_frame = (self.cfg.bitrate_bps as usize / 8 / fps).clamp(64, 256 * 1024);
        if key {
            per_frame * 4
        } else {
            per_frame
        }
    }
}

impl VideoEncoder for MockEncoder {
    fn encode(&mut self, frame: &Frame<'_>) -> Result<Vec<EncodedUnit>, CodecError> {
        let FrameData::Cpu { plane0, .. } = frame.data else {
            return Err(CodecError::Unsupported(
                "mock encoder takes CPU frames".into(),
            ));
        };
        let stamp = read_stamp(plane0.data).unwrap_or(0);
        let key = self.encoded == 0 || std::mem::take(&mut self.force_key);
        self.encoded += 1;

        let header = format!(
            "{MAGIC}:{}:{}:{}:{}:",
            u8::from(key),
            frame.width,
            frame.height,
            stamp
        );
        // Padding to the bitrate budget; hex digits keep the payload free of
        // zero bytes so it can never contain a start code.
        let pad = self.payload_len(key).saturating_sub(header.len());
        let mut nal = Vec::with_capacity(header.len() + pad + 1);
        nal.push(if key { NAL_IDR } else { NAL_SLICE });
        nal.extend_from_slice(header.as_bytes());
        nal.extend(std::iter::repeat_n(b'a', pad));

        let mut out = Vec::with_capacity(nal.len() + 32);
        if key {
            out.extend_from_slice(&START);
            out.push(NAL_SPS);
            out.extend_from_slice(hex(b"mock-sps").as_bytes());
            out.extend_from_slice(&START);
            out.push(NAL_PPS);
            out.extend_from_slice(hex(b"mock-pps").as_bytes());
        }
        out.extend_from_slice(&START);
        out.extend_from_slice(&nal);
        Ok(vec![EncodedUnit {
            data: out,
            keyframe: key,
        }])
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<(), CodecError> {
        self.cfg.bitrate_bps = bps;
        Ok(())
    }

    fn force_keyframe(&mut self) {
        self.force_key = true;
    }
}

/// Mock decoder: parses [`MockEncoder`] output, refuses deltas until the first
/// keyframe (like a real decoder without references), and outputs an NV12 frame
/// whose Y plane starts with the sequence stamp.
pub struct MockDecoder {
    width: u32,
    height: u32,
    have_key: bool,
    y: Vec<u8>,
    uv: Vec<u8>,
}

impl MockDecoder {
    fn resize(&mut self, w: u32, h: u32) {
        if (w, h) != (self.width, self.height) || self.y.is_empty() {
            self.width = w;
            self.height = h;
            self.y = vec![16; (w * h) as usize];
            self.uv = vec![128; (w * h.div_ceil(2)) as usize];
        }
    }
}

impl VideoDecoder for MockDecoder {
    fn decode(
        &mut self,
        au: &[u8],
        _keyframe: bool,
        _pts_100ns: i64,
    ) -> Result<Option<Frame<'_>>, CodecError> {
        let slice = split_annex_b(au)
            .into_iter()
            .find(|n| matches!(n.first(), Some(&NAL_IDR) | Some(&NAL_SLICE)))
            .ok_or_else(|| CodecError::Backend("no slice NAL in access unit".into()))?;
        let text = std::str::from_utf8(&slice[1..])
            .map_err(|_| CodecError::Backend("slice is not mock text".into()))?;
        let mut f = text.splitn(6, ':');
        let parse = |s: Option<&str>| s.and_then(|v| v.parse::<u64>().ok());
        if f.next() != Some(MAGIC) {
            return Err(CodecError::Backend("bad mock magic".into()));
        }
        let (Some(key), Some(w), Some(h), Some(stamp)) = (
            parse(f.next()),
            parse(f.next()),
            parse(f.next()),
            parse(f.next()),
        ) else {
            return Err(CodecError::Backend("bad mock header".into()));
        };
        let is_key = key == 1;
        if is_key != (slice[0] == NAL_IDR) {
            return Err(CodecError::Backend("NAL type disagrees with header".into()));
        }
        if !is_key && !self.have_key {
            return Ok(None);
        }
        if is_key {
            self.have_key = true;
            self.resize(w as u32, h as u32);
        }
        if self.y.len() >= 8 {
            self.y[..8].copy_from_slice(&stamp.to_le_bytes());
        }
        let (w, h) = (self.width, self.height);
        Ok(Some(Frame::nv12(
            w, h, &self.y, w as usize, &self.uv, w as usize,
        )))
    }

    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// Codec factory over [`MockEncoder`] / [`MockDecoder`]. Advertises H.264 only.
#[derive(Debug, Default)]
pub struct MockCodecFactory;

impl CodecFactory for MockCodecFactory {
    fn host_encodable(&self) -> Vec<Codec> {
        vec![Codec::H264]
    }
    fn viewer_decodable(&self) -> Vec<Codec> {
        vec![Codec::H264]
    }
    fn new_encoder(
        &self,
        _device: Option<&dyn Any>,
        cfg: EncoderConfig,
    ) -> Result<Box<dyn VideoEncoder>, CodecError> {
        if cfg.codec != Codec::H264 {
            return Err(CodecError::NoEncoder(cfg.codec));
        }
        Ok(Box::new(MockEncoder {
            cfg,
            force_key: false,
            encoded: 0,
        }))
    }
    fn new_decoder(
        &self,
        _device: Option<&dyn Any>,
        codec: Codec,
        width: u32,
        height: u32,
    ) -> Result<Box<dyn VideoDecoder>, CodecError> {
        if codec != Codec::H264 {
            return Err(CodecError::NoDecoder(codec));
        }
        let mut d = MockDecoder {
            width,
            height,
            have_key: false,
            y: Vec::new(),
            uv: Vec::new(),
        };
        d.resize(width.max(2), height.max(2));
        Ok(Box::new(d))
    }
}

// ---------------------------------------------------------------------------
// Surface + presenter
// ---------------------------------------------------------------------------

/// One presented frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Presented {
    pub stamp: Option<u64>,
    pub width: u32,
    pub height: u32,
    pub cursor: Option<(f64, f64)>,
}

/// Everything the mock surface saw. Inspect via [`MockSurface::state`].
#[derive(Debug, Default)]
pub struct SurfaceState {
    pub visible: bool,
    pub client_size: (u32, u32),
    pub fits: u64,
    pub presented: Vec<Presented>,
    input_sink: Option<Sender<VideoInput>>,
    drop_sink: Option<Sender<FileDrop>>,
    held_keys: BTreeSet<&'static str>,
    held_buttons: BTreeSet<u8>,
    last_pos: (f64, f64),
}

/// Recording [`VideoSurface`]; tests drive "user input" with
/// [`user_input`](Self::user_input) and [`user_drop`](Self::user_drop).
#[derive(Debug, Default)]
pub struct MockSurface {
    state: Arc<Mutex<SurfaceState>>,
}

impl MockSurface {
    pub fn new(client_size: (u32, u32)) -> Arc<Self> {
        Arc::new(Self {
            state: Arc::new(Mutex::new(SurfaceState {
                client_size,
                ..Default::default()
            })),
        })
    }

    pub fn state(&self) -> MutexGuard<'_, SurfaceState> {
        lock(&self.state)
    }

    /// Simulate the user interacting with the video. Returns `false` when no sink
    /// is installed (input not captured outside a controlling session).
    pub fn user_input(&self, ev: VideoInput) -> bool {
        let mut s = lock(&self.state);
        match &ev {
            VideoInput::Key { code, down: true } => {
                s.held_keys.insert(code);
            }
            VideoInput::Key { code, down: false } => {
                s.held_keys.remove(code);
            }
            VideoInput::Button {
                button,
                down,
                nx,
                ny,
            } => {
                if *down {
                    s.held_buttons.insert(*button);
                } else {
                    s.held_buttons.remove(button);
                }
                s.last_pos = (*nx, *ny);
            }
            VideoInput::Move { nx, ny } => s.last_pos = (*nx, *ny),
            VideoInput::Wheel { .. } => {}
        }
        match &s.input_sink {
            Some(tx) => tx.send(ev).is_ok(),
            None => false,
        }
    }

    pub fn user_drop(&self, drop: FileDrop) -> bool {
        match &lock(&self.state).drop_sink {
            Some(tx) => tx.send(drop).is_ok(),
            None => false,
        }
    }

    pub fn set_client_size(&self, size: (u32, u32)) {
        lock(&self.state).client_size = size;
    }
}

impl VideoSurface for MockSurface {
    fn show(&self) {
        lock(&self.state).visible = true;
    }
    fn hide(&self) {
        let mut s = lock(&self.state);
        s.visible = false;
    }
    fn fit(&self) {
        lock(&self.state).fits += 1;
    }
    fn client_size(&self) -> (u32, u32) {
        lock(&self.state).client_size
    }
    fn set_input_sink(&self, sink: Option<Sender<VideoInput>>) {
        let mut s = lock(&self.state);
        if sink.is_none() {
            // Contract: release everything still held before dropping the sink.
            if let Some(tx) = s.input_sink.take() {
                for code in std::mem::take(&mut s.held_keys) {
                    let _ = tx.send(VideoInput::Key { code, down: false });
                }
                let (nx, ny) = s.last_pos;
                for button in std::mem::take(&mut s.held_buttons) {
                    let _ = tx.send(VideoInput::Button {
                        button,
                        down: false,
                        nx,
                        ny,
                    });
                }
            }
        }
        s.input_sink = sink;
    }
    fn set_drop_sink(&self, sink: Option<Sender<FileDrop>>) {
        lock(&self.state).drop_sink = sink;
    }
    fn create_presenter(&self) -> Result<Box<dyn Presenter>, SurfaceError> {
        Ok(Box::new(MockPresenter {
            state: self.state.clone(),
        }))
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Records each presented frame's stamp into the surface state.
#[derive(Debug)]
pub struct MockPresenter {
    state: Arc<Mutex<SurfaceState>>,
}

impl Presenter for MockPresenter {
    fn present(
        &mut self,
        frame: &Frame<'_>,
        cursor: Option<(f64, f64)>,
    ) -> Result<(), SurfaceError> {
        let stamp = match frame.data {
            FrameData::Cpu {
                plane0,
                format: PixelFormat::Nv12 | PixelFormat::Bgra8,
                ..
            } => read_stamp(plane0.data),
            FrameData::Native(_) => None,
        };
        lock(&self.state).presented.push(Presented {
            stamp,
            width: frame.width,
            height: frame.height,
            cursor,
        });
        Ok(())
    }
    fn device(&self) -> Option<&dyn Any> {
        None
    }
}

// ---------------------------------------------------------------------------
// Injector
// ---------------------------------------------------------------------------

/// What the mock injector did.
#[derive(Debug, Default)]
pub struct InjectorLog {
    /// Every injected event, including releases emitted by `release_all`.
    pub events: Vec<InputMsg>,
    pub target: Option<Rect>,
    pub release_all_calls: u64,
    pub focus_requests: Vec<(f64, f64)>,
    held_keys: BTreeSet<String>,
    held_buttons: BTreeSet<u8>,
}

impl InjectorLog {
    pub fn held_keys(&self) -> &BTreeSet<String> {
        &self.held_keys
    }
}

pub struct MockInjector {
    log: Arc<Mutex<InjectorLog>>,
    focus_result: bool,
}

impl InputInjector for MockInjector {
    fn set_target_rect(&mut self, rect: Option<Rect>) {
        lock(&self.log).target = rect.filter(|r| r.w > 0 && r.h > 0);
    }
    fn dispatch(&mut self, msg: &InputMsg) {
        let mut l = lock(&self.log);
        match msg {
            InputMsg::KeyDown { code } => {
                l.held_keys.insert(code.clone());
            }
            InputMsg::KeyUp { code } => {
                l.held_keys.remove(code);
            }
            InputMsg::ButtonDown { b, .. } => {
                l.held_buttons.insert(u8::from(*b));
            }
            InputMsg::ButtonUp { b, .. } => {
                l.held_buttons.remove(&u8::from(*b));
            }
            InputMsg::Move { .. } | InputMsg::Wheel { .. } => {}
        }
        l.events.push(msg.clone());
    }
    fn release_all(&mut self) {
        let mut l = lock(&self.log);
        l.release_all_calls += 1;
        for code in std::mem::take(&mut l.held_keys) {
            l.events.push(InputMsg::KeyUp { code });
        }
        for b in std::mem::take(&mut l.held_buttons) {
            if let Ok(b) = Button::try_from(b) {
                l.events.push(InputMsg::ButtonUp { b, x: 0.0, y: 0.0 });
            }
        }
    }
    fn focus_window_at(&mut self, nx: f64, ny: f64) -> bool {
        lock(&self.log).focus_requests.push((nx, ny));
        self.focus_result
    }
}

// ---------------------------------------------------------------------------
// Clipboard, secrets, autostart
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct ClipboardState {
    pub text: Option<String>,
    pub files: Vec<PathBuf>,
    pub count: u64,
}

/// Shared in-memory clipboard. Clones share state.
#[derive(Debug, Clone, Default)]
pub struct MockClipboard(pub Arc<Mutex<ClipboardState>>);

impl MockClipboard {
    /// Simulate the local user copying text in another app.
    pub fn user_copies(&self, text: &str) {
        let mut s = lock(&self.0);
        s.text = Some(text.to_owned());
        s.files.clear();
        s.count += 1;
    }
}

impl Clipboard for MockClipboard {
    fn change_count(&self) -> u64 {
        lock(&self.0).count
    }
    fn get_text(&self) -> Option<String> {
        lock(&self.0)
            .text
            .clone()
            .filter(|t| !t.is_empty() && t.len() <= MAX_TEXT_BYTES)
    }
    fn set_text(&self, text: &str) -> Option<u64> {
        let mut s = lock(&self.0);
        s.text = Some(text.to_owned());
        s.files.clear();
        s.count += 1;
        Some(s.count)
    }
    fn set_files(&self, paths: &[PathBuf]) -> Option<u64> {
        if paths.is_empty() {
            return None;
        }
        let mut s = lock(&self.0);
        s.text = None;
        s.files = paths.to_vec();
        s.count += 1;
        Some(s.count)
    }
}

/// Reversible, clearly-not-plaintext sealing for tests.
#[derive(Debug, Default)]
pub struct MockSecretStore;

const SEAL_PREFIX: &str = "mock1:";

impl SecretStore for MockSecretStore {
    fn seal(&self, plain: &str) -> Option<String> {
        let x: Vec<u8> = plain.bytes().map(|b| b ^ 0x5a).collect();
        Some(format!("{SEAL_PREFIX}{}", hex(&x)))
    }
    fn open(&self, sealed: &str) -> Option<String> {
        let body = sealed.strip_prefix(SEAL_PREFIX)?;
        let bytes = unhex(body.as_bytes())?;
        String::from_utf8(bytes.into_iter().map(|b| b ^ 0x5a).collect()).ok()
    }
}

#[derive(Debug, Default)]
pub struct MockAutostart {
    pub enabled: Mutex<bool>,
    pub reconciles: Mutex<u64>,
}

impl Autostart for MockAutostart {
    fn is_enabled(&self) -> bool {
        *lock(&self.enabled)
    }
    fn set_enabled(&self, on: bool) -> Result<(), String> {
        *lock(&self.enabled) = on;
        Ok(())
    }
    fn reconcile(&self, want: bool) {
        *lock(&self.reconciles) += 1;
        *lock(&self.enabled) = want;
    }
}

// ---------------------------------------------------------------------------
// Platform
// ---------------------------------------------------------------------------

/// A whole fake OS. Public fields are the shared state tests inspect.
pub struct MockPlatform {
    pub paths: Paths,
    pub capture: MockCaptureConfig,
    pub surface: Arc<MockSurface>,
    pub injector: Arc<Mutex<InjectorLog>>,
    pub clipboard: MockClipboard,
    pub autostart: Arc<MockAutostart>,
    pub permissions: Mutex<PermissionReport>,
    pub permission_requests: Mutex<Vec<OsPermission>>,
    /// What `focus_window_at` returns.
    pub focus_result: bool,
    codecs: MockCodecFactory,
    secrets: Arc<MockSecretStore>,
}

impl MockPlatform {
    /// All paths live under `root` (use a per-test temp dir).
    pub fn new(root: &Path) -> Self {
        Self {
            paths: Paths {
                config_dir: root.join("config"),
                log_dir: root.join("logs"),
                downloads_dir: root.join("downloads"),
            },
            capture: MockCaptureConfig::default(),
            surface: MockSurface::new((640, 360)),
            injector: Arc::default(),
            clipboard: MockClipboard::default(),
            autostart: Arc::default(),
            permissions: Mutex::new(PermissionReport {
                screen_capture: PermissionState::Granted,
                input_injection: PermissionState::Granted,
                input_monitoring: PermissionState::NotApplicable,
                notes: Vec::new(),
            }),
            permission_requests: Mutex::default(),
            focus_result: true,
            codecs: MockCodecFactory,
            secrets: Arc::new(MockSecretStore),
        }
    }
}

impl Platform for MockPlatform {
    fn info(&self) -> PlatformInfo {
        PlatformInfo {
            name: "mock",
            start_at_login_label: "Start at sign-in",
            shortcut_capture_hint: "Shortcut capture is not available in tests.",
            login_screen_supported: false,
        }
    }
    fn paths(&self) -> Paths {
        self.paths.clone()
    }
    fn displays(&self) -> Vec<DisplayInfo> {
        vec![DisplayInfo {
            id: "mock-0".into(),
            name: "Mock display".into(),
            x: 0,
            y: 0,
            width: self.capture.width,
            height: self.capture.height,
            scale: 1.0,
            primary: true,
        }]
    }
    fn permissions(&self) -> PermissionReport {
        lock(&self.permissions).clone()
    }
    fn request_permission(&self, p: OsPermission) {
        lock(&self.permission_requests).push(p);
    }
    fn create_capture(
        &self,
        display: Option<&str>,
    ) -> Result<Box<dyn ScreenCapture>, CaptureError> {
        if display.is_some_and(|d| d != "mock-0") {
            return Err(CaptureError::NoOutput);
        }
        if lock(&self.permissions).screen_capture == PermissionState::Denied {
            return Err(CaptureError::PermissionDenied);
        }
        Ok(Box::new(MockCapture::new(self.capture.clone())))
    }
    fn codecs(&self) -> &dyn CodecFactory {
        &self.codecs
    }
    fn create_surface(
        &self,
        _window: RawWindowHandle,
        _display: RawDisplayHandle,
    ) -> Result<Arc<dyn VideoSurface>, SurfaceError> {
        Ok(self.surface.clone())
    }
    fn create_injector(&self) -> Result<Box<dyn InputInjector>, InputError> {
        if lock(&self.permissions).input_injection == PermissionState::Denied {
            return Err(InputError::PermissionDenied("mock: denied".into()));
        }
        Ok(Box::new(MockInjector {
            log: self.injector.clone(),
            focus_result: self.focus_result,
        }))
    }
    fn create_shortcut_hook(&self) -> Option<Box<dyn ShortcutHook>> {
        None
    }
    fn create_clipboard(&self) -> Box<dyn Clipboard> {
        Box::new(self.clipboard.clone())
    }
    fn secrets(&self) -> Arc<dyn SecretStore> {
        self.secrets.clone()
    }
    fn autostart(&self) -> Arc<dyn Autostart> {
        self.autostart.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn dummy_handles() -> (RawWindowHandle, RawDisplayHandle) {
        (
            RawWindowHandle::Web(raw_window_handle::WebWindowHandle::new(1)),
            RawDisplayHandle::Web(raw_window_handle::WebDisplayHandle::new()),
        )
    }

    #[test]
    fn capture_frames_are_stamped_and_dirty() {
        let mut c = MockCapture::new(MockCaptureConfig {
            fps: 1000,
            ..Default::default()
        });
        for expect in 0..5u64 {
            let f = c
                .acquire(Duration::from_millis(50))
                .unwrap()
                .expect("frame");
            let FrameData::Cpu { plane0, .. } = f.frame.data else {
                panic!()
            };
            assert_eq!(read_stamp(plane0.data), Some(expect));
            assert!(!f.dirty.is_empty());
            assert!(f.dirty.iter().all(|r| r.x as u32 + r.w <= 640));
            c.release();
        }
        assert_eq!(c.releases, 5);
    }

    #[test]
    fn capture_goes_static_and_reports_lost_once() {
        let mut c = MockCapture::new(MockCaptureConfig {
            fps: 1000,
            static_after: Some(3),
            lose_at: Some(1),
            ..Default::default()
        });
        assert!(c.acquire(Duration::from_millis(20)).unwrap().is_some());
        assert!(matches!(
            c.acquire(Duration::from_millis(20)),
            Err(CaptureError::Lost)
        ));
        c.reinit().unwrap();
        assert!(c.acquire(Duration::from_millis(20)).unwrap().is_some());
        assert!(c.acquire(Duration::from_millis(20)).unwrap().is_some());
        assert!(c.acquire(Duration::from_millis(5)).unwrap().is_none());
        assert_eq!(c.reinits, 1);
    }

    #[test]
    fn mock_codec_roundtrips_stamps_and_gates_on_keyframes() {
        let f = MockCodecFactory;
        let mut enc = f.new_encoder(None, EncoderConfig::default()).unwrap();
        let mut dec = f.new_decoder(None, Codec::H264, 2, 2).unwrap();
        let mut px = vec![0u8; 8 * 2 * 4];

        let mut units = Vec::new();
        for stamp in 0..4u64 {
            px[..8].copy_from_slice(&stamp.to_le_bytes());
            let mut u = enc.encode(&Frame::bgra(8, 2, &px, 32)).unwrap();
            assert_eq!(u.len(), 1);
            units.push(u.remove(0));
        }
        assert!(units[0].keyframe && !units[1].keyframe);
        // Real H.264 framing: SPS, PPS, IDR on the keyframe.
        let types: Vec<u8> = split_annex_b(&units[0].data)
            .iter()
            .map(|n| n[0] & 0x1f)
            .collect();
        assert_eq!(types, vec![7, 8, 5]);
        assert!(!units[1].data[4..].windows(3).any(|w| w == [0, 0, 1]));

        // A delta before any keyframe decodes to nothing.
        assert!(dec.decode(&units[1].data, false, 0).unwrap().is_none());
        let frame = dec
            .decode(&units[0].data, true, 0)
            .unwrap()
            .expect("key decodes");
        assert_eq!((frame.width, frame.height), (8, 2));
        let FrameData::Cpu { plane0, .. } = frame.data else {
            panic!()
        };
        assert_eq!(read_stamp(plane0.data), Some(0));
        let frame = dec
            .decode(&units[3].data, false, 0)
            .unwrap()
            .expect("delta decodes");
        let FrameData::Cpu { plane0, .. } = frame.data else {
            panic!()
        };
        assert_eq!(read_stamp(plane0.data), Some(3));

        enc.force_keyframe();
        let u = enc.encode(&Frame::bgra(8, 2, &px, 32)).unwrap();
        assert!(u[0].keyframe);
    }

    #[test]
    fn encoder_output_tracks_bitrate() {
        let f = MockCodecFactory;
        let cfg = EncoderConfig {
            bitrate_bps: 2_400_000,
            fps_num: 30,
            ..Default::default()
        };
        let mut enc = f.new_encoder(None, cfg).unwrap();
        let px = vec![0u8; 32];
        let key = enc.encode(&Frame::bgra(2, 2, &px, 8)).unwrap().remove(0);
        let delta = enc.encode(&Frame::bgra(2, 2, &px, 8)).unwrap().remove(0);
        assert!(
            (9_000..11_000).contains(&delta.data.len()),
            "{}",
            delta.data.len()
        );
        assert!(key.data.len() > 3 * delta.data.len());
        enc.set_bitrate(240_000).unwrap();
        let small = enc.encode(&Frame::bgra(2, 2, &px, 8)).unwrap().remove(0);
        assert!(small.data.len() < 1_100);
    }

    #[test]
    fn decoder_rejects_garbage() {
        let mut dec = MockCodecFactory
            .new_decoder(None, Codec::H264, 2, 2)
            .unwrap();
        assert!(dec
            .decode(b"\x00\x00\x00\x01\x65nonsense", true, 0)
            .is_err());
        assert!(dec.decode(b"no start code", true, 0).is_err());
    }

    #[test]
    fn surface_clearing_the_sink_releases_held_input() {
        let s = MockSurface::new((800, 600));
        let (tx, rx) = mpsc::channel();
        s.set_input_sink(Some(tx));
        assert!(s.user_input(VideoInput::Key {
            code: "ShiftLeft",
            down: true
        }));
        assert!(s.user_input(VideoInput::Button {
            button: 0,
            down: true,
            nx: 0.5,
            ny: 0.25
        }));
        s.set_input_sink(None);
        let got: Vec<_> = rx.try_iter().collect();
        assert!(got.contains(&VideoInput::Key {
            code: "ShiftLeft",
            down: false
        }));
        assert!(got.contains(&VideoInput::Button {
            button: 0,
            down: false,
            nx: 0.5,
            ny: 0.25
        }));
        assert!(
            !s.user_input(VideoInput::Move { nx: 0.0, ny: 0.0 }),
            "no sink, no capture"
        );
    }

    #[test]
    fn show_hide_toggle_visibility_only() {
        let s = MockSurface::new((800, 600));
        s.show();
        assert!(s.state().visible);
        s.hide();
        assert!(!s.state().visible);
        assert_eq!(s.client_size(), (800, 600));
    }

    #[test]
    fn injector_release_all_emits_real_releases() {
        let p = MockPlatform::new(Path::new("/tmp/unused"));
        let mut inj = p.create_injector().unwrap();
        inj.dispatch(&InputMsg::KeyDown {
            code: "KeyA".into(),
        });
        inj.dispatch(&InputMsg::ButtonDown {
            b: Button::Right,
            x: 0.1,
            y: 0.2,
        });
        inj.release_all();
        let log = p.injector.lock().unwrap();
        assert!(log.held_keys().is_empty());
        assert!(log
            .events
            .iter()
            .any(|e| matches!(e, InputMsg::KeyUp { code } if code == "KeyA")));
        assert!(log.events.iter().any(|e| matches!(
            e,
            InputMsg::ButtonUp {
                b: Button::Right,
                ..
            }
        )));
    }

    #[test]
    fn clipboard_counter_supports_the_echo_guard() {
        let c = MockClipboard::default();
        let before = c.change_count();
        let after = c.set_text("hello").unwrap();
        assert_eq!(after, before + 1);
        assert_eq!(
            c.change_count(),
            after,
            "returned count is the post-write count"
        );
        assert_eq!(c.set_files(&[PathBuf::from("/x")]), Some(after + 1));
        assert_eq!(c.get_text(), None, "a file drop leaves no text behind");
        assert_eq!(c.set_files(&[]), None);
        c.user_copies(&"x".repeat(MAX_TEXT_BYTES + 1));
        assert_eq!(
            c.get_text(),
            None,
            "oversized text is refused, not truncated"
        );
    }

    #[test]
    fn secret_store_never_stores_plaintext_and_rejects_garbage() {
        let s = MockSecretStore;
        let sealed = s.seal("hunter2").unwrap();
        assert!(!sealed.contains("hunter2"));
        assert_eq!(s.open(&sealed).as_deref(), Some("hunter2"));
        assert_eq!(s.open("not-sealed"), None);
        assert_eq!(s.open("mock1:zz"), None);
    }

    #[test]
    fn platform_wires_shared_state() {
        let p = MockPlatform::new(Path::new("/tmp/unused"));
        let (w, d) = dummy_handles();
        let surface = p.create_surface(w, d).unwrap();
        let mut pres = surface.create_presenter().unwrap();
        let y = 7u64.to_le_bytes().repeat(2);
        pres.present(&Frame::nv12(4, 2, &y, 8, &[128; 8], 4), Some((0.5, 0.5)))
            .unwrap();
        assert_eq!(p.surface.state().presented[0].stamp, Some(7));

        *p.permissions.lock().unwrap() = PermissionReport {
            screen_capture: PermissionState::Denied,
            ..PermissionReport::all_not_applicable()
        };
        assert!(matches!(
            p.create_capture(None),
            Err(CaptureError::PermissionDenied)
        ));
        assert!(matches!(
            p.create_capture(Some("nope")),
            Err(CaptureError::NoOutput)
        ));
        p.request_permission(OsPermission::ScreenCapture);
        assert_eq!(
            *p.permission_requests.lock().unwrap(),
            vec![OsPermission::ScreenCapture]
        );
    }
}
