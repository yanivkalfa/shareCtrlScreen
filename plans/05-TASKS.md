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
| T3 X11 CI job | **done** (Haiku, Opus-reviewed; job unverified until pushed) | `x11-tests` job on ubuntu-22.04 runs the L2, L5 (X11), L8 clipboard and L7 shortcut tests with `SCT_X11_TESTS_REQUIRED=1`, so a skip fails; with L6's XTest and uinput read-back steps, `SCT_LINUX_INPUT_TESTS_REQUIRED=1` too. **Unverified:** the Actions run itself (branch not pushed). The WSL runs of the same commands are a partial check only. See the T3 entry. |
| T2 hardware-codec CI on macOS | **done** (Opus, Opus-reviewed; macOS job unverified until pushed) | The macOS job runs the VideoToolbox hardware tests with `--ignored --nocapture`. A skip is a printed line plus a `::warning` annotation, never a pass; strict mode (`SCT_MAC_HW_TESTS_REQUIRED`) is an opt-in repository variable, unset until a run shows the hardware on hosted runners. **Unverified:** no GitHub run yet (branch not pushed) and no Mac run. See the T2 entry. |
| I5 cross-codec matrix | **done** `8ea0065` (Opus, Opus-reviewed; MF/VT goldens device-pending per checklists/I5.md) | `crates/codec-goldens` (dev-only): one golden loader + table-driven decoder row + encoder-shape check for every crate. Media Foundation, OpenH264, NVDEC rows bit-exact on machine A; **fixed an OpenH264 decoder bug** (no picture from Main/High hosts). `mf-*` goldens **device-pending**: machine A's NVIDIA MFTs fail to open (`E_UNEXPECTED`, known quirk), so its MF shape check ran on the software encoder. VideoToolbox goldens and VA-API device-pending (checklists/I5.md). See the I5 entry. |
| platform-macos / platform-linux | **scaffold → filling in** | Real paths (+ Linux session-type detection); lanes M and L replace the stubs in place (see rows below). Crate-level `#![cfg(target_os = …)]` is the convention; pure-logic tests run on that OS's CI job. |
| D6 docs | **done** `c4c55d4` (Haiku, Opus-reviewed) | root README install/what-works-where + docs/FIRST-RUN.md |
| D5 permissions modal | **done** `1f3b607` (Sonnet) | host preflight: screenCapture + inputInjection; viewer preflight only with captureShortcuts on |
| L10 XDG autostart | **done** `136dbd9` (Haiku, Opus-reviewed) | `~/.config/autostart/sharectrl.desktop` |
| D3 keymap | **done** (Sonnet) | `platform_api::keymap` generated from Chromium `dom_code_data.inc` (`tools/gen-keymap`); NumLock kept at the plain `0x45` override until W3 (now removed); Power/edit keys excluded |
| M7 clipboard | **done** `8d2dea6` (Sonnet) | NSPasteboard text + change count + file URLs; `set_files` refuses relative paths |
| M9 autostart | **done** `2a7aa03` (Haiku, Opus-reviewed) | per-user LaunchAgent, no launchctl |
| L12 permissions | **done** `2f5c0b4` (Sonnet) | portal preflight + consent flow, restore-token store (`portal_token.rs`) |
| L6 input injection | **done** (Opus, Opus-reviewed; device-pending per checklists/L6.md) | `platform-linux/src/input/`: XTest, wlroots virtual pointer+keyboard, RemoteDesktop portal + libei (reis), opt-in uinput; selection reported in `permissions()`. The combined RemoteDesktop+ScreenCast portal session is held in `portal_shared.rs` over L1's `portal_session::start`: the injector, the capture (on a combined token, while the injector holds it, or whenever input would use the portal — so a first run shows one dialog) and the permissions Grant flow share it; the last holder closes it (single-use restore token). Tests: EIS always runs; Xvfb, uinput read-back and headless-sway tests run where their peer exists and **run in CI once T3/T4 land** (until then the CI job skips them — `SCT_X11_TESTS_REQUIRED=1` / `SCT_LINUX_INPUT_TESTS_REQUIRED=1` turn skips into failures). uinput read-back has not yet passed anywhere (WSL2 has no evdev). GNOME/KDE/real-device pass pending (checklists/L6.md) |
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
| M5 CGEvent injector | **done** (Opus) | `CgInjector` posts to `kCGHIDEventTap` from its own event source (local suppression 0, sentinel in `kCGEventSourceUserData`); refuses with `PermissionDenied` without Accessibility. Explicit modifier + device-bit flags on every event, modifiers and Caps Lock as `flagsChanged`, Caps Lock toggles per press, drags + click counts; wheel → 40 px/notch pixel events (pixels-per-line 40/3 = 3 lines/notch); `focus_window_at` = window list → AX raise → `AXFrontmost` + `NSRunningApplication.activate`; `paste_chord` Cmd+V. **Device-pending** (checklists/M5.md) |
| L9 Secret Service store | **done** `bc7f6d0` (Haiku, Opus-reviewed) | keyring-core + zbus store, fresh connection per call (no poisoned cache), `forget` implemented; daemon round-trip test. The dev-dep feature-unification build defect was caught by the gate and fixed |
| M8b Keychain `forget` | **done** `63e5466` (Haiku) | follow-up to M8 |
| L3 NVENC + VA-API encode | **done** `8baf62c` (Opus, independent FFI review: layouts verified with gcc; 9 findings fixed) | dlopen bindings, CBR/max-QP-32/one-frame VBV recipe, levels from size+fps, driver-header capture. NVENC verified on the GTX 1080 Ti; **VA-API device-pending** (checklists/L3.md) |
| L11 Linux packaging | **done** `8e15a11` (Sonnet) | deb/rpm/AppImage targets + deps, desktop template, hicolor icons, `uinput_access` (pkexec installer), `linux-build.yml`. deb built and inspected in WSL; AppImage/VM installs CI/device-pending (checklists/L11.md). **Owner TODO:** 1024×1024 icon |
| advisories | **clean** `e92735f` | tokio → 1.54.1; `cargo deny check` passes on all four triples |
| M2 VideoToolbox encode | **done** `096a90f` (Opus, Opus-reviewed; type-checked for aarch64-apple-darwin, device-pending) | H.264 + HEVC, hardware only (OpenH264 fallback via `WithSoftwareH264`). Low-latency rate control first (Apple: High profiles only → H.264 Constrained High, auto level); classic Main session with L3's level rule on `-12902`/`-12908`. Burst cap = WebRTC's 1.5 × target over 1 s (a one-frame hard cap would drop keyframes); max QP 32; live bitrate; sticky forced keyframes; lost sessions rebuilt with backoff. **Acceptance device-pending** (checklists/M2.md): `tests/vt_encode.rs` is all `#[ignore]` and CI's `cargo test --workspace` runs none of it; the GitHub macOS-runner step is T2's (`native-ci.yml`, in review) |
| M3 VideoToolbox decoder | **done** `a90ddfb` (Opus, Opus-reviewed; type-checked for aarch64-apple-darwin, device-pending) | H.264/HEVC/AV1 → IOSurface `420v` `CvPixelBufferFrame`; format changes keep or rebuild the session; synthetic I_PCM streams test it bit-exact on CI; encode→decode PSNR test and the golden-stream hash test run in T2's macOS CI step (`--ignored`, skip without hardware). **Device-pending** (checklists/M3.md) |
| M6 shortcut tap | **done** (Opus) | session `CGEventTap` on the hook thread; Cmd+Tab/Cmd+\`/Cmd+Space/Ctrl+arrows while the video view has the keyboard (focus published by the view into `SurfaceState::key_focus`); Cmd+Q left to the view's key monitor; macOS `input_monitoring` now needs Input Monitoring **and** Accessibility. **Device-pending** (checklists/M6.md); the Cmd+Tab acceptance is **blocked** on the owner's host-modifier-mapping decision |
| L2 X11 capture | **done** (Opus) | `platform_linux::X11Capture`: Damage `NonEmpty` → `DamageSubtract`/`FetchRegion` dirty rects (≤ 16, disjoint), MIT-SHM 1.2 segment read via its fd (core `GetImage` fallback), cursor out of band (`QueryPointer` + XFixes), RandR monitors as `x11:<name>` ids in `Platform::displays()`, refuses Xwayland; abstract-socket connect fallback (`x11_connect`). Xvfb/Xephyr tests in `tests/x11_capture.rs`. **Device-pending** (checklists/L2.md) |
| L7 shortcut capture | **done** `078793c` (Opus, Opus-reviewed) | XI2 keyboard grab (X11, deviation from `XGrabKeyboard` recorded on the card) + shortcuts inhibitor (Wayland); Xvfb acceptance green. **Device-pending** (checklists/L7.md) |
| L4 VA-API + NVDEC decode | **done** (Opus) | in-tree H.264 header parser + reference model (POC, MMCO, LTR, list modification, frame-num gaps) for stateless VA-API VLD; NVDEC through the driver parser (H.264/HEVC per caps; AV1 built but not advertised until the device check, `ADVERTISE_AV1_DECODE`); CPU NV12 out; golden streams + FFmpeg hashes in `native/testdata/golden`. NVDEC bit-exact vs FFmpeg on the GTX 1080 Ti; **VA-API device-pending** (checklists/L4.md). MF (W1) and macOS (M2) goldens → I5 |
| W3 keymap switch | **done** `e2d6b1c` (Sonnet, Opus-reviewed) | `input::scancode` is an adapter over the generated keymap; NumLock is E0 45 (plain 45 is Pause). Device check: NumLock LED toggles, Pause does nothing |
| W2 software codec selection | **done** (Sonnet, Opus-reviewed) | H.264 policy on Windows: hardware → MF software decoder → OpenH264 (only while `useOpenH264` is on); `h264_problem()` gives an actionable report when Media Foundation has no H.264 MFTs (N editions without the Media Feature Pack fail at launch — documented). Machine-B check: ignored test + log line "no hardware H264 decoder — using software decode" |
| D7 release pipeline | **done** `448df49` (Opus, Opus-reviewed) | tag-triggered `release.yml` → reusable `build-assets.yml` → `scripts/release-assets.sh` (one recipe); NSIS/DMG/AppImage/deb/rpm, SHA256SUMS, per-OS third-party notices generated and attached (never committed), version read from cargo metadata, actions pinned to SHAs; macOS: certificate required when the secrets exist, ad-hoc only without them. macOS leg + AppImage verified only on GitHub |
| T4 Wayland CI | **done** (Haiku, Opus-reviewed; jobs unverified until pushed; GNOME portal tests are device-pass items) | `native-ci.yml` jobs `wayland-sway` (ubuntu-24.04: sway + xdg-desktop-portal-wlr + pipewire + wireplumber; runs L5 `wayland_surface`, L1 `portal_capture --ignored`, and L6 `input::tests` minus uinput and `xtest_*`) and `wayland-gnome` (ubuntu-24.04: mutter 46 + xdg-desktop-portal-gnome; proves the stack, runs no test: the consent dialog has no headless pre-authorization, so the libei and GNOME portal-capture tests are device-pass items). Scripts in `.github/wayland/`. Local (lane, WSL): `wayland_surface` PASS (T4-02); `input::tests` wlroots and EIS pass, uinput fails under REQUIRED as designed (T4-03). Owner verification after the push: see the T4 card |
>| L5 video surface | **done** (Opus, Opus-reviewed; device-pending per checklists/L5.md) | X11: child window of the GTK toplevel on our own x11rb connection, XShape reveal clip, XDND drops via `XdndProxy`, Expose → redraw of the last frame. Wayland: the spike's area+video subsurface tree on GDK's connection (re-attached when GDK re-creates the toplevel surface), `wp_viewporter` crop + fractional scale, drops through GDK's own data device (sink window); decision-#3 separate window when there is no `wp_viewporter` or `SHARECTRL_VIDEO_WINDOW=separate`. Keys from the GTK toplevel on both (keymap: XKB − 8 = evdev). One wgpu presenter (Vulkan, GL fallback) over `surface-wgpu`. Headless tests: `tests/x11_surface.rs` (Xvfb), `tests/wayland_surface.rs` (headless sway). **Device-pending** (checklists/L5.md) |
| L1 Wayland capture | **done** (Opus, Opus-reviewed) | `portal_capture/`: portal ScreenCast (handshake shared with L12 in `portal_session.rs`, token scope kept: a combined token reopens a combined session) + pipewire-rs 0.10 stream thread, shared-memory BGRx/BGRA first (no modifiers → no DMA-BUF; L1b), cursor + damage metadata, pause/resize → `Lost` → resume; a stream-level failure → rebuild from the token on a background thread (reinit never blocks the host thread on a dialog); "Stop sharing" in the desktop indicator, a declined dialog or an unanswered one (120 s) end the hosted session (terminal, AnyDesk behaviour). The 120 s dialog wait on the host media thread happens only in the first `create_capture`. Monitor chosen in the portal dialog (Wayland `display` ids → `NoOutput`). Build needs `libpipewire-0.3-dev libspa-0.2-dev libclang-dev`. **Device-pending** (checklists/L1.md); acceptance test `tests/portal_capture.rs` (ignored) for T4. **Note for L6:** L6 layers a shared session slot over `portal_session::start(scope, previous, bound, abandon)` and extends `Granted` with the RemoteDesktop proxy; today the capture's `watch()` owns `granted.session` and closes it when the capture drops it. Once shared, that ownership moves into L6's slot, so only the last holder (capture or injector) closes the session. **Done in L6:** `portal_shared.rs` holds the combined session; the capture joins it (`Held::Shared`) when its token is combined, remote control already holds it, or remote control would use the portal, and owns only screen-only sessions itself |
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
* Depends on: D2. **Code done** (selection policy): hardware Media Foundation, then
  Microsoft's software MF codec, then Cisco OpenH264 only while `useOpenH264` is on;
  `WindowsPlatform::h264_problem()` tells Settings → Advanced when none exists, i.e. Media
  Foundation is present but its H.264 transforms are missing or blocked (e.g. by an
  administrator) and OpenH264 is off or unavailable. Matrix unit-tested with mocks.
  Not covered: a Windows "N" edition without the Media Feature Pack never starts the app
  (`mfplat.dll` is imported statically), so nothing of ours can advise there; that pack
  must be installed first. Acceptance (machine B decodes H.264 from machine A at ≥ 30 fps
  1080p) still needs the device: run the ignored test in `platform-windows`, then a real
  session. On machine B expect the log line "no hardware H264 decoder — using software
  decode" (Microsoft's software decoder); OpenH264 ("hardware H.264 decoder unavailable")
  is only expected when MF's software decoder is missing too.

### W3 — Keymap switch on Windows  (size S, Windows)
* Depends on: D3. Replace `input/src/scancode.rs` lookups with the generated table (Set-1
  column). Acceptance: existing scancode tests pass against the generated table; the
  104 old entries map identically (write a test that asserts it).
* **Done (W3).** `input::scancode` is now a thin adapter over `KeyId::{to,from}_set1`; the
  hand table is gone (kept only as a test fixture proving the 104 old entries). Callers
  (`Injector`, the viewer window's `code_for`) are unchanged in shape. `MapVirtualKeyW` is not
  used (layout-dependent). Decision, **NumLock parity override removed**: the old table sent
  NumLock as plain `0x45`, but Windows reads plain `45` as Pause (`E1 1D 45`, make code `45`
  not extended) and NumLock as `E0 45` (Chromium `0xe045`, Microsoft "About Keyboard Input"). So the old
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
  locally — **BLOCKED** on the owner's host-modifier-mapping decision (Cmd arrives as the
  Win key, so the host gets Win+Tab; see "Open" below); clicking another window restores
  local Cmd+Tab instantly.
* As built: Cmd+Q is **not** taken by the tap — it is an app menu shortcut that the video
  view's key monitor (M4) already sends to the remote. Force Quit and screenshots are never
  taken. An active tap needs **Accessibility** (Input Monitoring covers listen-only taps),
  so on macOS `PermissionReport.input_monitoring` — the row the viewer preflight asks for
  when shortcut capture is on — is `Granted` only with Input Monitoring **and**
  Accessibility, and requesting it asks for whichever is missing first (`permissions.rs`).
  Secure Input does not stop `install`: capture pauses while it is on and resumes by itself.
  Open (escalated to the owner): Cmd travels as `MetaLeft`, so a Windows host sees Win+Tab
  (Task View), not Alt+Tab; mapping Mac modifiers to the host OS's meaning is not part of
  any task yet.

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
* As built (deviations accepted 2026-11-01):
  * **Resize test runs on Xephyr nested in Xvfb.** Xvfb's RandR has exactly one mode, so
    `xrandr -s`, `--fb` and `--newmode/--addmode` all fail on it; Xephyr implements RandR
    size changes. Everything else runs on plain Xvfb.
  * **Abstract-socket connect.** WSLg mounts `/tmp/.X11-unix` read-only, so X servers
    there get only the Linux abstract socket, which x11rb 0.14 never tries (socket path,
    then TCP). `platform_linux::x11_connect` retries a local display through the abstract
    socket (same Xauthority lookup); the capture and the tests use it, so MIT-SHM runs in
    WSL too and every test capture asserts `X11Capture::uses_mit_shm()`. L8's file-list
    selection owner uses `x11_connect` too; clipboard *text* goes through arboard, which
    connects with plain x11rb.
  * **Clipboard tests (L8) on a private Xvfb.** WSLg's `:0` mirrors the Windows clipboard,
    so the X11 clipboard tests failed whenever a Windows `cargo test` ran alongside. They
    now share one private `Xvfb -terminate` per test process (shared harness
    `tests/support/xserver.rs`), with `DISPLAY` pointed at it before the first clipboard
    use; over TCP where `/tmp/.X11-unix` is read-only, because arboard cannot use the
    abstract socket. Assertions unchanged.
  * Test packages (for T3's CI job): `xvfb xserver-xephyr x11-xserver-utils xterm xclip`;
    set `SCT_X11_TESTS_REQUIRED=1` so a missing tool fails the job instead of skipping.

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
* As built: `cros-codecs` was not used (it links libva at build time, which decision #7
  rules out — same finding as L3); the stateless H.264 half is in-tree
  (`platform-linux/src/codec/h264dec`). VA-API decodes H.264 only (CB/Main/High, 8-bit
  4:2:0, progressive, no B-frames); HEVC/AV1 decode comes from NVDEC where the GPU has
  it. Goldens in `native/testdata/golden`: NVENC (L3), OpenH264 (D2) and an x264
  High-profile reference, checked bit-exactly against FFmpeg hashes. **The W1 (Media
  Foundation) and M2 (VideoToolbox) goldens cannot be produced in L4 and move to I5**;
  the tests pick up any stream dropped into that folder.
* AV1 via NVDEC is built but not advertised: set `ADVERTISE_AV1_DECODE = true`
  (`platform-linux/src/codec/mod.rs`) once checklists/L4.md step 2b passes on an
  Ampere-or-newer viewer.

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
* **Owed to L7 (the shortcut hook reads the surface only through `native_handles()`):**
  1. Implement `VideoSurface::native_handles()` — returning `None` turns shortcut capture
     off on Linux.
  2. X11: `video` = the video child window's XID; `parent` = the toplevel XID GTK gave
     `create_surface`; the video window is a **direct child** of that toplevel.
  3. Wayland: `parent` = the toplevel `wl_surface` (`create_surface`'s handle, not the
     video subsurface); `display` = GDK's `wl_display`.
  4. X11: `hide()` **unmaps** the video window and `show()` maps it (the hook's grab ends
     on unmap, which is what lets a modal be typed into).
* As built: the shader is M4's `surface-wgpu/shaders/present.wgsl` (shared through the
  `surface-wgpu` crate, not `platform-api/shaders`). X11 scale = GDK's integer window
  scale, **not `Xft.dpi`**: on X11 `Xft.dpi` scales only text, while CSS px (and so the
  40 px web toolbar the reveal uncovers) follow GDK's window scale. For the L7 hook:
  `native_handles()` = (video child XID / video `wl_surface`, the toplevel handle
  `create_surface` received, the display); the X11 video window is a **direct child** of
  that toplevel and `hide()` unmaps it (tested under Xvfb). The Wayland fallback window
  returns `None` (its toplevel surface is re-created on every show).
  Accepted deviations (review): keys go to the remote whenever the video is shown with an
  input sink (Windows takes them only after a click focuses the child HWND); a touch on
  the video produces no `VideoInput`. Wayland drops go through **GDK's own** data device
  (the sink window's GDK drag events, taken by a GDK event handler): compositors other
  than wlroots deliver a drag to one data device per client, so a second device would
  break either video drops or WebKit's own DnD. X11 needs the handle's own Xlib display
  (tao's); GDK's is refused (libX11 < 1.8 is not thread-safe). Tests:
  `tests/x11_surface.rs` runs its scenario twice (default backend, then `WGPU_BACKEND=gl`)
  and honours `SCT_X11_TESTS_REQUIRED=1`; `tests/wayland_surface.rs` fails instead of
  skipping when `SCT_WAYLAND_TESTS_REQUIRED=1`.

### L6 — Input injection: XTest / portal+libei / wlroots virtual input / uinput  (size L)
* `impl InputInjector` with runtime backend selection and `PermissionReport` for each
  (`portal_available`, `uinput_access`). Keys via `KeyId → evdev` (X11: +8). Wheel 120 units
  → buttons 4/5 (X11) / `NotifyPointerAxisDiscrete` (libei) / `REL_WHEEL` (uinput).
  `focus_window_at`: X11 `_NET_ACTIVE_WINDOW`; Wayland → `false`.
* Acceptance (CI): Xvfb + XTest types into `xterm` (verify via `xdotool getwindowname`/
  `xev` log); uinput path with `sudo modprobe uinput` on the runner types into the Xvfb
  session through `evdev`→X; libei path in the headless sway/GNOME job (`xdg-desktop-portal-gnome`
  in the mutter job) moves the pointer (verify with `wayland-info`/a test client).
* As built: selection order X11 XTest → uinput; Wayland wlroots → portal (RemoteDesktop ≥ 2)
  → uinput (uinput only when `/dev/uinput` is writable, i.e. after the L11 installer). The
  RemoteDesktop session is one combined RemoteDesktop+ScreenCast session per process
  (`portal_shared.rs`, layered over L1's `portal_session::start`) that **L1's capture shares** — the restore token is single-use. Wayland
  target rects are compositor logical coordinates (the ScreenCast stream's `position`/`size`).
  With no target (the engine has none on Wayland: the monitor is picked in the dialog), libei uses
  the EI region whose mapping id matches the granted stream's, else the region holding the stream;
  wlroots and uinput use the stream rectangle the capture publishes (`portal_monitor.rs`).
  In-repo tests: XTest on a private Xvfb, uinput read back through evdev (root), libei against an
  in-process EIS server, wlroots on a private headless sway; the real portal is `#[ignore]`.
  No target rect (the engine's primary display) maps onto the primary monitor's rectangle —
  X11: the RandR primary even off the root origin (as L2 reports it); uinput: that rect inside
  the root/layout; wlroots: the first output — never onto the whole desktop.
  Test servers: the XTest tests use L2's shared harness (`tests/support/xserver.rs`, a display
  number nobody holds — never WSLg's `:0`) and reach it through `x11_connect` (abstract socket
  when the socket file cannot exist), as production `XTest::connect` now does too. Skips print
  `SKIP:`/`skipping:` with the reason; `SCT_X11_TESTS_REQUIRED=1` (Xvfb) and
  `SCT_LINUX_INPUT_TESTS_REQUIRED=1` (uinput + evdev, sway) make them failures. The uinput
  read-back skips on kernels without evdev (WSL2); it runs in T3. The portal session closes
  itself when a start is abandoned mid-dialog.
  Device steps: [checklists/L6.md](checklists/L6.md).

