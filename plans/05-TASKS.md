# 05 — Tasks (one shot, parallel lanes)

## Status (2026-11-01, branch `port/multi-os`)

| Task | State | Notes |
|---|---|---|
| F1 platform-api | **done** `dfe14f3` | The crate is the source of truth for every trait; 04 §2 is design rationale only. Changes vs the plan: `OsPermission` (not `Permission`, which clashes with view/control); `VideoInput::Key` carries the DOM code (`&'static str`) and backends map native keys; `set_top_clip` removed — toolbar reveal is the surface's own job (thresholds in the trait doc); GPU handles are `&dyn Any`; media objects are created on their own thread so need not be `Send`; `platform_api::unavailable` holds fail-safe impls. |
| F2 pipeline split | **done** `f1b961c` | `engine/src/session/{mod,ice,driver,files,clipboard}.rs`, `media/{host,viewer}.rs`, `tuning.rs`. No statics; the engine has no OS deps. 13 bugs fixed (see commit). |
| F3 secret/autostart/clipboard | **done** `f1b961c` | now in `platform-windows`. |
| W1 Windows backend | **done** `f1b961c` | `platform-windows` (adapters.rs, displays.rs, gpu.rs). App verified live: surface created, signaling registered. Display selection (`shareDisplayId` = `dxgi:<n>`) is now wired. |
| D1 loopback harness | **done** `7f91371` | `engine/tests/loopback.rs` — ~4 s, all OSes, security assertions mutation-checked. |
| T1 CI matrix | **done** `dc47c95` | Windows / macOS / Ubuntu 22.04 + license gate on all 4 triples. **The macOS job is unverified until the branch is pushed.** |
| platform-macos / platform-linux | **scaffold → filling in** | Real paths (+ Linux session-type detection); lanes M and L replace the stubs in place (see rows below). Crate-level `#![cfg(target_os = …)]` is the convention; pure-logic tests run on that OS's CI job. |
| D6 docs | **done** `c4c55d4` (Haiku, Opus-reviewed) | root README install/what-works-where + docs/FIRST-RUN.md |
| D5 permissions modal | **done** `1f3b607` (Sonnet) | host preflight: screenCapture + inputInjection; viewer preflight only with captureShortcuts on |
| L10 XDG autostart | **done** `136dbd9` (Haiku, Opus-reviewed) | `~/.config/autostart/sharectrl.desktop` |
| D3 keymap | **done** (Sonnet) | `platform_api::keymap` generated from Chromium `dom_code_data.inc` (`tools/gen-keymap`); NumLock kept at the plain `0x45` override until W3 (now removed); Power/edit keys excluded |
| M7 clipboard | **done** `8d2dea6` (Sonnet) | NSPasteboard text + change count + file URLs; `set_files` refuses relative paths |
| M9 autostart | **done** `2a7aa03` (Haiku, Opus-reviewed) | per-user LaunchAgent, no launchctl |
| L12 permissions | **done** `2f5c0b4` (Sonnet) | portal preflight + consent flow, restore-token store (`portal_token.rs`) |
| M11 permissions | **done** `969e6aa` (Sonnet) | TCC preflight/request: Screen Recording, Accessibility, Input Monitoring, Secure Input note; Settings deep links. **Device-pending** (checklists/M11.md) |
| M8 Keychain store | **done** `410663c` + M8b `forget` (Haiku, Opus-reviewed) | keyring 4.2 / apple-native store; legacy SecKeychain ACL by design (no Team ID). **Device-pending** (checklists/M8.md) |
| M10 bundle + signing | **done** (Sonnet) | `.app`/DMG universal build, self-signed cert scripts (`scripts/macos-*.sh`), `mac-build.yml`, first-run docs. **Owner TODO:** run `macos-make-signing-cert.sh`, add the two secrets, supply a 1024×1024 icon. |
| T5 latency smoketest | **done** `b657f43` (Sonnet) | `tools/smoketest` on the traits (`--mock`, `--software`, `--display`, `--size`); real-hardware verdict still needs an idle machine |
| D4 platform info UI | **done** `fd7a128` (Sonnet) | `get_platform_info`, OS-neutral settings copy, monitor list |
| M1 ScreenCaptureKit capture | **done** `2a53a47` (Opus) | SCStream → newest-wins slot, cursor out-of-band, stable display ids. **Device-pending** (checklists/M1.md) |
| L8 clipboard | **done** `01d438b` (Sonnet) | arboard text; X11/Wayland file-list selection owners; content-derived change count. Real GNOME/KDE paste unverified |
| engine secret-store hardening | **done** `62e14ca` (orchestrator) | `SecretStore::forget`; seal/open off the signaling loop (spawn_blocking, 60 s bound on open); MSRV 1.88 |
| L5-spike | **done** `de9f354` (Opus) | Wayland subsurface works on sway/Weston; GNOME/KDE device pass pending; L5 builds option 1 (see plans/spikes) |
| D2 OpenH264 | **done** `0dd75a1` (Opus) | Cisco binary downloaded at runtime (SHA-256 pinned), licence toggle + notice in Settings → Advanced, `WithSoftwareH264` adapter on all three platforms, D3D11 readback + CPU-NV12 upload on Windows. 1080p encode budget to be measured on an idle machine |
| M4 NSView surface | **done** `76eccb2` (Opus) | `surface-wgpu` shared crate (WGSL NV12→RGB, letterbox, cursor, reveal, held-input release) + NSView/CAMetalLayer view, bounded main-thread hop, zero-copy CVPixelBuffer import. Uses `gpu::CvPixelBufferFrame` and the generated keymap. **Device-pending** (checklists/M4.md) |
| L9 Secret Service store | **done** `bc7f6d0` (Haiku, Opus-reviewed) | keyring-core + zbus store, fresh connection per call (no poisoned cache), `forget` implemented; daemon round-trip test. The dev-dep feature-unification build defect was caught by the gate and fixed |
| M8b Keychain `forget` | **done** `63e5466` (Haiku) | follow-up to M8 |
| L3 NVENC + VA-API encode | **done** `8baf62c` (Opus, independent FFI review: layouts verified with gcc; 9 findings fixed) | dlopen bindings, CBR/max-QP-32/one-frame VBV recipe, levels from size+fps, driver-header capture. NVENC verified on the GTX 1080 Ti; **VA-API device-pending** (checklists/L3.md) |
| L11 Linux packaging | **done** `8e15a11` (Sonnet) | deb/rpm/AppImage targets + deps, desktop template, hicolor icons, `uinput_access` (pkexec installer), `linux-build.yml`. deb built and inspected in WSL; AppImage/VM installs CI/device-pending (checklists/L11.md). **Owner TODO:** 1024×1024 icon |
| advisories | **clean** `e92735f` | tokio → 1.54.1; `cargo deny check` passes on all four triples |
| M2, M3, L1, L2, L4, M6, L7 | queued (Opus cap) | start in wave 2, warm caches |
| W2, W3, M5, L6, L5, D7, T2, T3, T4 | blocked on dependencies | W3/M5/L6 unblocked by D3; W2 by D2; D7 by M10 (+L11); L5 by the spike |
| everything else | open | as below. |

