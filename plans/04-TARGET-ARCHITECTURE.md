# 04 — Target architecture for a 3-OS build

This is the design every task in [05-TASKS.md](05-TASKS.md) builds toward. It keeps the
Windows product exactly as it behaves today and adds macOS and Linux as *backends* behind
one set of traits. Nothing here changes the wire protocol (except one additive optional
field, `connect-response.os`, §6, and the additive `curshape` control message of D9, §2),
the signaling server, or the
Tauri UI contract (commands/events) — those were verified portable by the code sweep
([07-CODEBASE-SEAMS.md](07-CODEBASE-SEAMS.md)).

## 1. What stays, what moves

| Layer | Today | Target |
|---|---|---|
| `protocol`, `transport` (str0m RTP + data channels, FEC), `signaling`, `engine::turn`, `engine::handshake`, `engine::transfer`, Tauri UI (`app/ui`) | portable, unit-tested | **unchanged** |
| `engine::pipeline` (2 509 lines) | one Windows-only file: ICE gathering + transport driver + media loops + Win32 calls | split into `engine::session` (portable: ICE, STUN/TURN, transport driver, liveness, bitrate policy, keyframe gate, file push, clipboard loop, input translation) and `engine::media` (portable host/viewer media loops that call traits) |
| `capture`, `codec`, `render`, `input`, `engine::{clipboard,secret,autostart}` | `#![cfg(windows)]` crates / modules | become the **Windows implementation** of the traits in a new `platform-api` crate; sibling crates `platform-macos`, `platform-linux` implement the same traits |
| `elevation`, `service` | SYSTEM service for the secure desktop | Windows-only, unchanged. No macOS/Linux equivalent (see §9) |
| `app` (Tauri shell) | `hwnd()` + `%APPDATA%` + Run-key autostart | per-OS surface creation via raw-window-handle; `dirs`-based paths; autostart via trait |

## 2. The `platform-api` crate (the contract every lane codes against)

> **Implemented** in `native/crates/platform-api` — read the crate, not this
> sketch, for exact signatures. Differences are listed in 05-TASKS § Status.

Create `native/crates/platform-api` first (task **F1**). It has **no OS dependencies**, only
`protocol`, `thiserror`, `std`. Everything below is derived from the call sites the sweep
found; line references are in [07-CODEBASE-SEAMS.md](07-CODEBASE-SEAMS.md).

```rust
// ---- geometry / neutral types (replace windows POINT/RECT in public APIs) ----
pub struct Point { pub x: i32, pub y: i32 }
pub struct Rect  { pub x: i32, pub y: i32, pub w: u32, pub h: u32 }
pub struct MoveRect { pub src: Point, pub dst: Rect }
pub struct CursorShape { pub w: u32, pub h: u32, pub hotspot: Point, pub bgra: Vec<u8> } // DDA monochrome/masked shapes are converted to BGRA by the Windows backend
pub struct CursorUpdate { pub visible: bool, pub position: Point, pub shape: Option<CursorShape> }

// ---- GPU frame handle: opaque per backend, zero-copy inside one backend ----
pub enum GpuFrame<'a> {
    #[cfg(windows)] D3d11 { texture: &'a ID3D11Texture2D, array_index: u32 },
    #[cfg(target_os = "macos")] CvPixelBuffer(&'a CVPixelBufferRef),          // IOSurface-backed
    #[cfg(target_os = "linux")] DmaBuf { planes: &'a [DmaBufPlane], fourcc: u32, modifier: u64, w: u32, h: u32 },
    Cpu { format: PixelFormat /* Bgra8 | Nv12 */, planes: &'a [CpuPlane], w: u32, h: u32 },  // software paths, tests, VMs
}
```
(Backend-specific variants live behind `cfg`, so `platform-api` still has no OS deps at
the type level; the handle types are re-exported from thin `-sys` crates.)