### L7 — Shortcut capture: XGrabKeyboard / keyboard-shortcuts-inhibit  (size S) — report `install == false` when refused. Acceptance: Xvfb test shows Super/Alt+Tab delivered to our window while grabbed.
* Built (`platform-linux/src/shortcuts/`), on the `SurfaceHandles` / `native_handles()` seam
  added to platform-api for it (backend chosen from the handles, not the session type).
* **Deviation (approved): `XIGrabDevice`, not the core `XGrabKeyboard`.** Same semantics and
  refusals, but XI2 events carry the source device. The trait requires the hook to ignore
  events produced by our own injector (L6 types through XTEST), and a core grab cannot tell
  XTEST keys from real ones. XTEST input is not forwarded; the grab steps aside until focus
  leaves or the video is hidden (a grab cannot pass a key through). A press the grab took
  is replayed through XTEST after the ungrab, inside one server grab, according to the
  key's state then (still held → release + press; already up → press + release). The
  guarantee: no stuck key and no lost press; a racing tap may arrive twice.
* X11 grab held only while the app window has the input focus **and** the video window is
  viewable (relies on L5 unmapping it on `hide`; L5's obligations are on its card), so
  modals can be typed into. Keys already down when the grab starts get their key-up
  forwarded. Under **XWayland** (`GDK_BACKEND=x11` in a Wayland session) `install` refuses:
  an X grab cannot take the compositor's shortcuts. Wayland: inhibitor on the **toplevel**
  over GDK's `wl_display`; it never calls `forward` (the keys arrive through GTK like any
  other); it stays on while a modal hides the video (Super then goes to the web view — a
  behaviour follow-up, documented in the checklist).