Interfaces the remaining tasks must use: `Platform::create_capture / create_surface / create_injector / create_shortcut_hook / create_clipboard` (call on the thread that uses the object), `Engine::start(platform, decider)`, `Engine::attach_surface`, `IcePolicy::LoopbackOnly` for tests, mocks in `platform_api::mock`.

---

Everything ships together; the tasks are cut so that **up to ~12 agents/people work at the
same time** after a short foundation phase. Each task lists: lane, depends-on, machine
needed, size (S ≤ 1 day, M 2–3 days, L ~1 week, XL 2 weeks for one person), deliverable,
acceptance test. Line references point at today's code; the per-file maps in
[appendix-codemap/](appendix-codemap/) have the details an implementer needs.

Lanes: **F** foundation · **W** Windows re-plumb · **M** macOS · **L** Linux · **D** cross-cutting ·
**T** tests/CI · **I** integration.

```
F1 ──┬── W1, W2, W3 (Windows machine)
     ├── M1..M11 (macOS runner for compile; a real Mac for M1,M4,M5,M6,M10)
     ├── L1..L12 (WSL2/VM for X11+software paths; real GPU Linux for L3/L4)
     ├── D1..D7  (any OS)
     └── T1..T5  (CI)
I1..I4 after all lanes (hardware validation)
```

---

## Lane F — foundation (blocks everything; do first, 2–3 days, 1–2 people)