```rust
pub trait ScreenCapture: Send {
    fn dimensions(&self) -> (u32, u32);                       // real pixels, encoder MUST use this (capture lib.rs:119-123)
    fn acquire(&mut self, timeout: Duration) -> Result<Option<CapturedFrame<'_>>, CaptureError>; // Ok(None) = nothing changed
    fn release(&mut self);                                    // idempotent; MUST be called before next acquire
    fn reinit(&mut self) -> Result<(), CaptureError>;         // after CaptureError::Lost
    fn device(&self) -> &GpuDevice;                           // shared with the encoder (zero-copy invariant)
}
pub struct CapturedFrame<'a> { pub frame: GpuFrame<'a>, pub dirty: Vec<Rect>, pub moves: Vec<MoveRect>, pub pointer_only: bool, pub cursor: Option<CursorUpdate> }
pub enum CaptureError { Lost, PermissionDenied, SecureDesktop, NoOutput, Backend(String) }

pub trait VideoEncoder: Send {
    fn encode(&mut self, frame: &GpuFrame<'_>) -> Result<Vec<EncodedUnit>, CodecError>; // may return 0 units
    fn set_bitrate(&mut self, bps: u32) -> Result<(), CodecError>;
    fn force_keyframe(&mut self);
}
pub trait VideoDecoder: Send {
    fn decode(&mut self, au: &[u8], keyframe: bool, pts_100ns: i64) -> Result<Option<DecodedFrame<'_>>, CodecError>;
    fn dimensions(&self) -> (u32, u32);                       // may change mid-stream; re-query after each Some
}
pub trait CodecFactory {                                       // one per backend; the engine picks by capability
    fn host_encodable(&self) -> Vec<Codec>;                   // H264 always last/guaranteed (via software fallback)
    fn viewer_decodable(&self) -> Vec<Codec>;                 // AV1/HEVC only if hardware
    fn new_encoder(&self, dev: &GpuDevice, cfg: EncoderConfig) -> Result<Box<dyn VideoEncoder>, CodecError>;
    fn new_decoder(&self, dev: &GpuDevice, codec: Codec, w: u32, h: u32) -> Result<Box<dyn VideoDecoder>, CodecError>;
}

pub trait VideoSurface: Send + Sync {                           // the child surface under the Tauri window
    fn create(parent: RawWindowHandle) -> Result<Self, SurfaceError> where Self: Sized; // UI thread
    fn show(&self); fn hide(&self); fn fit(&self);            // callable from the render thread; hide clears clip; show re-fits
    fn client_size(&self) -> (u32, u32);                      // invariant under the hover-bar clip
    fn set_top_clip(&self, css_px: u32);                      // hover-bar reveal (render window.rs:233-253)
    fn set_input_sink(&self, tx: Option<Sender<VideoInput>>);  // None = clear; clears held state WITH releases
    fn set_drop_sink(&self, tx: Option<Sender<FileDrop>>);
    fn presenter(&self, dev: &GpuDevice) -> Result<Box<dyn Presenter>, SurfaceError>;
}
pub trait Presenter: Send {
    fn present(&mut self, frame: &DecodedFrame<'_>, cursor: Option<(f64, f64)>) -> Result<(), SurfaceError>; // letterbox + NV12→RGB + cursor sprite
    fn resize(&mut self, w: u32, h: u32) -> Result<(), SurfaceError>;
}
pub enum VideoInput { Move{nx,ny}, Button{button:u8,down,nx,ny}, Wheel{dx,dy}, Key{code: KeyId, down: bool} }
pub struct KeyId(pub u16 /* USB HID usage, see §6 */);

pub trait InputInjector: Send {
    fn set_target_rect(&mut self, r: Option<Rect>);
    fn dispatch(&mut self, msg: &InputMsg);                   // DOM code strings on the wire; backend maps via §6 table
    fn release_all(&mut self);                                // MUST emit real release events
    fn focus_window_at(&self, nx: f64, ny: f64) -> bool;      // focus WITHOUT clicking (pipeline.rs:1918-1919)
}
pub trait ShortcutHook {                                       // viewer side: swallow Win/Cmd/Super, Alt+Tab, Cmd+Tab while focused
    fn install(&mut self, focus_root: RawWindowHandle, forward: Box<dyn Fn(&str, bool) + Send>) -> bool; // false = unsupported here
    fn run(&mut self, stop: &AtomicBool);                     // blocking pump on the installing thread
    fn uninstall(&mut self);
}
pub trait Clipboard: Send {
    fn change_count(&self) -> u64;                            // MUST advance after our own writes and only once committed (echo guard)
    fn get_text(&self) -> Option<String>;
    fn set_text(&self, s: &str) -> Option<u64>;               // returns the new change_count
    fn set_files(&self, paths: &[PathBuf]) -> Option<u64>;    // real file references (CF_HDROP / file-URL / text/uri-list)
}
pub trait SecretStore { fn seal(&self, plain: &str) -> Option<String>; fn open(&self, sealed: &str) -> Option<String>; } // None ≠ plaintext
pub trait Autostart { fn is_enabled(&self) -> bool; fn set_enabled(&self, on: bool) -> Result<(), String>; fn reconcile(&self, want: bool); }
pub struct Paths { pub config_dir: PathBuf, pub log_dir: PathBuf, pub downloads_dir: PathBuf }
pub trait Platform {                                           // the one object `app` constructs
    fn name(&self) -> &'static str;                           // "windows" | "macos" | "linux-x11" | "linux-wayland"
    fn paths(&self) -> Paths;
    fn capture(&self, display: Option<&str>) -> Result<Box<dyn ScreenCapture>, CaptureError>;
    fn codecs(&self) -> &dyn CodecFactory;
    fn surface(&self, parent: RawWindowHandle) -> Result<Arc<dyn VideoSurface>, SurfaceError>;
    fn injector(&self) -> Result<Box<dyn InputInjector>, InputError>;   // Err(PermissionDenied) on macOS without Accessibility
    fn shortcut_hook(&self) -> Option<Box<dyn ShortcutHook>>;
    fn clipboard(&self) -> Box<dyn Clipboard>;
    fn secrets(&self) -> Box<dyn SecretStore>;
    fn autostart(&self) -> Box<dyn Autostart>;
    fn permissions(&self) -> PermissionReport;                 // §8 — drives the new UI preflight panel
    fn displays(&self) -> Vec<DisplayInfo>;                    // fills #set-share-display (today empty)
}
```