* Keys the shared keymap lacks (media, brightness, vendor keys) are dropped while grabbed
  (neither remote nor local).
* Tests: Xvfb acceptance (WM passive grabs see nothing while grabbed; focus-out, unmap,
  real and virtual keys held across the grab start, AlreadyGrabbed, XTEST replay of a tap
  and of a held key); private Xvfbs (`-noreset`) from L2's shared harness
  (`tests/support/xserver.rs`), reached through `x11_connect` (abstract socket under
  WSLg) — the hook connects the same way. Wayland test is `#[ignore]`d.
  CI needs `xvfb` installed (T3). Device pass: [checklists/L7.md](checklists/L7.md).
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
* **As built.** One source of truth, three layers:
  `native/scripts/release-assets.sh` is the recipe (phases `steps` / `build` / `package` /
  `checksums`; the cargo commands are printed by `steps`, so a build queue runs exactly
  them); `.github/workflows/build-assets.yml` is a reusable workflow (`workflow_call`) that
  owns runner setup (packages, rustup, caches, pinned tool versions, signing secrets) and
  runs the script for one OS; `release.yml`, `mac-build.yml` and `linux-build.yml` only
  *call* it. M10's and L11's build steps moved into the script and the reusable workflow
  rather than being duplicated or called as whole workflows (calling them would have made
  release assets depend on two workflows with their own triggers and naming).
  `native/scripts/release-dry-run.sh` runs the same recipe on a developer machine.