### F1 — `platform-api` crate: traits + neutral types + keymap stub  (size M, any OS)
* Depends on: nothing. Output: `native/crates/platform-api` exactly as in
  [04 §2](04-TARGET-ARCHITECTURE.md#2-the-platform-api-crate-the-contract-every-lane-codes-against),
  plus `Codec`/`negotiate`/`EncodedFrame`/`EncoderConfig` moved here from `codec/src/lib.rs:26-75`
  (their test moves too and now runs on every OS), `VideoInput`/`FileDrop` moved from
  `render/src/window.rs:33-63`, `CursorUpdate` etc. with no `windows` types.
* Also: a `mock` module (feature `mock`): `MockCapture` (synthetic moving-gradient frames +
  cursor + dirty rects, deterministic), `MockSurface` (records presented frames into a
  `Vec`, emits scripted `VideoInput`), `MockInjector` (records `InputMsg`s), `MockClipboard`,
  `MockSecretStore`, `MockAutostart`. These are what T1 and every lane's unit tests use.
* Acceptance: `cargo test -p platform-api` green on Windows **and** in WSL; `cargo doc`
  explains every invariant listed in 04 §2 on the trait item it belongs to.
* Note: freeze the trait signatures in a PR reviewed by the three platform leads before
  lanes start; later additions are allowed, renames are not.

### F2 — Split `engine::pipeline` into portable `session` + `media` modules  (size L, Windows machine)
* Depends on: F1. The 2 509-line `pipeline.rs` becomes:
  * `engine/src/session/ice.rs` (lines 134-441: candidates, STUN, TURN glue — already portable),
  * `engine/src/session/driver.rs` (1060-1785 `transport_driver` — portable once
    `render::window::{show,hide,clear_*}`, `input::Injector`, `elevation::InputDesktopFollower`,
    `deliver_received_file` go through the traits; the Windows `InputDesktopFollower` becomes a
    no-op `DesktopFollower` trait on other OSes),
  * `engine/src/session/files.rs` (1810-1955), `clipboard.rs` loop (1957-1996),
  * `engine/src/media/host.rs` (2000-2257) and `media/viewer.rs` (2259-2421) calling only
    `ScreenCapture`/`VideoEncoder`/`VideoDecoder`/`Presenter`,
  * `engine/src/session/mod.rs`: `begin_host`, `begin_viewer`, `on_signal`, `teardown`,
    `set_control`, `send_ctl`, the statics (`SESSION`, `VIEW_SIZE`, `NEGOTIATED_CODEC`…).
  `Engine` gets an `Arc<dyn Platform>` field; `lib.rs` loses every `#[cfg(windows)]` (lines
  11-17, 368-374, 391, 402-405, 535-538, 551, 556-563, 574-579, 585-590, 617-622). The
  non-Windows "silent accept with no media" path disappears.
* Fix while there (cheap, documented in the sweep): viewer path never calls `teardown`
  before a new offer (pipeline.rs:768-961); `set_input_sink(None)` must emit releases
  (`window.rs:308`); `Downloads\ShareCtrlScreen` literal (pipeline.rs:1701) → `Paths`.
* Acceptance: Windows build behaves identically (run the two-machine test in
  [native-test-setup]); `cargo check -p engine` passes in WSL with `--features mock`;
  `engine` unit tests (handshake, turn, transfer, fit_within, STUN parser) run in WSL.

### F3 — `secret`, `autostart`, `clipboard`, paths behind traits  (size S, Windows)
* Depends on: F1. Move `engine/src/{secret,autostart,clipboard}.rs` into
  `crates/platform-windows` as `impl SecretStore/Autostart/Clipboard`; `app/src/main.rs:256-262`
  `config_path()` → `Platform::paths()`. Settings key `startWithWindows` stays on the wire
  (UI compat) but the label becomes "Start at sign-in".
* Acceptance: this is the change that makes `engine` compile on Linux today (the only two
  errors in WSL are `secret.rs:15-18`).

---

## Lane W — Windows backend re-plumb (keeps today's behaviour)

### W1 — `platform-windows` crate: capture/codec/render/input/elevation behind the traits  (size L, Windows)
* Depends on: F1 (can start in parallel with F2 against the trait crate).
* Wrap, don't rewrite: `capture::Duplicator` → `impl ScreenCapture` (convert `POINT/RECT`
  to neutral types, DDA monochrome/masked cursor shapes → BGRA); `codec::{Encoder,Decoder}`
  → `impl VideoEncoder/VideoDecoder` + `impl CodecFactory` (uses `host_encodable`/
  `viewer_decodable`); `render::{VideoWindow,Renderer}` → `impl VideoSurface/Presenter`;
  `input::Injector` + `keyhook` → `impl InputInjector/ShortcutHook`; `elevation::process_has_uiaccess`
  → `PermissionReport`.
* Known gaps to close in the wrapper (from the sweep): encoder staging-map leak on error
  (`encode.rs:378-397`), `enumerate_*` COM array element leaks (`encode.rs:571-574`,
  `decode.rs:612-613`), decoder SW-path chroma offset before first stream change
  (`decode.rs:147,266-268`), no software decode fallback when HW activation fails
  (`decode.rs:618`). None of these change behaviour on the happy path.
* Acceptance: W1 + F2 together = identical Windows product; `latency_smoketest` still
  prints the same numbers on machine A.

### W2 — Software codec backend selection on Windows  (size S, Windows)
* Depends on: D2. Wire the OpenH264 fallback into `CodecFactory` for the no-HW-decoder
  case (machine B). Acceptance: machine B decodes H.264 from machine A at ≥ 30 fps 1080p.

### W3 — Keymap switch on Windows  (size S, Windows)
* Depends on: D3. Replace `input/src/scancode.rs` lookups with the generated table (Set-1
  column). Acceptance: existing scancode tests pass against the generated table; the
  104 old entries map identically (write a test that asserts it).