Rules that every implementation must keep (each is a known hard-won invariant in the
current code; the sweep lists the line for each):

1. **Encoder is sized from `ScreenCapture::dimensions()`**, never assumed 1920×1080.
2. `acquire` → `Ok(None)` on "nothing changed" costs no encode; `Lost` → `reinit` → force keyframe.
3. `release()` must run on every path, including the early-continue when nothing changed.
4. Keyframes only on demand, rate-limited (1 s host side, 500 ms PLI viewer side); the first
   frame after `Connected` must be an IDR (`force_key`).
5. Bitrate target is clamped to [2.5, 8] Mbps with 15 % hysteresis; the pacer uses the raw
   link estimate — keep both numbers separate.
6. `release_all()` on control revocation and at session end, and `set_input_sink(None)` must
   emit releases for held keys (fixes a documented gap in `window.rs:308`).
7. Clipboard echo guard: `set_text` returns the counter **after** the write is committed.
8. Secret store failure → do not save, toast; never plaintext.
9. One session at a time; teardown order: clear sinks → stop flag → join → hide surface.
10. Hover-bar clip never changes `client_size()`, so the host encode size stays stable.

Additive contract changes made after this design (each approved by the orchestrator):

* **D9, host cursor image (2026-11-01).** `Presenter::set_cursor_sprite(&mut self,
  sprite: Option<&CursorSprite>) -> Result<(), SurfaceError>`, default `Ok(())` (a
  presenter that ignores it keeps the built-in arrow). New `CursorSprite { width, height,
  hotspot: Point, bgra: Vec<u8> /* premultiplied */, frame: (u32, u32) }` with
  `is_valid()` and `placement(cursor) -> Option<[f64; 4]>` (the image rectangle as
  fractions of the video, top-left = pointer − hotspot, size = image / host frame size),
  and `MAX_CURSOR_SIDE` (= `protocol::cursor::MAX_SIDE`, 256). Doc-only fix:
  `CursorUpdate.position` is the **hotspot** in captured-frame pixels on every backend
  (DXGI reports the image's top-left; the Windows adapter adds the hotspot), and
  `CursorShape` sizes/hotspot are captured-frame pixels with straight alpha.
* **D9 wire protocol.** One additive control message, `{"t":"curshape", id, w, h, hx, hy,
  fw, fh, part, parts, data}` (`ControlMsg::CursorShape`), on the reliable ordered `ctl`
  channel — not the `cursor` channel, which never retransmits and would leave a lost image
  wrong until the next change. `data` is lower-case hex of premultiplied BGRA, 16 KiB of
  pixels per part (≤ 16 parts, each message ~32 KiB, under the 64 KiB SCTP default);
  images over 256 px per side are not sent (the viewer draws the arrow). Positions stay
  on the `cursor` channel unchanged; their existing optional `shape` field now names the
  image id (`None` = arrow). Both ends keep the last 32 ids in insertion order, so a
  cursor seen before is switched to by id alone. Older viewers fail to parse `curshape`
  and drop it; older hosts never send it and never set `shape`, so the viewer keeps its
  arrow. Details: `protocol/src/cursor.rs`.

## 3. Media data paths per OS (zero-copy where it matters)

```
HOST     capture ──────────────► encoder ─────► transport (RTP over str0m)
Windows  DXGI DDA (ID3D11Texture2D) ─ GPU BGRA→NV12 (VideoProcessor) ─ MF HW MFT          (today)
macOS    ScreenCaptureKit SCStream → CVPixelBuffer (IOSurface, NV12 or BGRA, compositor-scaled)
         ─► VTCompressionSession (RealTime, no B-frames, LowLatencyRateControl) — zero-copy
Linux    Wayland: portal ScreenCast → PipeWire DMA-BUF (or MemFd fallback)
         X11: XShm + XDamage (CPU BGRA, dirty rects from Damage events)
         ─► VA-API (cros-libva): import DMA-BUF as VASurface (DRM_PRIME_2) or upload CPU BGRA
            → VPP BGRA→NV12 → H.264 encode. NVIDIA: NVENC (libnvidia-encode, CUDA interop) or
            the nvidia-vaapi-driver shim. Fallback everywhere: software H.264 (§5)

VIEWER   transport ─► decoder ─────► presenter
Windows  MF D3D11VA → NV12 texture array → D3D11 flip swapchain on child HWND                (today)
macOS    VTDecompressionSession → CVPixelBuffer → CVMetalTextureCache (R8 + RG8 planes) →
         wgpu (Metal backend, `create_texture_from_hal`) on a CAMetalLayer-backed NSView — zero-copy
Linux    VA-API decode → v1: vaDeriveImage/Map → CPU NV12 → wgpu upload (≈1–2 ms @1080p);
         v2 (stretch task): vaExportSurfaceHandle DMA-BUF → Vulkan external memory via wgpu-hal
         presenter: wgpu (Vulkan, GL fallback) on an X11 child window / Wayland wl_subsurface
```

The presenter shader is one WGSL file (NV12 BT.709 limited-range → RGB + letterbox +
cursor triangle), a port of the HLSL at `render/src/lib.rs:51-105`. Windows keeps D3D11.

## 4. The video surface under the Tauri window (the hardest UI piece)

Today: a hidden `WS_CHILD` HWND created under the Tauri HWND, z-ordered **above** WebView2,
sized to the client area; the web bar is revealed by clipping the child's region at the top
(`render/src/window.rs`). The engine hides it while modals are open. Keep that design:

| OS | Surface | Input capture | Hover-bar reveal |
|---|---|---|---|
| Windows | child HWND (today) | wndproc | `SetWindowRgn` |
| macOS | `NSView` subclass with `CAMetalLayer`, added to `ns_view()`'s superview **above** the `WKWebView` (`addSubview:positioned:NSWindowAbove`). `wantsLayer = YES`. | override `mouseDown/Up/Moved/Dragged`, `scrollWheel`, `keyDown/Up`, `flagsChanged`; make it first responder on click | set the view's `frame` to exclude the top strip (CAMetalLayer with `contentsGravity = bottom`) or a `CAShapeLayer` mask; `client_size()` reports the unclipped size |
| Linux X11 | `XCreateWindow` child of the GTK toplevel XID (`gtk_window().window().xid()`), mapped above WebKitGTK (wry's `wgpu.rs` example proves this works on X11) | X11 event mask on the child (ButtonPress/Motion/Key*) via x11rb on our own thread | `XShapeCombineRectangles` (XShape) |
| Linux Wayland | `wl_subsurface` of the GTK toplevel's `wl_surface` (`gdk_wayland_window_get_wl_surface`), `place_above`, desync mode; wgpu surface from `WaylandWindowHandle` — same technique as GStreamer's `gtkwaylandsink` | pointer: the subsurface receives `wl_pointer` events on its own wl_surface; keyboard: focus stays on the GTK toplevel → connect `key-press-event`/`key-release-event` on `gtk_window()` and return `Inhibit(true)` while a session is active | `wl_surface.set_input_region` + an opaque region / resize the subsurface |

**Spike first (task L5-spike):** the Wayland subsurface path is the only one without a
public reference in the Rust/Tauri ecosystem (a closed raw-window-handle issue argues it is
"unsupported upstream", but `gtkwaylandsink` ships exactly this). The spike has ordered
fallbacks: (1) subsurface; (2) a separate undecorated video `winit` window that the UI
toggles (loses the overlay UX on Wayland only); (3) in-webview WebCodecs decode (WebKitGTK
2.54 has WebCodecs via GStreamer; decoder crashes take the web process down — last resort).
X11 needs no spike.

**Spike result (2026-11-01, [spikes/L5-wayland-subsurface.md](spikes/L5-wayland-subsurface.md)):**
option (1) works on wlroots (sway) and Weston, with the `VideoSurface` contract unchanged.
GNOME/KDE still need the device pass before L5 is done. Design corrections to the Linux
Wayland row above: use an `area` + `video` subsurface pair (gtkwaylandsink layout) so the
reveal crop is one atomic commit we own; do **not** call `place_above(parent)` (a new
subsurface is already top-most; that call would lower it below sibling subsurfaces); bind our
own `wl_seat`/`wl_pointer` on our own queue over GDK's connection; create the video
`wl_surface` through libwayland with a widget-less `GdkWindow` as its user data (GDK 3's touch
path casts it unchecked). Decision #3 stays as a runtime fallback (missing
`wp_viewporter`, or a compositor that fails the hide check).

Tauri 2.11 exposes everything needed: `WebviewWindow::{hwnd, ns_window, ns_view,
gtk_window}` (verified in `tauri-2.11.5/src/webview/webview_window.rs:1835-1862`) plus
`HasWindowHandle`.

## 5. Codec policy

* Negotiation stays as is (`AV1 → HEVC → H264`, H264 guaranteed). Each backend's
  `CodecFactory` reports **real** capabilities (VideoToolbox: H264/HEVC encode on all
  Apple Silicon, AV1 decode only on M3+; VA-API: query `vaQueryConfigProfiles`).
* **Software H.264 fallback on all three OSes** (task D2): the `openh264` crate with the
  `libloading` feature, loading **Cisco's binary downloaded from Cisco at first use** (the
  Firefox model). Cisco's binary license covers MPEG-LA/Via royalties *only* for the binary
  "separately downloaded to the end user's device", the app must show "OpenH264 Video Codec
  provided by Cisco Systems, Inc." and let the user disable it. Building openh264 from source
  (the crate's default `source` feature) shifts patent exposure to you — do **not** ship that.
  This also fixes machine B (no HW decoder) on Windows and makes every CI runner/VM able to
  run the full pipeline. Encoder preset: `SCREEN_CONTENT_REAL_TIME`, RC bitrate mode,
  1-slice, intra period 0 (keyframes on demand), max QP floor as today.
* Royalty-free alternative kept as a follow-up (not in the first cut): SVT-AV1 (BSD-3-Clear)
  + dav1d (BSD-2) software AV1; VP8/VP9 via libvpx (BSD-3). All permissive, no download step,
  but a second codec family to maintain.
* The `codec` crate's pure parts (`Codec`, `negotiate`, `EncodedFrame`) move to
  `platform-api` so their test runs everywhere.

## 6. Keyboard identity

Wire format stays **DOM `KeyboardEvent.code` strings** (`protocol::InputMsg`). Replace the
hand-written Set-1 table (`input/src/scancode.rs`, 104 entries, duplicated list) with **one
generated table** from Chromium's `dom_code_data.inc` (BSD-3), which maps per physical key:
USB HID usage ↔ evdev ↔ XKB ↔ Windows scancode ↔ macOS `kVK_*` ↔ DOM code. Task D3 generates
`platform-api/src/keymap.rs` with `fn from_dom(code) -> Option<KeyId>` and per-OS
`to_native(KeyId) -> u16/u32` + `from_native(...)`. Facts that make this cheap:

* Linux evdev codes for the main block **equal** Set-1 make codes (`KEY_A = 30 = 0x1E`);
  extended (E0-prefixed) keys differ and need the table. X11 keycodes = evdev + 8.
* macOS `kVK_ANSI_*` are *positional* (US layout positions) — exactly the layout-independent
  identity the protocol wants.
* The viewer side needs the inverse (native → DOM) for its captured keys; the hook path
  (`keyhook.rs`) already emits DOM codes directly.

**Modifier translation (D8).** Codes stay physical on the wire; the *viewer* rewrites
modifiers for the host's OS before sending (`engine/src/modifiers.rs`): Mac → Windows/Linux
swaps Cmd ↔ Ctrl and turns Cmd+Tab / Cmd+\` into Alt+Tab / Alt+\`; Windows/Linux → Mac swaps
Ctrl ↔ Cmd (Win/Super becomes Control) and turns Alt+Tab into Cmd+Tab; Ctrl+Tab is kept in
both directions; every other pair is identity. Releases use the code that was sent, so held
keys survive a toggle flip. Setting `translateModifiers` (default on). The host learns
nothing new and injects what arrives. **The one protocol addition:** `connect-response`
carries an optional `os` (`"windows"` | `"macos"` | `"linux"`), set by the host from
`PlatformInfo::name`. Absent (older hosts) or unknown → the viewer forwards keys literally.
The signaling server relays it opaquely, like `codec`.

## 7. Threads, runtimes, and ownership (unchanged shape)

Keep the current model: one `transport_driver` std thread per session owning `Rtc` +
`UdpSocket` (+ TURN); one media thread per direction owning the GPU device; a clipboard
poll thread; viewer input-translation thread; optional hook thread. Backends that are
thread-bound (macOS SCStream delivery queue, CGEventTap run loop, X11 connection, PipeWire
main loop) run **inside** the media/hook thread they belong to and hand frames over a
`Receiver` with a bounded depth of 1 (newest-wins), preserving the pull semantics of
`acquire(timeout)`.

## 8. Permissions model (new, shared UI)

`Platform::permissions()` returns `{screen_capture, input_injection, input_monitoring,
uinput_access, portal_available}` each `Granted | Denied | NotApplicable | Unknown`, and
`Platform::request(permission)` opens the right system dialog/deep link (macOS:
`x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture` /
`Privacy_Accessibility`; Linux Wayland: triggers the portal dialog and stores the restore
token). The UI gets one new modal (task D5) that lists them with "Grant" buttons and is
shown before the first host/viewer session when anything is `Denied`. On Windows it reports
`NotApplicable` (plus the existing UIAccess note).

## 9. Explicitly out of scope (and why)

* **Login screen / lock screen / UAC-equivalent on macOS and Linux.** macOS loginwindow
  capture needs a LaunchDaemon running as root *and* the TCC grant for a root-context
  binary; Apple does not allow screen capture of the login window by third-party apps
  without MDM. Linux greeters (GDM/SDDM) run their own compositor session; no portal exists
  there. The Windows SYSTEM service stays Windows-only. State this in the UI copy.
* **DRM-protected content** stays black on all platforms (as today on Windows).
* **Wayland on compositors without the RemoteDesktop portal** (Hyprland ≤1.3.11, Weston):
  view-only; the UI reports `input_injection: Denied` with the reason, and the uinput
  fallback can be enabled by the user (udev rule).