* `release.yml`: `v*` tags publish; `workflow_dispatch` with `dry_run` (default on) builds and
  uploads everything, publishes nothing; publishing from a branch is refused. Version from
  `cargo metadata --format-version 1` (`scripts/release-version.sh`, jq or python), must equal
  the tag; `tauri.conf.json` no longer carries a `version` (Tauri takes Cargo's) and the script
  refuses one. `cargo deny --locked check` once (deny.toml covers all four triples). Assets:
  NSIS installer + `sharectrl-service.exe`, universal DMG (one, via `--target
  universal-apple-darwin`, i.e. `lipo`), AppImage, deb, rpm, `THIRD-PARTY-LICENSES-<os>.html`,
  `SHA256SUMS.txt`. Least-privilege `permissions` (only `publish` writes), concurrency per ref
  plus one global group for `publish`, actions pinned to commit SHAs.
* **Deviation from the card (approved by the orchestrator):** the `cargo about` output is
  **not committed**. A checked-in copy goes stale on every `Cargo.lock` change. Instead each
  OS build generates it for that OS's triples (`--target`), bundles it into the installer
  (`app/tauri.release.conf.json` adds it to `bundle.resources`) and attaches it to the
  release. `about.toml` gained `private = { ignore = true }` (as deny.toml) so `--fail` does
  not reject our own `Proprietary` crates.
* **macOS signing policy:** secrets set → the app in the DMG must carry a certificate
  signature, else the job fails (never an ad-hoc fallback after a failed signing); secrets
  absent (forks) → ad-hoc with a warning annotation and a line in the release notes. The
  temporary keychain lives in `$RUNNER_TEMP/mac-sign` and an `always()` step runs
  `macos-sign.sh --cleanup`.
* Verified locally: Windows leg end to end (lane D7-03 build + `package` + `sha256sum -c`; the
  notices file is in the NSIS script); Linux deb + rpm built in WSL (lane D7-04) and the
  `package linux` phase run on them (notices in both packages; the AppImage slot filled by a
  placeholder for that run); actionlint + shellcheck clean.
  **The AppImage is GitHub-only:** linuxdeploy fails in WSL (no FUSE, and the lane cannot set
  `APPIMAGE_EXTRACT_AND_RUN`). Also verified only by the first GitHub run: the macOS leg
  (universal build, signing, DMG), artifact merge and `gh release create`.

---

## Lane T — tests & CI

### T1 — CI matrix  (size S) — `native-ci.yml` → `{windows-latest, macos-latest, ubuntu-22.04}`: fmt, clippy `-D warnings`, `cargo test --workspace --exclude service` (Linux/macOS) with mock features; `deny.toml [graph].targets` += `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`; `about.toml` same + allow-list parity with deny (`MIT-0`, `CDLA-Permissive-2.0`).
### T2 — Hardware-codec CI on macOS runners  (size S) — M2/M3 tests run on `macos-latest` (VideoToolbox needs no permission). Gate on `VTIsHardwareDecodeSupported` and skip (not fail) when absent.
* As built (branch `task/T2`, in review; the macOS job has never run): every VideoToolbox hardware test probes first and skips with a printed line without the hardware (`tests/support/hardware.rs`). Encode probe: `h264_hardware_encoder()`, a real session with `RequireHardwareAcceleratedVideoEncoder`; only VideoToolbox's `NoEncoder` counts as "no hardware", any other error fails the test. Decode gate: `VTIsHardwareDecodeSupported`. `SCT_MAC_HW_TESTS_REQUIRED=1` turns a skip into a failure (opt-in: the macOS step passes the repository variable `vars.SCT_MAC_HW_TESTS_REQUIRED`, unset by default); the macOS step of `native-ci.yml` runs `vt_encoder::tests`, `decode::` and `tests/vt_encode` with `--ignored --nocapture`, skipping the two timing tests by name. Timing tests (`mean under 8 ms`, bitrate within 15 %) stay device-only (a loaded shared runner cannot hold a wall-clock budget). New: `decode::golden` checks VideoToolbox's H.264 (on whichever decoder it has, reported in the log) and HEVC (where hardware decodes it) NV12 output against FFmpeg's per-frame SHA-256 from `native/testdata/golden`, including one decoder across the three golden resolutions. `decode/golden.rs` duplicated `platform-linux/src/codec/golden.rs`; I5 replaced both with the shared dev-only `codec-goldens` crate (the test itself is unchanged). `macos-latest` is macOS 26 on arm64 (runner-images README, read 2026-11-01); whether that hosted image exposes VideoToolbox hardware is unproven (it runs inside Apple's Virtualization framework), so the first run decides whether strict mode can be switched on. **Not verified:** the CI run itself and any Mac; first real run is the owner's push of `port/multi-os`.
### T3 — X11 CI job  (size S) — `Xvfb` + `LIBGL_ALWAYS_SOFTWARE=1` + `xterm` + `xdotool`: runs L2, L5 (X11), L6 (XTest + uinput with sudo), L7.
  For L6: `apt install xvfb`; `sudo modprobe uinput`; give the runner user write access to
  `/dev/uinput` (`sudo setfacl -m u:$USER:rw /dev/uinput`, or `chmod 0666` on the throwaway runner)
  and read access to the `/dev/input/event*` nodes the test creates (add the user to `input`
  before the job's shell starts, or run `cargo test -p platform-linux input::tests::uinput` under
  `sudo -E`); set `SCT_X11_TESTS_REQUIRED=1` and `SCT_LINUX_INPUT_TESTS_REQUIRED=1` so a skip
  fails the job. The runner kernel has evdev (unlike WSL2), so the read-back really runs there.