* **Done (W3).** `input::scancode` is now a thin adapter over `KeyId::{to,from}_set1`; the
  hand table is gone (kept only as a test fixture proving the 104 old entries). Callers
  (`Injector`, the viewer window's `code_for`) are unchanged in shape. `MapVirtualKeyW` is not
  used (layout-dependent). Decision, **NumLock parity override removed**: the old table sent
  NumLock as plain `0x45`, but Windows reads plain `45` as Pause (`E1 1D 45`, make code `45`
  not extended) and NumLock as `E0 45` (Chromium `0xe045`, AutoHotkey `SC145`). So the old
  injector pressed Pause and the viewer forwarded a real Pause as NumLock. The generated
  `0xe045` is used as is; `SET1_OVERRIDES` is deleted from `tools/gen-keymap`. This is the one
  intentional difference from the 104 old entries (asserted in `scancode.rs`). Device smoke
  to do: NumLock toggles the LED on the host and Pause does nothing.

---

## Lane M — macOS backend (`crates/platform-macos`)

All compile on GitHub `macos-latest`; **M1, M4, M5, M6, M10, M11 need an interactive Mac**
to run (TCC). Use `objc2` 0.6-family crates throughout (`objc2-app-kit`,
`objc2-core-graphics`, `objc2-core-video`, `objc2-core-media`, `objc2-video-toolbox`,
`objc2-screen-capture-kit`, `objc2-metal`, `objc2-quartz-core`).

### M1 — ScreenCaptureKit capture  (size L)
* `impl ScreenCapture`: `SCShareableContent` → `SCContentFilter(display)` →
  `SCStreamConfiguration` per [01 §3](01-MACOS-FINDINGS.md#3-capture--screencapturekit);
  delegate on a dispatch queue pushes `CVPixelBuffer`s into a depth-1 newest-wins slot;
  `acquire(timeout)` waits on it; `release()` drops the buffer (returns it to SCK's pool);
  `reinit()` recreates the stream; `dimensions()` = configured capture size (= display
  pixels unless `VIEW_SIZE` requests smaller). Dirty rects from `SCStreamFrameInfoDirtyRects`,
  `pointer_only` when `SCStreamFrameInfoStatus == .idle` and the cursor moved. Cursor from
  `NSEvent.mouseLocation` + `NSCursor.currentSystem` on the same thread.
  `PermissionReport.screen_capture` from `CGPreflightScreenCaptureAccess()`; request via
  `CGRequestScreenCaptureAccess()`.
* Acceptance (real Mac): 60 fps capture of a 4K display with < 1 % CPU on the capture
  thread; `acquire` returns `Ok(None)` on a static screen; revoking the permission mid-run
  yields `PermissionDenied` and a toast; display sleep/wake recovers via `reinit`.

### M2 — VideoToolbox encoder  (size M)
* `impl VideoEncoder` per [01 §4](01-MACOS-FINDINGS.md#4-encodedecode--videotoolbox):
  accepts `GpuFrame::CvPixelBuffer` zero-copy and `GpuFrame::Cpu(Bgra8)` (via a
  `CVPixelBufferPool`); AVCC → Annex-B with SPS/PPS prepended on keyframes; live
  `set_bitrate` (`AverageBitRate` + `DataRateLimits`); `force_keyframe` via frame options;
  fallback session without `EnableLowLatencyRateControl` on `-12902`.
* Acceptance (**runs on GitHub macOS runners, no TCC needed**): encode 300 synthetic
  1080p frames; first unit is an IDR; mean encode ≤ 8 ms on M-series runner; bitrate
  within ±15 % of target over 5 s; `cargo test -p platform-macos --test vt_encode`.

### M3 — VideoToolbox decoder  (size M)
* `impl VideoDecoder`: Annex-B → AVCC, `CMVideoFormatDescriptionCreateFromH264ParameterSets`
  (HEVC/AV1 variants), `VTDecompressionSession` outputting `420v` Metal-compatible
  `CVPixelBuffer`; `dimensions()` tracks format changes; `viewer_decodable()` probes
  `VTIsHardwareDecodeSupported(kCMVideoCodecType_{H264,HEVC,AV1})`.
* Acceptance (GitHub runner): round-trip with M2 (encode→decode→PSNR ≥ 40 dB on a
  gradient); decoding a stream that switches 1080p→720p mid-way reports new dimensions.

### M4 — NSView video surface + wgpu/Metal presenter  (size L, real Mac for UX)
* `impl VideoSurface` as in [04 §4](04-TARGET-ARCHITECTURE.md#4-the-video-surface-under-the-tauri-window-the-hardest-ui-piece):
  `NSView` subclass with `CAMetalLayer`, inserted above `WKWebView`; mouse/key events →
  `VideoInput` (keys via `NSEvent.keyCode` → `KeyId` from D3's table; `flagsChanged` for
  modifiers); `NSDraggingDestination` for file drops (`NSFilenamesPboardType`/file URLs →
  `FileDrop` with normalized point); `set_top_clip` shrinks the frame from the top.
  `impl Presenter` with wgpu Metal backend: import CVPixelBuffer planes via
  `CVMetalTextureCache` + `create_texture_from_hal`; WGSL NV12→RGB + letterbox + cursor
  triangle (port of `render/src/lib.rs:51-105`); present with `presentsWithTransaction =
  false`, `displaySyncEnabled = false` (uncapped like `ALLOW_TEARING`), `maximumDrawableCount = 2`.
  Also `impl Presenter for Cpu frames` (software decode path).
* Acceptance (GitHub runner for the presenter: offscreen `MTLTexture` target instead of a
  layer, compare pixels; real Mac for the view): hover bar reveals at y ≤ 2 px; Settings
  modal opens over the video after `set_video_visible(false)`; Retina sizes correct.

### M5 — CGEvent injector + keymap  (size M, real Mac)
* `impl InputInjector`: `CGEventPost(kCGHIDEventTap)`; absolute moves in points across
  `CGGetActiveDisplayList` bounds; buttons; `CGEventCreateScrollWheelEvent(pixel units)`
  from the 120-unit wheel (÷ 120 × 3 lines like Windows); keys via `KeyId → kVK`; modifier
  flags maintained from held keys (`CGEventSetFlags`); `release_all`; sentinel in
  `kCGEventSourceUserData`; `focus_window_at` via `CGWindowListCopyWindowInfo` +
  `NSRunningApplication.activate`. `PermissionReport.input_injection` from
  `AXIsProcessTrustedWithOptions(prompt=false)`; `request()` with `prompt=true`.
* Acceptance (real Mac): a scripted `InputMsg` sequence types "hello" into TextEdit and
  Cmd+V pastes; releases on `release_all`; works on a two-display setup.

### M6 — CGEventTap shortcut hook  (size M, real Mac)
* `impl ShortcutHook`: session tap on the hook thread with its own `CFRunLoop`; swallow
  Cmd+Tab, Cmd+\`, Cmd+Space, Cmd+Q?, Ctrl+↑/↓/←/→ (Mission Control) while our window is
  key (`NSApp.keyWindow`); forward DOM codes; re-enable on `kCGEventTapDisabledByTimeout`;
  ignore events carrying our sentinel; detect `IsSecureEventInputEnabled()` and report.
* Acceptance (real Mac): Cmd+Tab while focused switches apps on the Windows host, not
  locally; clicking another window restores local Cmd+Tab instantly.

### M7 — NSPasteboard clipboard  (size S)
* `impl Clipboard`: `changeCount`, string get/set, `set_files` via `writeObjects([NSURL])`.
  Acceptance (runner): round-trip + `change_count` advances exactly once per write; after
  `set_files`, `get_text` is `None` (matches `clipboard.rs:204` contract).

### M8 — Keychain secret store  (size S) — `keyring` 4.2. Acceptance: round-trip; garbage → `None`.
### M9 — LaunchAgent autostart  (size S) — `auto-launch` 0.6 + `--autostart` minimise path in `app`. Acceptance: plist written/removed; `reconcile` rewrites a stale path.
### M10 — Bundle, signing, Gatekeeper, first-run  (size M, real Mac to verify once)
* `tauri.conf.json`: `bundle.targets += ["app","dmg"]`, `icons/icon.icns` (generate from
  `icon.png`), `macOS.minimumSystemVersion = "13.0"`, `signingIdentity` from env with `"-"`
  default, `entitlements` none, `Info.plist` additions `NSScreenCaptureUsageDescription`,
  `NSAppleEventsUsageDescription` (not strictly needed; harmless), `LSUIElement` no.
* Self-signed cert: script `native/scripts/macos-make-signing-cert.sh` (openssl, 10-year
  validity, codeSigning EKU) → `.p12` → repo secrets `MAC_SIGN_P12_B64`, `MAC_SIGN_P12_PASS`;
  CI imports into a temp keychain and codesigns; verify `codesign -dr - ShareCtrlScreen.app`
  shows `certificate leaf =` (not `cdhash`).
* First-run doc page + README section: "Open Anyway" steps with screenshots, `xattr`
  alternative, what the monthly Sequoia prompt is.
* Acceptance: a DMG from CI installs on a fresh Mac via "Open Anyway"; after replacing the
  app with the next CI build, Screen Recording/Accessibility stay granted.

### M11 — Permissions preflight on macOS  (size S, real Mac) — implements `Platform::permissions/request` (see D5 for the UI). Acceptance: fresh user account → preflight modal lists the two grants → both deep links open the right pane.

---

## Lane L — Linux backend (`crates/platform-linux`, feature flags `x11`, `wayland`, `vaapi`, `nvenc`, `uinput`)

### L1 — Wayland capture: portal ScreenCast + PipeWire  (size L; WSL2 can compile, a GNOME VM runs it)
* `ashpd` session (`persist_mode = ExplicitlyRevoked`, restore token at
  `$XDG_STATE_HOME/sharectrl/portal.token`), `pipewire` stream on its own thread with a
  `MainLoop`; buffer types MemFd/MemPtr (v1) + DmaBuf negotiation behind feature `dmabuf`
  (stretch L1b); cursor metadata; damage metadata if present; stream `closed` → `Lost`.
  Combined ScreenCast+RemoteDesktop session when L6 is active (one dialog, one token).
* Acceptance (CI with headless `sway` + `xdg-desktop-portal-wlr` + `pipewire`, see T4):
  capture 100 frames of a test pattern window; restore token round-trip skips the dialog on
  the second run (`-wlr` supports it); GNOME VM manual check.

### L2 — X11 capture: XShm + XDamage  (size M; runs in WSL under Xvfb and in CI)
* `x11rb` with `shm`,`damage`,`xfixes`,`randr`; dirty rects from Damage; `GpuFrame::Cpu(Bgra8)`;
  cursor via XFixes; refuses to run under rootless Xwayland when `WAYLAND_DISPLAY` is set.
* Acceptance (CI, `Xvfb :99 -screen 0 1920x1080x24`): a moving `xterm` produces dirty
  rects; static screen → `Ok(None)`; `xrandr` size change → `Lost` → `reinit` new dimensions.

### L3 — VA-API encoder (+ NVENC optional)  (size L; needs a real Intel/AMD GPU box — machine B booted from a Linux USB, or the user's Linux PC; NVENC testable in WSL2 per 2026 reports)
* `impl VideoEncoder` on `cros-libva` (evaluate `cros-codecs` 0.0.6 first; if its encoder
  fits, use it): H.264 Constrained Baseline/Main, `VAEntrypointEncSliceLP` preferred, CBR
  (`VAConfigAttribRateControl`), `VAEncMiscParameterRateControl` for live bitrate,
  `VAEncMiscParameterHRD`, max QP, intra period 0 + forced IDR, 1 slice; VPP BGRA→NV12;
  input: CPU upload (v1) and DMA-BUF import (`DRM_PRIME_2`, feature `dmabuf`, L3b).
  NVENC (feature `nvenc`): `dlopen("libnvidia-encode.so.1")` binding over the MIT
  `nv-codec-headers` (encode H.264/HEVC, `NV_ENC_PARAMS_RC_CBR`, `enableAQ`, low-latency
  preset P1–P3, tuning `ULTRA_LOW_LATENCY`), CUDA host-memory input v1.
  `CodecFactory::host_encodable` from `vaQueryConfigEntrypoints`.
* Acceptance: round-trip with L4 on real hardware; `vainfo`-less machine → factory reports
  only the software codec; CI compiles with `--features vaapi` on `ubuntu-22.04` (`libva-dev`).

### L4 — VA-API decoder (+ NVDEC optional)  (size M; same hardware as L3)
* `cros-codecs` VAAPI H.264/HEVC/AV1 decoders → `GpuFrame::Cpu(Nv12)` via `vaDeriveImage`
  (v1) / DMA-BUF export (L4b stretch, pairs with L5b). NVDEC via `libnvcuvid` (feature).
* Acceptance: decodes the M2/W1/D2 encoder outputs (golden `.h264` files committed from
  each encoder); 1080p60 ≤ 4 ms/frame on Intel iGPU.

### L5-spike — Wayland subsurface under WebKitGTK  (size M, **time-boxed to 3 days**, GNOME + KDE + Sway VMs)
* Prove: `wl_subsurface` of `gdk_wayland_window_get_wl_surface(gtk_window().window())`
  rendered by wgpu, placed above, receiving pointer events, keyboard intercepted on the GTK
  window, hover-bar clip, fractional scale. Deliver a `native/spikes/wayland-subsurface`
  example + a one-page result. If it fails on any of GNOME/KDE/Sway → fallback (2) separate
  video window on Wayland only; record the decision in 04 §4.
### L5 — Video surface (X11 child + Wayland subsurface) + wgpu presenter  (size L)
* `impl VideoSurface` for both; `impl Presenter` with wgpu (Vulkan; GL backend when Vulkan
  is unavailable, e.g. VMs/WSLg); CPU NV12 upload path v1; the WGSL shader shared with M4
  (put it in `platform-api/shaders/nv12.wgsl`). `XShape` clip on X11; input region on Wayland.
* Acceptance: CI X11 (Xvfb + software GL via `LIBGL_ALWAYS_SOFTWARE=1`): presents 100
  `Cpu(Nv12)` frames, `client_size` stable under clip, scripted X events arrive as
  `VideoInput`; Wayland: manual on the three compositors.

### L6 — Input injection: XTest / portal+libei / wlroots virtual input / uinput  (size L)
* `impl InputInjector` with runtime backend selection and `PermissionReport` for each
  (`portal_available`, `uinput_access`). Keys via `KeyId → evdev` (X11: +8). Wheel 120 units
  → buttons 4/5 (X11) / `NotifyPointerAxisDiscrete` (libei) / `REL_WHEEL` (uinput).
  `focus_window_at`: X11 `_NET_ACTIVE_WINDOW`; Wayland → `false`.
* Acceptance (CI): Xvfb + XTest types into `xterm` (verify via `xdotool getwindowname`/
  `xev` log); uinput path with `sudo modprobe uinput` on the runner types into the Xvfb
  session through `evdev`→X; libei path in the headless sway/GNOME job (`xdg-desktop-portal-gnome`
  in the mutter job) moves the pointer (verify with `wayland-info`/a test client).

### L7 — Shortcut capture: XGrabKeyboard / keyboard-shortcuts-inhibit  (size S) — report `install == false` when refused. Acceptance: Xvfb test shows Super/Alt+Tab delivered to our window while grabbed.
### L8 — Clipboard: arboard + file-list MIME targets + content-hash change counter  (size M) — Acceptance: echo-guard loop test (two mock peers) never ping-pongs; `set_files` is readable by `wl-paste --list-types` / `xclip -t TARGETS`.
### L9 — Secret Service store  (size S) — `keyring` with `sync-secret-service`; CI with `gnome-keyring` under `dbus-run-session`. No daemon → `None`.
### L10 — XDG autostart  (size S) — `auto-launch`. Acceptance: `.desktop` has `Exec="<abs path>" --autostart`; `reconcile` rewrites.
### L11 — Packaging: deb/AppImage/rpm + runtime deps + Linux docs  (size M) — `bundle.linux.deb.depends` list from [02 §7](02-LINUX-FINDINGS.md#7-packaging-and-runtime-dependencies); AppImage built on `ubuntu-22.04`; `vainfo` troubleshooting section; udev rule installer for uinput (`pkexec`). Acceptance: AppImage runs on Ubuntu 22.04, 24.04, Fedora 40 live ISOs (VMs).
### L12 — Permissions preflight on Linux  (size S) — portal availability (`org.freedesktop.portal.Desktop` version ≥ 4 for restore tokens, RemoteDesktop interface present), `/dev/uinput` access, VA-API presence; `request()` runs the portal flow. Pairs with D5.

---

## Lane D — cross-cutting

### D1 — Loopback integration harness  (size M, any OS; the single most valuable test)
* `engine/tests/loopback.rs`: two `Engine`s in one process with a fake in-memory signaling
  server (implements the JSON contract in `protocol::signaling`), `MockCapture` →
  **real** `transport` (str0m over `127.0.0.1` UDP, both ICE host candidates) → `MockSurface`;
  codec = OpenH264 software (D2) or a `MockCodec` (RLE, feature `mock`) so the test has no
  native deps. Assertions: first delivered frame is a keyframe; ≥ 60 frames in 3 s; a scripted
  `VideoInput::Key` on the viewer arrives as `InputMsg::KeyDown{code:"KeyA"}` in
  `MockInjector`; control revocation emits releases; file push lands in a temp
  `downloads_dir` with the right bytes; clipboard text syncs once (no echo); `Bye` ends both.
* Acceptance: green on Windows, macOS and Linux CI runners with no GPU and no permissions.

### D2 — Software H.264 codec backend (OpenH264, Cisco binary at runtime)  (size M, any OS)
* `crates/codec-openh264`: `openh264` 0.9.9 crate with `libloading` feature; downloader
  for `https://ciscobinary.openh264.org/` (per-OS/arch file, SHA-256 pinned per version,
  stored in `Paths::config_dir/codecs/`), required notice string, Settings toggle "Use
  Cisco OpenH264 (downloaded separately)"; `impl VideoEncoder` (usage `SCREEN_CONTENT_REAL_TIME`,
  `RC_BITRATE_MODE`, `iMaxQp` floor, intra period 0 + `ForceIntraFrame`, 1 slice, `bEnableFrameSkip=false`)
  and `impl VideoDecoder` → `GpuFrame::Cpu(I420→Nv12)`; `impl CodecFactory` adapter that
  **appends** H.264-software to any hardware factory as the guaranteed codec.
* Acceptance: unit round-trip on all three CI OSes; encode 1080p30 ≤ 12 ms/frame on a 4-core
  runner; the binary is **not** in the repo or the installer (license condition); offline
  first run shows a clear error and the Settings toggle.

### D3 — Generated keymap from Chromium `dom_code_data.inc`  (size S, any OS)
* `native/tools/gen-keymap` (Rust or Python, run manually, output committed):
  `platform-api/src/keymap.rs` with `const TABLE: &[(KeyId /*usb*/, &str /*dom*/, u16 /*win set1 incl. E0 bit*/, u16 /*evdev*/, u16 /*mac kVK*/)]`
  + lookups. Keep today's deliberate exclusions (Pause/media/Fn → `None`, `scancode.rs:14-15`).
* Acceptance: property test "every DOM code in the old `ALL_CODES` list resolves to the same
  Set-1 value as before"; `KeyA`→`kVK_ANSI_A (0x00)`, `KeyA`→evdev 30; `NumpadEnter`
  (E0 1C) → evdev `KEY_KPENTER` 96 → mac 0x4C.

### D4 — Paths + settings copy + display enumeration  (size S, any OS)
* `Platform::paths()` via `dirs` 6; `app/src/main.rs:256-292` and `transfer.rs:23-28` use it;
  UI copy becomes platform-neutral or pulled from a new `get_platform_info` command
  (`name`, `downloads_dir`, `shortcut_capture_hint`, `start_at_login_label`,
  `secure_desktop_supported`); `#set-share-display` populated from `Platform::displays()`
  (fixes the empty select noted in the UI map).
* Acceptance: strings "Windows", "Win key", `Downloads\ShareCtrlScreen`, "Windows account"
  no longer hard-coded in `app/ui/index.html:229-288`.

### D5 — Permissions preflight modal  (size S, any OS) — new Tauri command `get_permissions` / `request_permission(name)` + UI modal shown before the first session when anything is `Denied`; Windows shows the UIAccess note only. Acceptance: mock platform with scripted `Denied` → modal appears, "Grant" calls `request`, re-poll every 2 s until `Granted`.

### D6 — Per-OS install/usage docs + first-run page  (size S) — README sections per OS; macOS Gatekeeper walkthrough; Linux `vainfo`/udev/portal troubleshooting; "what cannot work" (login screen, Wayland focus).

### D7 — Release pipeline matrix  (size M) — `release.yml` matrix `{windows-latest, macos-latest (arm64 + x86_64 → universal via `lipo` or two DMGs), ubuntu-22.04}`; assets: NSIS, DMG, AppImage, deb, rpm; `cargo deny check` with the three targets; `cargo about generate` committed as `THIRD-PARTY-LICENSES.html` per OS; version read via `cargo metadata` (not `grep`). Depends on M10, L11.

---

## Lane T — tests & CI

### T1 — CI matrix  (size S) — `native-ci.yml` → `{windows-latest, macos-latest, ubuntu-22.04}`: fmt, clippy `-D warnings`, `cargo test --workspace --exclude service` (Linux/macOS) with mock features; `deny.toml [graph].targets` += `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`; `about.toml` same + allow-list parity with deny (`MIT-0`, `CDLA-Permissive-2.0`).
### T2 — Hardware-codec CI on macOS runners  (size S) — M2/M3 tests run on `macos-latest` (VideoToolbox needs no permission). Gate on `VTIsHardwareDecodeSupported` and skip (not fail) when absent.
### T3 — X11 CI job  (size S) — `Xvfb` + `LIBGL_ALWAYS_SOFTWARE=1` + `xterm` + `xdotool`: runs L2, L5 (X11), L6 (XTest + uinput with sudo), L7.
### T4 — Wayland CI job  (size M) — two jobs: (a) `sway --unsupported-gpu` with `WLR_BACKENDS=headless WLR_RENDERER=pixman` + `pipewire` + `wireplumber` + `xdg-desktop-portal` + `xdg-desktop-portal-wlr` under `dbus-run-session`, `portals.conf` set, `XDG_CURRENT_DESKTOP=sway` exported to the bus; runs L1 and the L5 Wayland surface in a nested-window mode; (b) `mutter --headless --virtual-monitor 1920x1080` + `xdg-desktop-portal-gnome` for L6's libei path (copy the DeskShare `linux-wayland-e2e` recipe).
### T5 — Per-OS `latency_smoketest`  (size S) — generalise `engine/examples/latency_smoketest.rs` to the traits; print the §2 budget table on every OS; add `--software` flag.

---

## Lane I — integration on real hardware (after all lanes merge)

| ID | What | Where |
|---|---|---|
| I1 | Windows A ↔ Windows B (regression) | your two machines |
| I2 | macOS host ↔ Windows viewer and reverse; Cmd+Tab, Retina, 2 displays, monthly-prompt simulation (`tccutil reset`) | rented Mac + machine A |
| I3 | Linux X11 host (Ubuntu 22.04 VM, software codec) ↔ Windows viewer; Linux Wayland GNOME 46 host (VM) with portal token across restarts; KDE 6 (token bug behaviour); Sway (view-only path) | VMs on machine A (VirtualBox/VMware Workstation, free for personal use) |
| I4 | Hardware encode on Linux: machine B from a live USB (Intel/AMD VA-API) or your NVIDIA box in WSL2 (NVENC) | machine B / machine A |
| I5 | Cross-codec matrix: every encoder output decodes on every decoder (golden files from T2/L3/W1/D2) | CI (golden files) |

## Parallel schedule (rough, 1 person per task unless noted)

```
week 1   F1 ─ F3 ─ D3 ─ D2(start) ─ D1(harness skeleton) ─ L5-spike ─ M10 cert/bundle ─ T1
week 2-3 F2+W1 (2 people) │ M1 M2 M3 M4 M5 M6 M7-9 (3-4 people) │ L1 L2 L3 L4 L5 L6 L8 (3-4 people) │ D1 D4 D5 T3 T4
week 4   W2 W3 M11 L7 L9 L10 L11 L12 D6 D7 T2 T5
week 5-6 I1-I5, bug-fix loop
```
Critical path: F1 → F2 → (W1 | M4 | L5) → I-tests. Everything else floats.
