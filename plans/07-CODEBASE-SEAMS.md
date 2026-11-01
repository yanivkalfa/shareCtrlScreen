# 07 — Codebase seams (what is Windows-bound, file by file)

Condensed from the 20-reader code sweep (full per-file maps with every line reference,
invariant and gotcha are in [appendix-codemap/](appendix-codemap/)). Numbers: 12 274 lines
of Rust in `native/`; ~4 900 are Windows-specific, the rest is portable.

## Verified today by compiling in WSL2 (Ubuntu 24.04, rustc 1.99)

`cargo check --workspace` on Linux, after installing Tauri's GTK prerequisites:

| Crate | Linux result | Why |
|---|---|---|
| `protocol`, `transport`, `signaling` | compiles, tests pass | pure |
| `input` | compiles | non-Windows stub `Injector` (`lib.rs:274-293`), `scancode` tests run |
| `capture`, `codec`, `render`, `elevation` | compile to **empty** crates | `#![cfg(windows)]` |
| `engine` | **2 errors** | `src/secret.rs:15-18` imports `windows::` while `pub mod secret` (`lib.rs:18`) is not gated; every other Windows use is already `#[cfg(windows)]` |
| `app` | not reached (engine failed); with `secret` stubbed it compiles (checked) | `hwnd()`, autostart, video window are all `cfg(windows)`; `%APPDATA%` falls back to temp dir |
| `service` | error: no `main` | `#![cfg(windows)]` on a bin — exclude from non-Windows builds in the workspace (`cargo check --workspace --exclude service` or `[target]`-gated default-members) |