* As built (branch `task/T3`, in review; the job has never run because the branch is not pushed): a `x11-tests` job in `native-ci.yml` on `ubuntu-22.04` with `SCT_X11_TESTS_REQUIRED=1`, `LIBGL_ALWAYS_SOFTWARE=1` and `SCT_LINUX_INPUT_TESTS_REQUIRED=1`. Steps, each `--nocapture`: `--test x11_capture` (L2), `--test x11_surface` (L5, runs itself twice, the second with `WGPU_BACKEND=gl`), `--lib clipboard::tests` (L8), `--lib shortcuts::xvfb_tests` (L7), `--lib input::tests::xtest` (L6 XTest, private Xvfb), `--lib input::tests::uinput` (L6 uinput read-back through evdev). The `input::tests` filter is narrowed on purpose: `libei_*` belongs to T4's EIS peer and `wlroots_*` needs sway, which this job does not install; under the REQUIRED variable the wlroots test would fail here. Apt: T1's Linux block plus `xvfb xserver-xephyr x11-xserver-utils xterm xclip xdotool acl`. The uinput setup is a `modprobe uinput`, `setfacl` on `/dev/uinput` for the runner user, and a udev rule (`MODE="0666"` on `KERNEL=="event*"`) for the nodes the test creates; group membership is not used, since it would not apply to the job's running steps. No global `Xvfb :99`: each test starts a private server through `tests/support/xserver.rs`. `build-test` keeps its `cargo test --workspace` leg unchanged; without `SCT_X11_TESTS_REQUIRED` a missing tool there is a printed skip.
* Skips that stay skips even in this job, by design: the Wayland-only clipboard tests (`wl-paste`, needs `WAYLAND_DISPLAY`) and `xwayland_is_refused` (needs a real Xwayland). The CI runner has neither.
* Runner needs (GitHub-hosted runner reference: Linux VMs run with passwordless `sudo`; each runner is a new VM): the apt packages above; Xvfb is in the 22.04 image already (runner-images Ubuntu2204-Readme). For L6 (uinput): the setup above, applied only on the throwaway runner.
* **Not verified:** the Actions run (the owner's push of `port/multi-os` is the first run). WSL runs the same commands with the REQUIRED variables; WSL has no evdev, so the uinput read-back cannot run there and fails under `SCT_LINUX_INPUT_TESTS_REQUIRED=1` (expected, recorded in the WSL run's log).
### T4 — Wayland CI job  (size M) — two jobs: (a) `sway --unsupported-gpu` with `WLR_BACKENDS=headless WLR_RENDERER=pixman` + `pipewire` + `wireplumber` + `xdg-desktop-portal` + `xdg-desktop-portal-wlr` under `dbus-run-session`, `portals.conf` set, `XDG_CURRENT_DESKTOP=sway` exported to the bus; runs L1 and the L5 Wayland surface (`tests/wayland_surface.rs`, which starts its own headless sway; set `SCT_WAYLAND_TESTS_REQUIRED=1` so a missing sway fails the job instead of skipping); (b) `mutter --headless --virtual-monitor 1920x1080` + `xdg-desktop-portal-gnome` for L6's libei path (copy the DeskShare `linux-wayland-e2e` recipe).
  For L6: `apt install sway` (the wlroots test starts its own headless sway with
  `WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1`, `xwayland disable`, in a
  private `XDG_RUNTIME_DIR`); set `SCT_LINUX_INPUT_TESTS_REQUIRED=1`. The mutter job runs
  `cargo test -p platform-linux -- --ignored portal_libei` inside the GNOME session with the portal
  consent pre-granted (or a restore token seeded). **Superseded by the As-built notes below:** no seeded-token path works (the tests use fresh token directories and the runner's permission store is empty), so the mutter job runs no libei test.
* **As built (task/T4, Haiku; NEW, never run: the branch is not pushed):**
  * Runner: both jobs `ubuntu-24.04`. sway 1.9 and xdg-desktop-portal-wlr 0.7 are the recent wlr pair; GNOME 46 (mutter with RemoteDesktop/ScreenCast on the bus) exists only on 24.04 (22.04 is GNOME 42, no libei). The 22.04 floor stays the build-test runner and the shipped target.
  * `wayland-sway`: `.github/wayland/sway-session.sh` under `dbus-run-session`. Headless sway (`--unsupported-gpu`, pixman, `LIBSEAT_BACKEND=noop`, `xwayland disable`, one `HEADLESS-1` 1920x1080 output, `weston-simple-shm` as the moving client); pipewire + wireplumber; `portals.conf` selects wlr for ScreenCast; wlr `[screencast] output_name=HEADLESS-1 chooser_type=none` (no dialog); `XDG_CURRENT_DESKTOP=sway` in the activation environment. The script waits until ScreenCast is on the bus and the wlr backend is registered, then runs `wayland_surface` (L5) and `portal_capture --ignored` (L1). `SCT_WAYLAND_TESTS_REQUIRED=1` and `SCT_LINUX_INPUT_TESTS_REQUIRED=1` are set job-wide. Logs upload on failure.
  * `wayland-gnome`: `.github/wayland/gnome-session.sh`. `mutter --headless --virtual-monitor 1920x1080 --wayland --no-x11`, `XDG_CURRENT_DESKTOP=GNOME`, portal stack. Proves `org.gnome.Mutter.ScreenCast` and `org.gnome.Mutter.RemoteDesktop` are on the bus and the RemoteDesktop portal plus the GNOME backend are registered. **Gap:** the libei test (L6) and the portal capture (L1) need the consent dialog answered once; no headless pre-authorization was found in the portal docs or the GNOME discussion (research 2026-11-01). **Decision (review round 1):** the job runs no portal test. The consent-gate variable was dropped: nothing in the repo lets a headless run pass the consent, and portal_libei asserts nothing about the pointer, so a gated run would be a fake pass. Making the libei and portal-capture tests run headlessly on GNOME is a follow-up (a consent-free path, or a real assertion in portal_libei). Until then both are device-pass items (checklists/L1.md, checklists/L6.md). No DeskShare `linux-wayland-e2e` recipe exists on this machine (searched `C:\Yanivs` top level and `deskConnect` for `wayland-e2e`, `ConnectToEIS` and `virtual-monitor`), so the job is designed from the mutter and portal docs.
  * Not verified here: both Actions jobs; the portal stack on a runner; the consent gap. The WSL lane can only run the L5 test (it starts its own sway). The L1 portal test and the libei test need a real portal session.
  * Local PASS (lane): T4-01 windows `cargo fmt --all --check`, `cargo check --workspace`, `cargo clippy --workspace --all-targets -- -D warnings` (`.claude\lane\done\T4-01.result`); T4-02 wsl `wayland_surface` with `SCT_WAYLAND_TESTS_REQUIRED=1` (`.claude\lane\done\T4-02.result`). The portal capture and the libei test cannot run in WSL. Windows static gate (fmt --check, check, clippy): T4-04 PASS on fa85598, T4-05 PASS on c26065c (`.claude\lane\done\T4-04.result`, `T4-05.result`); the final tip is gated by the request recorded in the hand-off.
  * L6 parts (added after L6 merged; the L6 note above is the spec):
    * `wayland-sway` runs `cargo test -p platform-linux --lib input::tests -- --nocapture --skip uinput_frames_reach_evdev --skip xtest_` with `SCT_LINUX_INPUT_TESTS_REQUIRED=1`: the wlroots test starts its own headless sway (`swaymsg` comes with the sway package), and the EIS test runs against its in-process server. Two filters, both by design: the uinput read-back (no `/dev/uinput` write access for the runner user) and the three `xtest_*` tests (need Xvfb and `SCT_X11_TESTS_REQUIRED`). T3's x11-tests job runs both.
    * `wayland-gnome` runs no L6 test. `portal_libei` needs an answered consent dialog on GNOME (see the decision above). It is a device-pass item.
    * Local (lane, WSL, T4-03, `.claude\lane\done\T4-03.result`): `input::tests` with `SCT_LINUX_INPUT_TESTS_REQUIRED=1`, no skip filter: 23 passed, 1 ignored, 1 failed. The wlroots test and the EIS test pass. The failure is `uinput_frames_reach_evdev` at tests.rs:341 (SKIP under REQUIRED: WSL has no evdev handler), which fails as designed. T3's x11-tests job (sudo, modprobe uinput) covers it on the runner. The wayland-sway job filters it out (see above). Not a blocker.
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

### I5 — Cross-codec matrix (in review)
* As built: `native/crates/codec-goldens`, a dev-only pure-Rust crate (sha2 + hex,
  already in the tree) that every codec crate takes as a dev-dependency.
  * `golden` loads `testdata/golden` and runs `check_decoder`, the table-driven
    row: every stream of a codec the decoder advertises, every frame's NV12 SHA-256
    against FFmpeg's framehash, plus a resolution change on one decoder. Hashing is
    a closure, so GPU decoders read back their own surface (MF: a D3D11 NV12 staging
    copy; VideoToolbox: the `420v` planes).
  * `shape` is the encoder conformance check: Annex-B, IDR first, the keyframe flag
    on exactly the IDRs, SPS/PPS(/VPS) in front of every IDR, and **no reordering**
    (the picture order count rises strictly between IDRs; low-delay B is accepted).
    It runs on every committed golden and on each encoder that can run.
  * `session` is L3's scripted host session: a forced IDR at frame 15 and a live
    bitrate drop at frame 22.
  * `skip` reads `SCT_CODEC_TESTS_REQUIRED`: `1`/`all` or a list of backend names.
  * platform-linux's `golden.rs` and T2's macOS twin loader are replaced by the
    shared crate.
* **Bug found and fixed (production code, own commit):** the Cisco OpenH264 decoder
  returned no picture for Main/High-profile streams, i.e. from NVENC, Media
  Foundation and VideoToolbox hosts, because OpenH264 buffers those pictures for
  reordering and `SoftwareDecoder` never flushed. It now flushes after each access
  unit that gave none. That is exact because no encoder in the project reorders. A
  regression test decodes the NVENC Main golden.
* Matrix on machine A: Media Foundation (Microsoft software H.264 decoder), OpenH264
  (Windows + WSL2) and NVDEC pass every golden of their codecs bit-exactly. MF and
  OpenH264 skip HEVC (not advertised / H.264 only). Encoder shape passes for NVENC,
  OpenH264 and MF.
* On machine A the MF encoder shape check ran against **Microsoft's software
  encoder**, because both NVIDIA MFTs failed to open (the owner's known quirk of
  that machine): `hardware H264 encoder failed (windows: Catastrophic failure
  (0x8000FFFF)) — trying the software encoder`, and the same line for Hevc. So
  no `mf-*` golden was written. They are **device-pending**: a Windows machine
  whose hardware MFT opens runs the ignored regenerate test, then the FFmpeg
  hash step, and commits them once (checklists/I5.md §1).
* Device-pending: the VideoToolbox goldens and encoder shape (`tests/vt_goldens.rs`,
  §2) and the VA-API decode + encoder shape (§3).
* CI: `SCT_CODEC_TESTS_REQUIRED=openh264` on windows-latest and ubuntu-22.04. The MF
  rows probe and skip until a run proves the runner has the MFTs (§4).
* Additive API: `codec::Encoder::is_hardware()`; `platform-windows`
  `adapters::mf_encoder()` (crate-private). No platform-api change.

## Parallel schedule (rough, 1 person per task unless noted)

```
week 1   F1 ─ F3 ─ D3 ─ D2(start) ─ D1(harness skeleton) ─ L5-spike ─ M10 cert/bundle ─ T1
week 2-3 F2+W1 (2 people) │ M1 M2 M3 M4 M5 M6 M7-9 (3-4 people) │ L1 L2 L3 L4 L5 L6 L8 (3-4 people) │ D1 D4 D5 T3 T4
week 4   W2 W3 M11 L7 L9 L10 L11 L12 D6 D7 T2 T5
week 5-6 I1-I5, bug-fix loop
```
Critical path: F1 → F2 → (W1 | M4 | L5) → I-tests. Everything else floats.