Cross-`cargo check` **from Windows** to Linux/macOS targets does not work (aws-lc-sys, via
str0m's default `aws-lc-rs` crypto, needs a C cross-compiler); check natively instead.

## Per-crate seams

### `engine/src/pipeline.rs` (2 509 lines) — the real port surface
* **Portable as-is (≈ 70 %):** ICE candidates/STUN/TURN glue (134-441), bitrate/fps
  constants (443-463), codec negotiation helpers (539-590), `begin_host`/`begin_viewer`/
  `on_signal`/`send_ctl`/`set_control`/`teardown` control flow (594-1052), the whole
  `transport_driver` loop with liveness, keyframe gate, bitrate policy, bulk/file state
  machine (1060-1785), `fit_within` (1791), `file_send_loop` (1810), `clipboard_loop`
  (1957), the pacing/keyframe policy inside `host_media_loop` (2044-2252), the catch-up/
  resync logic inside `viewer_media_loop` (2285-2418), tests (2449-2508).
* **Windows calls to route through traits:** `create_video_window`/`RENDER_HWND` (25, 50-61),
  `render::window::{show,hide,fit,client_size,set_*_sink,clear_*_sink}` (805-818, 1009-1019,
  1038-1050, 1100-1102, 2266-2307), `input::Injector` (1111, 1282, 1616, 1783),
  `elevation::InputDesktopFollower` (1115-1117, 1610-1612), `keyhook::*` + `process_has_uiaccess`
  (916-951), `input::scancode::code_for` (988), `codec::{encode::host_encodable,decode::viewer_decodable}`
  (568, 579), `deliver_received_file` → `clipboard::set_files` + `focus_window_at` + Ctrl+V
  (1922-1955), `capture::Duplicator` (2010-2245), `codec::Encoder/Decoder` (2026-2392),
  `render::Renderer` (2279-2398), `create_render_device` D3D11 (2423-2447),
  `"Downloads\\ShareCtrlScreen"` literal (1701).
* **Known bugs to fix in the split (F2):** viewer never tears down a previous session on a
  new offer (768-961); answer/ICE arriving before `SESSION` is set are dropped (661-667 vs
  734-741); `start_viewer_transport` spawns threads even when `accept_offer` failed (786-799);
  IPv6 host candidates advertised on an IPv4 socket (134-155 vs 292); `turns:` URLs ignored
  (500); unbounded `video_tx` with no backpressure (1504).

### `engine/src/lib.rs` (640) — control plane, portable
9 `#[cfg(windows)]` call sites into `pipeline` with silent non-Windows stubs (368-374, 391,
402-405, 535-538, 551, 556-563, 574-579, 585-590, 617-622) → replaced by `Arc<dyn Platform>`
+ portable `session`. `secret::{protect,unprotect}` called ungated at 458/517.

### `engine/src/{secret,autostart,clipboard,transfer}.rs`
DPAPI (`secret.rs`), HKCU Run key (`autostart.rs`), Win32 clipboard + CF_HDROP
(`clipboard.rs`, with the load-bearing "read the sequence number only after CloseClipboard"
echo-guard rule at 112-118), `%USERPROFILE%` (`transfer.rs:23-28`). `transfer::Incoming`
is portable (tests: 7, all pass on Linux except the backslash-traversal test at 162 which
must also cover `/`).

### `capture/src/lib.rs` (374) — DXGI DDA
Contract to reproduce: `acquire(timeout) → Ok(None)` idle, `Err(AccessLost)` → `reinit`,
`Err(SecureDesktop)` on E_ACCESSDENIED, `release()` idempotent, frame valid only until
release, encoder shares the device (`device()`), cursor only on change events
(`LastMouseUpdateTime != 0`), `pointer_only` when `LastPresentTime == 0`. Public types leak
`windows` `POINT/RECT/DXGI_OUTDUPL_*`.

### `codec/src/{encode,decode,convert,variant}.rs` (1 855) — Media Foundation
Low-latency recipe to mirror (encode.rs:665-718): CBR, MeanBitRate live, MaxQP 32, 0
B-frames, GOP 600 (≈ infinite), QualityVsSpeed 66, LowLatency on, ForceKeyFrame on demand;
HW async MFT with zero-copy `MFCreateDXGISurfaceBuffer`, SW fallback via staging readback.
Decoder: D3D11VA, `MF_E_TRANSFORM_STREAM_CHANGE` adoption, display aperture crop, hard-coded
166 667 (60 fps) sample duration. Pure parts: `Codec`, `negotiate`, `EncodedFrame`,
`copy_nv12`. Header comments claim LTR recovery that the code does not implement.

### `render/src/{lib,window}.rs` (1 213) — D3D11 flip swapchain + child HWND
Presenter contract: waitable swapchain latency 1, `ALLOW_TEARING` when supported, clear +
fullscreen triangle NV12→RGB BT.709 limited range (shader at lib.rs:51-105), cursor sprite
18×26 px scaled by letterbox, SRV cache (64), no device-loss recovery, latent bug: `resize`
failure leaves `rtv = None` forever (345-367). Window contract: hidden `WS_CHILD` above
WebView2, `SetWindowRgn` hover-bar clip (`BAR_CSS_PX` 40, unreveal 56 px, not DPI-scaled),
`client_size` invariant under clip, input via wndproc (Set-1 scancode + extended bit), no
`SetCapture` (drag-out loses button-up), `clear_input_sink` clears held state **without**
releases, `DragAcceptFiles`/`WM_DROPFILES` with normalized drop point.

### `input/src/{lib,keyhook,scancode}.rs` (927)
`SendInput` injector (absolute 0..65535, VIRTUALDESK when a target rect is set, move-before-
click, sentinel `0x5343_5253` in `dwExtraInfo`, `release_all` without a move),
`focus_window_at` (`WindowFromPoint` → `GA_ROOT` → `SetForegroundWindow`, no click),
`WH_KEYBOARD_LL` hook (suppress Win always, Tab/Esc with Alt, focus-gated, synthesizes
`AltLeft` for the remote switcher, forwards owed key-ups unconditionally, 10 ms `PeekMessage`
pump, forward closure called under the `STATE` lock), DOM-code ↔ Set-1 table (104 entries,
duplicated in `ALL_CODES`, `code_for` is O(n)).

### `elevation/*`, `service/*` (526) — Windows only, stays
`WTSQueryUserToken`/`CreateProcessAsUser`/`SetThreadDesktop`/SCM. Non-Windows: `DesktopFollower`
no-op; `process_has_uiaccess` → `false`.

### `app/src/main.rs` (407) + `app/ui/*` (998)
Portable except: `windows_subsystem` attr (16), `hwnd()` → `create_video_window` (391-395),
`set_video_visible` (160-164), autostart (246-252, 313-314, 396-401), `%APPDATA%` (256-262),
NSIS-only bundle config, the unreferenced `sharectrl.manifest` (not embedded by `build.rs`).
UI: no OS branches at all; five Windows-specific copy strings (`index.html:229-288`), empty
`#set-share-display`, `set_video_visible(false)` must complete before a modal paints.

### Test inventory
54 `#[test]`s in 17 files: 38 run on any OS today, 9 are Windows-API tests (DPAPI,
clipboard, Run key), 7 are pure logic trapped behind `cfg(windows)` modules (codec
negotiate, `fit_within`, STUN parser, autostart command format). CI runs on
`windows-latest` only; `latency_smoketest` is a manual on-hardware example. No tests for
`transport::Transport`, `signaling`, the injector, the hook, the renderer, or any UI.

### Old Electron build (`app/`) — nothing to reuse beyond ideas
Windows-only too (`koffi.load('user32.dll')`); its viewer sent DOM codes and normalized
coordinates, which the native protocol kept. Its stuck-input discipline and move-before-
click rule are already in the Rust injector.
