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
| M6 shortcut tap | **done** (Opus) | session `CGEventTap` on the hook thread; Cmd+Tab/Cmd+\`/Cmd+Space/Ctrl+arrows while the video view has the keyboard (focus published by the view into `SurfaceState::key_focus`); Cmd+Q left to the view's key monitor; macOS `input_monitoring` now needs Input Monitoring **and** Accessibility. **Device-pending** (checklists/M6.md); the Cmd+Tab acceptance (Meta+Tab translated to Alt+Tab) depends on D8 (done; I2 device pass pending) |
| L2 X11 capture | **done** (Opus) | `platform_linux::X11Capture`: Damage `NonEmpty` → `DamageSubtract`/`FetchRegion` dirty rects (≤ 16, disjoint), MIT-SHM 1.2 segment read via its fd (core `GetImage` fallback), cursor out of band (`QueryPointer` + XFixes), RandR monitors as `x11:<name>` ids in `Platform::displays()`, refuses Xwayland; abstract-socket connect fallback (`x11_connect`). Xvfb/Xephyr tests in `tests/x11_capture.rs`. **Device-pending** (checklists/L2.md) |
| L7 shortcut capture | **done** `078793c` (Opus, Opus-reviewed) | XI2 keyboard grab (X11, deviation from `XGrabKeyboard` recorded on the card) + shortcuts inhibitor (Wayland); Xvfb acceptance green. **Device-pending** (checklists/L7.md) |
| L4 VA-API + NVDEC decode | **done** (Opus) | in-tree H.264 header parser + reference model (POC, MMCO, LTR, list modification, frame-num gaps) for stateless VA-API VLD; NVDEC through the driver parser (H.264/HEVC per caps; AV1 built but not advertised until the device check, `ADVERTISE_AV1_DECODE`); CPU NV12 out; golden streams + FFmpeg hashes in `native/testdata/golden`. NVDEC bit-exact vs FFmpeg on the GTX 1080 Ti; **VA-API device-pending** (checklists/L4.md). MF (W1) and macOS (M2) goldens → I5 |
| W3 keymap switch | **done** `e2d6b1c` (Sonnet, Opus-reviewed) | `input::scancode` is an adapter over the generated keymap; NumLock is E0 45 (plain 45 is Pause). Device check: NumLock LED toggles, Pause does nothing |
| W2 software codec selection | **done** (Sonnet, Opus-reviewed) | H.264 policy on Windows: hardware → MF software decoder → OpenH264 (only while `useOpenH264` is on); `h264_problem()` gives an actionable report when Media Foundation has no H.264 MFTs (N editions without the Media Feature Pack fail at launch — documented). Machine-B check: ignored test + log line "no hardware H264 decoder — using software decode" |
| D7 release pipeline | **done** `448df49` (Opus, Opus-reviewed) | tag-triggered `release.yml` → reusable `build-assets.yml` → `scripts/release-assets.sh` (one recipe); NSIS/DMG/AppImage/deb/rpm, SHA256SUMS, per-OS third-party notices generated and attached (never committed), version read from cargo metadata, actions pinned to SHAs; macOS: certificate required when the secrets exist, ad-hoc only without them. macOS leg + AppImage verified only on GitHub |
| D8 modifier translation | **done** (Opus, Opus-reviewed; I2 device pass pending) | viewer-side `KeyTranslator` (`engine/src/modifiers.rs`): Cmd ↔ Ctrl swap and Cmd+Tab→Alt+Tab (Mac → PC), Ctrl ↔ Cmd and Alt+Tab→Cmd+Tab (PC → Mac), sent-code tracking; host OS via new optional `connect-response.os`; `translateModifiers` toggle (default on) in Settings → Advanced; unit + loopback-tested. **Device-pending** (I2 Mac↔Windows); closes M6's blocked Cmd+Tab line |
| M12 take every Cmd shortcut (macOS viewer) | **done** (Opus, Opus-reviewed, merged 99ed737; type-checked for aarch64-apple-darwin, device-pending) | M6 tap takes every Cmd chord + Ctrl+arrows while the video has the keyboard; Cmd+Q stays local (tap and key monitor); Force Quit / Lock Screen never claimed; every modifier synchronised before a taken key; hook keys now share the surface's FIFO input channel (D8 ordering risk closed); focus loss releases at once. Also: toolbar-reveal diagnostics + layer-frame sync + full-screen cover (owner's "top edge does nothing"). **Device-pending** (checklists/M6.md §6–§7) |
| D9 host cursor sprite on the viewer | **done** (Opus, Opus-reviewed; device-pending I1–I3: real cursor images incl. I-beam in a terminal) | additive cursor-shape message, host sends on change, viewer draws the real sprite at position−hotspot in both presenters; replaces the synthetic triangle |
| D10 viewer cursor lag on a high-RTT link (draw at local pointer + hide OS cursor) | **done** `da65e25` (Opus, Opus-reviewed; Windows+WSL lane green incl. loopback×3, platform-macos aarch64 type-checked; device-pending I1–I3) | capture already keeps the cursor out of the video; the drawn sprite followed the host out-of-band echo (a round trip behind) while the local OS cursor was instant → two cursors. Fixed: the sprite is drawn at the LOCAL pointer when controlling and over the video (client-side prediction), and the OS cursor is hidden over the video; SHAPE stays the host's (D9) |
| D11 controller cursor freezes between video frames (decouple drawn cursor from frame arrival) | **in review** (branch `task/D11`, Opus; Windows + WSL lane green incl. loopback×3, platform-macos aarch64 type-checked; device-pending checklists/D11.md; NO platform-api change) | D10 made it visible: on a static host screen few/no frames arrive, but the drawn sprite only repainted inside the decoded-frame branch, so the local pointer froze then snapped forward. Fixed: the newest decoded `Frame` is held and the idle tick (~16 ms) re-presents it with the CURRENT cursor when the cursor moved; `cursor_position` no longer vanishes on a momentary host-cursor blip while controlling (always draws at the local pointer); Windows `pointer_inside` re-evaluated on focus regain / control-on so the sprite doesn't drop to the stale host spot |
| W4 installer bundles + registers the SYSTEM service | **done** (Opus, Opus-reviewed, merged d0119b4; Rust lane-proven, installer by new CI job + machine-A smoke pending; device pass checklists/W4.md) | NSIS hooks install/start/stop/uninstall `sharectrl-service.exe` next to `sharectrl.exe`; one host instance rule; CI silent install/upgrade/uninstall job. Acceptance narrowed: service setup + one instance + standalone asset; elevated-window control is NOT a W4 claim |
| W5 UIAccess (Alt+Tab / Win key passthrough) | **done** (Opus, Opus-reviewed; device-pending checklists/W5.md: Alt+Tab/Win via the service) | route (a) only: the service sets `TokenUIAccess` (SeTcb) on a duplicate of the host token; `--print-uiaccess`; startup log line; fallback launches without the flag and logs why. Signed route deferred to W6 (see the card) |
| W7 uninstaller removes the whole install dir (service log leftover) | **done** (Opus, Opus-reviewed, merged 03d98af; proven by the windows-installer CI job on push) | first windows-installer CI run failed only on uninstall-clean: `sharectrl-service.log` left in Program Files; RMDir /r $INSTDIR in POSTUNINSTALL |
| U1 in-app updater (check/download/install/restart) | **done** (Opus, Opus-reviewed; device-pending U1.md: real update loop needs two updater-enabled releases) | tauri-plugin-updater 2.14.1 + tauri-plugin-process 2.4.0, updater signing key (orchestrator-created secrets), latest.json generated on the release, Settings UI, service-aware install+restart; TOP priority |
| U2 Settings/Update unreachable while hosting | **done** (Opus, Opus-reviewed, merged 3df0e22; UI-only) | added `btn-host-settings` gear to `screen-host`'s header, bound (with the viewer gear) to the shared `openSettingsOverVideo()` → `openSettings()`; `closeSettings()` restore generalised to `sessionLive()`. UI-only (index.html + app.js), no Rust. Settings→Updates now reachable mid-host-session; U1 live-session warning intact |
| W8 service-host fast path (DDA capture + HW encode) | **done** (Opus, Opus-reviewed, merged 0afabeb; capture fast-path implemented, HW-encode fix device-pending hypothesis — checklists/W8.md) | service-launched host loses the fast DDA path + HW encoder → software fallback + media-loop restarts = lag; attach the capture thread (and so the shared device the HW MFT uses) to the active input desktop + reclassify E_ACCESSDENIED as recoverable and retry instead of ending the loop; goal widened to restore capture AND HW encode (owner: was smooth self-launched) |
| M13 macOS toolbar drawer not shown (reveal fires, strip empty) | **done** (Opus, Opus-reviewed, merged bcf7b6f; type-checked for aarch64-apple-darwin, device-pending checklists/M13.md) | Root cause: `apply_layout` positioned the hosted layer's 40px gap with the parent *view*'s `isFlipped`, but a hosted layer's frame lives in the parent *layer*'s coordinate space, whose geometry flip is independent (wry: unflipped view, geometry-flipped WKWebView layer) — so the gap opened at the wrong visual edge and the video kept covering the toolbar. Fix (view.rs only; shared reveal.rs untouched): drive the layer frame from the parent layer's own `isGeometryFlipped`, so the strip is always at the visual top. Secondary: ignore `mouseExited` reveals mapped more than one bar height above the top (the app.log's y≈-470/-1025 artifacts). M12 reveal logging kept |
| U3 Linux .deb/.rpm cannot self-update | **done** `0c99ec3` (Opus, Opus-reviewed; Rust lane-proven windows+wsl incl. loopback×3 + deny, UI + device-pending checklists/U3.md) | ROOT CAUSE: the feed serves an AppImage only, so a .deb/.rpm install downloads a payload whose type mismatches the install and the updater rejects it ("invalid updater binary format"). Fix: pure `update_capability(is_windows, is_macos, appimage)` → `SelfInstall`/`ManualDownload` (AppImage-mount detection mirrors autostart's `choose_exe`: `$APPIMAGE` + `$APPDIR` + exe under the mount, so a `.deb` launched from inside another AppImage is not mistaken for one); `UpdateInfo` gains `selfUpdatable` + `downloadUrl`; `updater_install` early-returns for a packaged install (never attempts the install); UI swaps "Update & restart" for a "Download update" link to the releases page (opener = `open` crate `#[tauri::command]`). Windows/macOS/AppImage unchanged |
| V1 host overshoots a congested link (video stutter) | **done** (Opus, Opus-reviewed, merged 003fbf8; loopback proves encoder tracks BWE down+up; on-screen smoothness device-pending checklists/V1.md) | ROOT CAUSE (investigation): July 78cd049 raised MIN_BITRATE 400k→2.5M + DESIRED→9M + a raw-BWE source pacer → encoder wont follow BWE down → overshoot→loss→BWE collapse (65↔3700), unpaced keyframe bursts. Fix (As built): MIN_BITRATE→500k + DESIRED→8M so the encoder follows BWE down; a hysteretic resolution ladder (1.0/0.75/0.5/0.33) spends degradation on encode size, fps drops only below the smallest step; the source pacer is removed (str0m paces egress) and forced IDRs no longer bypass it. Owner: Windows went slow when the service came in (lost the fast path → tipped the latent overshoot); Mac/Linux slow from the start |
| W9 Alt+Tab on a service host | **blocked: need controller app.log** | host UIAccess granted + injects reserved shortcuts, yet Alt+Tab does not switch → controlling-side capture/forward issue (keyhook on a Windows controller, or the Mac tap) |
| T4 Wayland CI | **done** (Haiku, Opus-reviewed; jobs unverified until pushed; GNOME portal tests are device-pass items) | `native-ci.yml` jobs `wayland-sway` (ubuntu-24.04: sway + xdg-desktop-portal-wlr + pipewire + wireplumber; runs L5 `wayland_surface`, L1 `portal_capture --ignored`, and L6 `input::tests` minus uinput and `xtest_*`) and `wayland-gnome` (ubuntu-24.04: mutter 46 + xdg-desktop-portal-gnome; proves the stack, runs no test: the consent dialog has no headless pre-authorization, so the libei and GNOME portal-capture tests are device-pass items). Scripts in `.github/wayland/`. Local (lane, WSL): `wayland_surface` PASS (T4-02); `input::tests` wlroots and EIS pass, uinput fails under REQUIRED as designed (T4-03). Owner verification after the push: see the T4 card |
| L5 video surface | **done** (Opus, Opus-reviewed; device-pending per checklists/L5.md) | X11: child window of the GTK toplevel on our own x11rb connection, XShape reveal clip, XDND drops via `XdndProxy`, Expose → redraw of the last frame. Wayland: the spike's area+video subsurface tree on GDK's connection (re-attached when GDK re-creates the toplevel surface), `wp_viewporter` crop + fractional scale, drops through GDK's own data device (sink window); decision-#3 separate window when there is no `wp_viewporter` or `SHARECTRL_VIDEO_WINDOW=separate`. Keys from the GTK toplevel on both (keymap: XKB − 8 = evdev). One wgpu presenter (Vulkan, GL fallback) over `surface-wgpu`. Headless tests: `tests/x11_surface.rs` (Xvfb), `tests/wayland_surface.rs` (headless sway). **Device-pending** (checklists/L5.md) |
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

### W4 — Windows installer bundles, registers and starts the SYSTEM service  (size S–M, Windows; owner request 2026-11-02)
* Problem: the NSIS installer puts only `sharectrl.exe` in Program Files. The SYSTEM service
  (`sharectrl-service.exe`, lane §8b) is a separate download and a manual admin step, the
  service launches the host from ITS OWN folder (`service/src/main.rs` joins
  `sharectrl.exe` to `current_exe().parent()`), and nothing says so. Result in the owner's
  live test: the service was mistaken for the app, and a plain install could neither see nor
  control a UAC prompt or an elevated window (UIPI + secure desktop; README "Elevated
  windows are not controllable").
* Deliverable: one installer that leaves a host machine fully set up.
  (1) `sharectrl-service.exe` ships INSIDE the NSIS bundle, installed next to
  `sharectrl.exe` in `$INSTDIR` (Tauri `bundle.resources` or an equivalent that keeps the
  file name; `scripts/release-assets.sh` already builds `-p service --release` before
  `tauri build`; keep the standalone asset too for manual installs).
  (2) Tauri NSIS installer hooks (`bundle.windows.nsis.installerHooks`, a `.nsh` file under
  `native/app/`): post-install → stop an existing service if present, run
  `"$INSTDIR\sharectrl-service.exe" --install-service`, start it; pre-uninstall → stop +
  `--uninstall-service`; upgrade → service stopped before files are replaced and started
  after, never leaving a stopped service behind. Installer stays `perMachine` (elevated).
  Every hook step logs to the NSIS details pane and a failure aborts with a message that
  names the step.
  (3) Exactly one host instance: when the service is installed and running, the app's own
  Start-with-Windows registration is not created / is removed (the service instance IS the
  autostart), and a user-launched `sharectrl.exe` that finds the service instance running
  exits with a log line and (if a single-instance mechanism exists) brings its window up
  instead of silently dying. Document the rule in README.
  (4) The service's launch of the host carries the host's working directory, environment
  and `--autostart` semantics exactly as today (`elevation::session` invariants).
* Research: Tauri v2 "Windows Installer" → Installer hooks; NSIS `nsExec::ExecToLog`
  (bundled plugins only, no SimpleSC); Microsoft Service Control Manager docs for
  stop/start ordering. Cite them in the `.nsh` header and the Rust `//! ## References`.
* Tests / CI: a `windows-installer` job (native-ci, windows-latest) builds the NSIS bundle
  (reuse the release-build job's artifact if cheaper), installs it silently (`/S`), asserts
  the service exists and is RUNNING (`sc query`) and `sharectrl-service.exe` sits next to
  `sharectrl.exe` under Program Files, then uninstalls silently and asserts the service is
  gone. Unit tests for any new Rust (instance rule, autostart decision).
* Acceptance (narrowed 2026-11-02, see As built): on a real Windows host, one installer run
  sets up the service — it is registered, auto-start and RUNNING afterwards; an upgrade over
  it leaves it running; uninstall removes it and the folder. Exactly one host instance (a
  second launch raises the running one and exits). The standalone `sharectrl-service.exe`
  asset still installs by hand next to a portable `sharectrl.exe`. Whether a service-run host
  can see/control a UAC prompt or the lock screen is NOT a W4 outcome (the service launches
  the host with the user's token, unchanged by W4; that capability is tracked separately).
  Orchestrator smoke on machine A.

* As built (branch `task/W4`, in review; Rust proven in the lane, the installer only by the
  new CI job — see "not verified"):
  * **Service lifecycle** (`crates/elevation/src/service.rs`, rewritten): `install` creates
    the LocalSystem auto-start service, or re-points an existing one with
    `ChangeServiceConfigW` (upgrade / moved-into-Program-Files), then `StartService` and polls
    `QueryServiceStatusEx` to RUNNING. `stop` waits for STOPPED **and then for the service
    process to exit** (`OpenProcess(SYNCHRONIZE)` on the pid from the status), because the SCM
    reports STOPPED before the process releases its locked `.exe`, which the installer then
    overwrites/deletes. `uninstall` stops first (DeleteService only marks a running service).
    `query` + `ServiceStatus::owns_sign_in()` drive the autostart rule and need no admin (the
    default SD grants every user `QUERY_STATUS`/`QUERY_CONFIG`). Errors name the failed step;
    `ERROR_ACCESS_DENIED` → an actionable "needs administrator" message; idempotent across
    fresh install / upgrade / re-install. `service/src/main.rs` exposes
    `--install/-start/-stop/-uninstall-service`, each printing one line and exiting 0/1 (2 for
    bad usage) so the NSIS hooks' `nsExec::ExecToLog` surfaces the reason and the exit code
    gates the install; `--install-service` refuses unless `sharectrl.exe` is beside it.
  * **Host launch UNCHANGED in substance** (decision 2026-11-02, risk 1): the service still
    launches the host into the active console session via `WTSQueryUserToken` +
    `CreateProcessAsUserW` on `winsta0\default` with the user's environment block (§8b
    invariants intact). The only change is the argument string: `--engine` (which the app
    never read) → `--autostart --service-host`. `--autostart` because the service instance IS
    the sign-in autostart, so it comes up minimized; `--service-host` marks it the session's
    one host. The user token is now closed after the launch (it leaked before).
  * **Exactly one host per session** (`crates/platform-windows/src/instance.rs`, new): the
    service host creates a named auto-reset event `Local\ShareCtrlScreen.ServiceHost` (per
    session; destroyed with its last handle, so no stale lock after a crash) and holds it for
    life. A later launch opens it: a user launch sets it (after `AllowSetForegroundWindow`),
    which makes the host unminimize/show/focus, then exits; a sign-in or duplicate service
    launch exits quietly. With no service present the marker never exists and every launch
    runs — nothing changes for a serviceless install. The decision table is a pure, unit-
    tested `decide()`; `app/src/main.rs` runs the gate before logging so a handed-over launch
    logs to `app-handover.log` instead of truncating the running host's `app.log`.
  * **Autostart made service-aware** (`crates/platform-windows/src/autostart.rs`): while the
    service owns sign-in (`owns_sign_in()`), the per-user Run entry is removed and never
    written — the service is the autostart, a Run entry would be a second host. `set_enabled`
    still returns `Ok` in that case (the UI sends `startWithWindows` on every Save; erroring
    would block unrelated settings), the saved bool is kept, so the entry returns by itself if
    the service is removed. A query failure counts as "no service" (never worse than pre-W4).
    The write/remove decision is a pure, tested `plan()`.
  * **Installer** (`native/app/installer-hooks.nsh` + `tauri.windows-release.conf.json`, new):
    `sharectrl-service.exe` ships as a bundle **resource** (installed next to `sharectrl.exe`
    in `$INSTDIR`, name preserved) and the `installerHooks` `.nsh` is wired through a Windows-
    only release overlay (second `--config`, so a plain `cargo build` never sees the resource
    that tauri-build would check at compile time). PREINSTALL stops an old service (using the
    NEW exe extracted to `$PLUGINSDIR`; two embedded copies, justified in the `.nsh` header),
    POSTINSTALL registers+starts, PREUNINSTALL stops+removes (with an `sc.exe` fallback if the
    exe was deleted by hand). Each step logs to the details pane via `nsExec::ExecToLog` and a
    failure `Abort`s naming the step; stays `perMachine`. `release-assets.sh` stages the exe
    with `cargo install --path ../service --root service-bin` (fixed path independent of
    `CARGO_TARGET_DIR`) and still ships the standalone asset (the same file).
  * Tests: `elevation` (`owns_sign_in`/state mapping/error mapping, a no-admin `query`);
    `service` (launch args, host-beside-service, and that `installer-hooks.nsh` names the same
    service and commands via `include_str!`); `platform-windows` `instance::` (flag parsing,
    the decision table, a real event claimed/opened/raised/dropped) and `autostart::` (the
    `plan()` table, a registry round-trip restoring the exact prior value). The installer
    itself (silent install → service RUNNING + both exes in Program Files → upgrade restarts
    it → silent uninstall removes service+folder) is the new `windows-installer` CI job.

### W5 — UIAccess for the Windows host: Alt+Tab / Win key passthrough  (size M, Windows; owner request 2026-11-02; after W4 merges, rebase onto it)
* **Scope decision 2026-11-02 (orchestrator, after the first research pass):** W5 ships route (a)
  only — the service route. Route (b), the signed `uiAccess="true"` build, moves to a
  follow-up card (W6) because the research showed it is a different, larger job: Tauri signs
  inside `cargo tauri build` (main exe, bundled .exe resources, uninstaller, installer) so
  the secrets must reach the Build step through `bundle.windows.signCommand`; the manifest
  is not embedded today at all (`build.rs` only calls `tauri_build::build()`, Tauri's default
  Common-Controls manifest is used; `sharectrl.manifest` is unreferenced) and would go in via
  `tauri_build::WindowsAttributes::app_manifest`; a `uiAccess="true"` exe cannot be started by
  plain `CreateProcess` from a caller without UIAccess (error 740: installer "run after
  install", CI smoke, and the service launch unless it sets the token flag first); the
  portable copy would need its manifest reset to `false` and a re-sign or it may not start on
  a machine without the certificate; and Microsoft does not document whether an out-of-path
  or untrusted signed UIAccess exe still launches. The owner's two Windows hosts are
  installed by W4 (service), so route (a) alone delivers Alt+Tab / Win key for them.
* **Route (a) as scoped:** the SYSTEM service, which already holds the user's token from
  `WTSQueryUserToken` and `SeTcbPrivilege`, sets `TokenUIAccess` on the token it hands to
  `CreateProcessAsUserW` — the documented way Windows itself launches assistive tools (Ease
  of Access / utilman) — then the host's existing `elevation::process_has_uiaccess()` reports
  the flag. Deliver: the token step in `elevation::session` with the privilege enabled
  explicitly and every failure mapped to an actionable error (the service falls back to a
  launch WITHOUT the flag and logs why, never refuses to launch the host); the startup log
  line and `--print-uiaccess` from the card; README / FIRST-RUN / release-notes wording "Alt+Tab
  and the Windows key pass through when the host runs under the service"; checklist W5.md
  (service mode: granted + Alt+Tab switches + Win opens Start; user-launched mode: `not
  granted — the host was not started by the service`; portable: same). No manifest change, no
  certificate, no signing in W5.
* Problem: synthesized Alt+Tab and the Windows key are refused by the shell unless the
  injecting process has the UIAccess token flag (Microsoft, "Security Considerations for
  Assistive Technologies" / UIPI). Our manifest (`native/app/sharectrl.manifest`) is
  `asInvoker uiAccess="false"`; `elevation::process_has_uiaccess()` already exists and the
  host warns at `platform-windows/src/lib.rs:162` when it is missing.
* Two routes; research both, implement both unless the docs rule one out, and say which
  applies when:
  (a) **Service route (no certificate):** the SYSTEM service holds `SeTcbPrivilege`, so when it
  launches the host it can set `TokenUIAccess` on the user token it passes to
  `CreateProcessAsUserW` (`SetTokenInformation(TokenUIAccess)`, documented to need TCB).
  Verify in the docs that the runtime check for reserved shortcuts is the TOKEN flag, not the
  manifest, and that the duplicated token keeps the flag. This makes Alt+Tab work for every
  service-installed host (W4) with no signing at all.
  (b) **Signed route (user-launched host from Program Files):** Windows grants UIAccess at
  process start only when ALL hold: manifest `uiAccess="true"`, an Authenticode signature
  chaining to a certificate in the machine's Trusted Root (self-signed is fine when the
  installer imports the public cert into `LocalMachine\Root`, code-signing EKU only), and
  the exe lives under a secure path (Program Files / System32).
  HARD CONSTRAINT: an exe whose manifest says `uiAccess="true"` but is NOT validly signed
  fails to launch ("A referral was returned from the server") — so the flag may only be
  embedded in builds that will be signed (build-time switch via env/feature in
  `native/app/build.rs`; dev and unsigned CI builds keep `uiAccess="false"`), and the
  portable copy run from a Downloads folder keeps working (UIAccess simply not granted;
  log says why). Certificate: `native/scripts/windows-make-signing-cert.ps1` mirroring
  `macos-make-signing-cert.sh` (self-signed code-signing cert → PFX; secrets
  `WIN_SIGN_PFX_B64` / `WIN_SIGN_PFX_PASS`; private key only in GitHub secrets, the
  orchestrator creates it blind like the macOS one); `native/scripts/windows-sign.ps1`
  (signtool from the Windows SDK on the runner, SHA-256) run in the package phase for
  `sharectrl.exe`, `sharectrl-service.exe` and the installer; without the secrets:
  unsigned, `uiAccess="false"`, loud `::warning`. The installer hook (W4's `.nsh`, separate
  banner section) imports the public certificate into `LocalMachine\Root` and removes it
  on uninstall.
* Host startup logs one line: `UIAccess: granted` or `UIAccess: not granted — <reason:
  unsigned build | not under Program Files | service did not set the flag>`.
* Tests / CI: unit tests for the manifest switch and reason text; the W4 installer job
  extends to: import the cert, install silently, launch `sharectrl.exe --print-uiaccess`
  (new diagnostic flag that prints the token flag and exits 0/1) and assert granted; also
  assert the unsigned dev build still launches. Document in README (feature table "Alt+Tab
  passthrough" row, SmartScreen note unchanged) and `docs/FIRST-RUN.md`.
* Acceptance: on a real host installed by W4, `app.log` says `UIAccess: granted`, and from
  the viewer Alt+Tab switches windows and the Win key opens Start — in service mode at
  minimum; user-launched mode too when route (b) is implemented. Checklist
  `plans/checklists/W5.md` with both modes.
* **As built (2026-11-02, route (a) only):**
  * `elevation::uiaccess` is the new module. `primary_token_with_uiaccess(user_token)`
    duplicates the user's token (`DuplicateTokenEx`, exactly the rights the launch needs),
    enables `SeTcbPrivilege` on the service's own token (`AdjustTokenPrivileges`, with the
    `ERROR_NOT_ALL_ASSIGNED` check because that call "succeeds" without the privilege), then
    sets `TokenUIAccess = 1` on the duplicate. It returns `(Option<OwnedToken>, Result<(),
    UiAccessError>)`; `UiAccessError` carries an actionable message per failure.
  * `elevation::session::launch_in_session` now returns `Launched { pid, ui_access }` and
    builds the process from the flagged duplicate. **Fallback invariant:** if the flag fails
    it launches from the duplicate without it; if even the duplication fails it launches from
    the original `token`. Desktop, environment block, command line, working directory and
    creation flags are unchanged. `--autostart`/launch-args line left as-is for the W4 rebase.
  * `elevation::process_has_uiaccess()` is now a thin `matches!(query_process_uiaccess(),
    Ok(true))`; the new `query_process_uiaccess() -> windows::core::Result<bool>` keeps the
    error for the host's startup line. Same signature/behaviour for existing callers.
  * The service logs one line per launch (`with UIAccess` / `WITHOUT UIAccess … <reason>`),
    now closes the user token each launch (was leaked), logs at info by default and, under
    the SCM (`--run-service`), writes `sharectrl-service.log` next to the service exe (not
    under ProgramData — a standard user could pre-plant a link there and redirect SYSTEM's
    truncate).
  * `app/src/uiaccess.rs`: `UiAccessStatus` with the exact `line()` and `exit_code()`.
    `sharectrl.exe --print-uiaccess` prints the line and exits 0/1 before any logging,
    window or network. On startup (Windows only) the host logs the same line once to
    `app.log`. The text/exit mapping is pure and unit-tested on every OS; the token call is
    Windows-only behind `cfg`.
  * Reasons emitted: `the host was not started by the service` (flag absent) or `the token
    query failed: <err>`.
  * Not done here (scope): no manifest change, no certificate, no signing, no installer hook
    (route (b) is W6; the installer is W4).
  * Tests: error-mapping and flag-parsing units; a Windows unit that proves an unprivileged
    process gets the actionable refusal (the fallback path in miniature); one device-only
    `#[ignore = "needs the installed service"]` test that runs as SYSTEM, flags the console
    user's token and reads it back.
  * Not verified by CI: a real host started by the service (the token call only does
    anything as SYSTEM). Verify with `plans/checklists/W5.md`.

### W7 — Uninstaller removes the whole install directory, including the service's runtime log  (size XS, Windows; from the first windows-installer CI run 2026-11-02)

* Problem: the new `windows-installer` CI job (native-ci, added by W4) FAILED its final check on the
  first run on `main`. Install, service RUNNING/Auto/LocalSystem, and the reinstall/upgrade
  (new PID) all PASSED. The uninstall check failed: after `uninstall.exe /S`, the service is
  gone (`sc query` → 1060) but `C:\Program Files\ShareCtrlScreen\` still exists because it
  holds one runtime-created file, `sharectrl-service.log` (W5's service log, deliberately
  beside the exe in the admin-only Program Files dir — do NOT move it to ProgramData, W5's
  reviewer established that is the safe location). The NSIS uninstaller only deletes files it
  recorded at install time, so the log it never installed is left behind, and with it the
  folder. The CI assertion `throw "$dir was not removed"` fires. A real user also sees the
  leftover folder + log after uninstall.
* Deliverable: the uninstaller removes the install directory and everything in it, including
  runtime-created files (the service log, any crash dump), AFTER the service is stopped and
  removed. In Tauri's NSIS uninstaller this is the POSTUNINSTALL hook in
  `native/app/installer-hooks.nsh` (the service is already `--uninstall-service`'d in
  PREUNINSTALL): add `RMDir /r "$INSTDIR"` guarded so it only runs when `$INSTDIR` is the real
  per-machine install dir (never a root/system path — check it ends in `\ShareCtrlScreen` and
  is under `$PROGRAMFILES64`/`$PROGRAMFILES`), logged via the same banner/ExecToLog style, and
  it must not abort the uninstall if a file is locked (best-effort delete, then report). Keep
  the existing service stop/remove ordering. Confirm the installer still lists the log for
  removal is NOT required — RMDir /r covers it.
* Research/cite: NSIS `RMDir /r` and the "do not blindly RMDir /r a user-supplied path" safety
  note; Tauri v2 installer-hooks POSTUNINSTALL timing. Header comment says WHY (runtime files
  the installer did not record).
* Tests/CI: the existing `windows-installer` job already asserts the folder is gone after
  uninstall — W7 is proven green when that job passes on push. Add, if cheap, an assertion
  that a file CREATED after install (touch a dummy in $INSTDIR) is also removed, so the
  RMDir /r path is exercised deterministically even if the service wrote no log yet. No Rust
  change expected; if none, no lane cargo gate beyond `fmt --check`/`clippy`/`test` sanity.
* Acceptance: the `windows-installer` CI job passes end to end on push (install → service
  RUNNING → upgrade new PID → uninstall → `sc` 1060 AND folder gone); on a real machine,
  uninstall leaves nothing under Program Files. Device line in plans/checklists/W4.md updated.

### U1 — In-app updater: check, download, install, restart  (size L, all OSes; owner request 2026-11-02, TOP priority)
* Problem: there is no updater. The app is Tauri v2 (`tauri = "2"`), no `tauri-plugin-updater`, no
  updater block in `tauri.conf.json`. The owner wants: an in-app "Check for updates" and, when one
  exists, a one-click Update that downloads, installs, **closes and restarts the app** so they can
  reconnect and never get stuck outside. Must work when the owner is mid-session (controlling side).
* Deliverable:
  (1) **Plugin + config:** add `tauri-plugin-updater` (and `tauri-plugin-process` for relaunch) at an
      exact version from a crate the reviewer can confirm; register in `main.rs`
      (`tauri::Builder … .plugin(tauri_plugin_updater::Builder::new().build())`). `tauri.conf.json`:
      `plugins.updater` with `endpoints` = the GitHub "latest" static file
      `https://github.com/yanivkalfa/shareCtrlScreen/releases/latest/download/latest.json`, `pubkey`
      = the updater public key (see 3), and `bundle.createUpdaterArtifacts = true`.
  (2) **Updater signing keypair (separate from code signing):** `native/scripts/updater-make-key.sh`
      (wraps `tauri signer generate`, writes private key + password to files, prints nothing secret) —
      the ORCHESTRATOR runs it blind like the macOS cert and sets repo secrets `TAURI_SIGNING_PRIVATE_KEY`
      and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`; the public key goes in `tauri.conf.json` (committed,
      public by design). Document in the card which secrets are needed so the orchestrator can create them.
  (3) **Release artifacts:** `release-assets.sh` build phase passes the signing env so the Tauri CLI emits
      the per-bundle `.sig` files alongside the installer/AppImage/.app(.tar.gz). The release workflow
      (`build-assets.yml`/`release.yml`) uploads the `.sig` files AND generates/uploads a `latest.json`
      (version, pub_date, notes, and per-target `{ url, signature }` pointing at the release assets).
      Windows updater target = the NSIS installer; macOS = the `.app.tar.gz`; Linux = the AppImage. Decide
      and document which bundles get updater artifacts (Windows nsis + macOS app + Linux appimage; .deb/.rpm
      are not self-updating — the UI says "download from releases" for those, no silent update).
  (4) **In-app UI + flow:** a Settings → About/Updates control and a non-blocking check at startup (and a
      manual "Check for updates" button). When an update is found: show version + notes and an "Update &
      restart" button. On click: `update.downloadAndInstall()` with progress, then restart. RESTART must
      actually reconnect the user: after install, relaunch the app (tauri-plugin-process `restart()` /
      updater's relaunch) so they land back in the app ready to reconnect. If a session is live, warn that
      updating ends the current session.
  (5) **SERVICE INTERACTION (Windows, critical):** on a service-installed host the exe is owned/launched by
      the Windows service, and W4's NSIS installer hooks already `--stop-service` in PREINSTALL and
      `--install-service` (start) in POSTINSTALL. So the updater running the NSIS installer silently will
      naturally stop the service, replace files, and restart it — the service then relaunches the host.
      The card must: detect a service-managed install (the host runs with `--service-host`, or
      `elevation::service::query` says the service owns sign-in) and, in that case, let the SERVICE bring
      the host back after the installer runs (do not also self-`relaunch`, which would double-launch or
      race the one-instance rule); on a non-service / portable install, the app relaunches itself. Verify
      the updater's silent NSIS run uses the same install dir and does not strip the service. State clearly
      what is proven by CI vs needs a device.
  (6) **Non-goals:** auto-apply without consent (always ask), delta updates, rollback. Just safe
      check → consent → download → install → restart.
* Research/cite (`//! ## References` + config comments): Tauri v2 "Updater" plugin + "Updater Artifacts"
  and signing (`https://v2.tauri.app/plugin/updater/`), `createUpdaterArtifacts`, the `latest.json`
  schema, `tauri signer generate`, tauri-plugin-process `restart`. The GitHub "releases/latest/download"
  static-URL pattern.
* Tests/CI: unit-test the version-compare / latest.json parsing where it is pure Rust or TS; a CI step that
  validates the generated `latest.json` schema and that `.sig` files exist for each updater target in the
  release run (dry-run path). Keep README (an "Updating" section) and docs/FIRST-RUN truthful.
* Acceptance (device I-level): from a published updater-enabled release, the running app detects a newer
  release, shows it, and on "Update & restart" downloads, installs, closes and reopens into a usable app;
  on a service install the service brings the host back and no duplicate instance results; a session that
  was live is cleanly ended with a warning. Note: full end-to-end needs TWO updater-enabled releases (this
  one to ship the mechanism, the next to update TO) — say so. Checklist `plans/checklists/U1.md`.
* **As built (branch `task/U1`, done + Opus-reviewed; Rust/TS lane-proven, end-to-end needs a published release):**
  * **Plugins + config:** `tauri-plugin-updater = "2.14.1"`, `tauri-plugin-process = "2.4.0"`
    (plans/06), registered in `app/src/main.rs` (`.plugin(tauri_plugin_updater::Builder::new().build())`,
    `.plugin(tauri_plugin_process::init())`). `tauri.conf.json` → `plugins.updater` with the
    `releases/latest/download/latest.json` endpoint, `windows.installMode = "passive"`, and a
    **placeholder `pubkey`** (`REPLACE_WITH_UPDATER_PUBLIC_KEY_FROM_native-scripts-updater-make-key-sh`)
    the orchestrator substitutes. `createUpdaterArtifacts` is in a separate overlay
    `tauri.updater.conf.json`, applied by `release-assets.sh` ONLY when `TAURI_SIGNING_PRIVATE_KEY`
    is set — so native-ci's unsigned installer build is untouched. Capabilities gain
    `updater:default` + `process:default`.
  * **Flow in Rust** (`app/src/updater.rs`, so the UI stays a thin invoke/listen shell): commands
    `updater_check` (version + notes, no download) and `updater_install` (ends the live session,
    `download_and_install` with `updater-progress` events, then the restart). UI: an "Updates"
    fieldset in Settings with the current version, "Check for updates", and, when one exists,
    notes + "Update & restart" (relabelled "End session & update" + an inline warning when a
    session is live). A non-blocking check runs at startup.
  * **Service-aware restart (W4):** `restart_plan(is_windows, service_managed)` →
    `ServiceRelaunches` | `InstallerRelaunches` | `SelfRelaunch` (unit-tested). `service_managed`
    is the `--service-host` launch flag or `elevation::service::query().owns_sign_in()`. On every
    Windows path the app does NOT self-relaunch (the installer, and on a service install the
    POSTINSTALL `--install-service` restart, bring the host back; W4's one-instance gate is the
    backstop against a double-launch); only macOS/Linux self-relaunch via `AppHandle::restart`.
  * **Release:** `release-assets.sh` layers the updater overlay and copies the per-bundle `.sig`
    files; macOS rebuilds + `tauri signer sign`s the `.app.tar.gz` from the **signed** app (so an
    update keeps the certificate signature). `release.yml` exposes `TAURI_SIGNING_PRIVATE_KEY` /
    `_PASSWORD` to the build (and, on macOS, package) step, then the checksums job runs
    `updater-latest-json.sh` (discovers `.sig` files → `windows-x86_64`, `darwin-universal`/`-aarch64`/`-x86_64`,
    `linux-x86_64`; .deb/.rpm are not updater targets) and `updater-check-latest-json.py`, and
    uploads `latest.json` with the release. `updater-make-key.sh` wraps `tauri signer generate`.
  * **Secrets the orchestrator creates** (then substitutes the public key in `tauri.conf.json`):
    `TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.
  * **Proven in the lane:** the Rust (plugins resolve, commands + `restart_plan` tests, clippy, fmt,
    cargo-deny licences) and the `latest.json` generator's target mapping. **Needs a published
    release:** the actual check/download/install/restart, the silent NSIS run through W4's hooks,
    and the service bringing the host back with no duplicate — and the full loop needs TWO
    updater-enabled releases (this one ships the mechanism, the next is what it updates TO).
  * **platform-api:** unchanged (no trait touched).

### U3 — Linux .deb/.rpm install can't self-update ("invalid updater binary format")  (size S, Linux; follow-on to U1, 2026-11-02)
* Problem (ROOT CAUSE, confirmed): on Linux the in-app updater fails with the raw Tauri error
  "invalid updater binary format". `tauri-plugin-updater` picks the installer from how the app was
  installed (the current bundle type) and validates the downloaded payload against it. Our feed
  (`latest.json` `linux-x86_64`, from `scripts/updater-latest-json.sh`) serves the AppImage only. So a
  `.deb`/`.rpm` install downloads an AppImage payload, the type does not match, and the plugin rejects
  it. AppImage installs self-update fine. There is no clean way to serve both a `.deb` and an AppImage
  under one `linux-x86_64` key, and a `.deb` self-update would need root write to `/usr` anyway
  (EACCES). Ref: tauri-apps/plugins-workspace#3108.
* Non-goals: serving `.deb`/`.rpm` updater payloads; any change to the Windows/macOS update flow or
  `RestartPlan`; changing the feed generator.
* **As built (branch `task/U3`, in review; Rust lane-proven on windows + wsl, UI + device-pending):**
  * **Install kind as pure data** (`app/src/updater.rs`): `update_capability(is_windows: bool,
    is_macos: bool, appimage: bool) -> UpdateCapability` where `UpdateCapability` is `SelfInstall` |
    `ManualDownload`. Windows and macOS are always `SelfInstall`; on Linux only an AppImage is
    (`is_windows || is_macos || appimage`). OS-call-free, so unit-tested. The `appimage` bool is
    decided at the call site by `running_from_appimage()` (Linux-only; always false off Linux),
    which delegates to the pure, testable `running_from_appimage_inner(appimage, appdir, exe)`.
    That helper mirrors the **containment rule** `platform-linux`'s autostart (`choose_exe`) uses —
    `$APPIMAGE` set AND `$APPDIR` set AND our own `current_exe()` under `$APPDIR` — because
    `$APPIMAGE` alone is inherited by child processes, so a `.deb`/`.rpm` launched from inside any
    AppImage would otherwise be taken for self-updatable, offer "Update & restart", tear the session
    down and then hit the very "invalid updater binary format" we are fixing. `current_update_capability()`
    ties it together; `install_should_proceed(capability)` is the one-line guard the command shares
    with its test.
  * **Surfaced in the check** (`UpdateInfo`, serde camelCase): new `self_updatable: bool`
    (`selfUpdatable`) and `download_url: String` (`downloadUrl` = `RELEASES_LATEST_URL`,
    `https://github.com/yanivkalfa/shareCtrlScreen/releases/latest`). `updater_check` still reports
    `available` by the version compare exactly as before — only the install affordance changes.
  * **Never installs on a packaged install:** `updater_install` early-returns `Ok(())` when
    `install_should_proceed(current_update_capability())` is false, before `end_session` /
    `download_and_install`, so the raw "invalid updater binary format" can never surface. Every
    Windows path and the macOS/Linux-AppImage `SelfRelaunch` are unchanged.
  * **Opener:** a new `open_releases_page` `#[tauri::command]` using the `open` crate
    (`open::that_detached`, does not wait on the browser), opening the fixed `RELEASES_LATEST_URL`.
    Chosen over `tauri-plugin-opener` because a custom app command needs no capability-JSON entry in
    Tauri v2 and keeps the all-`invoke` UI contract; `open` is MIT (already in the deny/about
    allow-lists), so `cargo deny check` stays green. Registered in `main.rs`'s `generate_handler!`.
  * **UI** (`app/ui/index.html` + `app.js`): the U1 Updates panel splits into `#update-selfinstall`
    (the existing version/notes/warning/"Update & restart" + progress) and `#update-manual` (a
    "Download update" button → `invoke('open_releases_page')` + the line "Your install is a .deb/.rpm
    package; download the latest from the releases page (or use the AppImage for automatic updates).").
    `renderUpdate` shows one or the other on `lastUpdate.selfUpdatable` (defaulting to self-install when
    absent). The available version + notes still show in both. When `selfUpdatable` is true the panel is
    byte-for-byte the U1 behaviour.
  * **Tests (unit-proven):** `update_capability` — linux+appimage → `SelfInstall`;
    linux+!appimage → `ManualDownload`; windows and macos → `SelfInstall` regardless of the appimage
    bool; `install_should_proceed` — false for `ManualDownload`, true for `SelfInstall` (the
    `updater_install` early-return guard, tested at unit level without a Tauri app); and
    `running_from_appimage_inner` — true only for $APPIMAGE+$APPDIR+exe-under-$APPDIR, false for
    inherited $APPIMAGE with no $APPDIR, false for $APPIMAGE+$APPDIR but the exe outside the mount
    (the inherited-env case), and false for none/empty. The UI toggle is device/manual.
  * **Device-pending** (checklists/U3.md): a real `.deb` Ubuntu install on a newer release shows the
    version + a working "Download update" link and never the raw error; an AppImage install still
    self-updates; Windows/macOS unchanged.
  * **platform-api:** unchanged (no trait touched). New dep: `open = "5"` (app only).

### W8 — Service-launched host: restore the fast screen-capture path (fix the input/video lag)  (size M, Windows; owner report 2026-11-02, host app.log attached)
* Problem: after the W4 installer started launching the host from the SYSTEM service, screen capture on
  the host degrades and the whole session feels very laggy (mouse + typing), because the user sees their
  actions through a slow, stuttering video. Host app.log (one live instance, `UIAccess: granted`, service
  log "launched engine pid=… in session 1 with UIAccess") shows the real cause:
    - `capture: DuplicateOutput1 failed (Access is denied. (0x80070005)); falling back`
    - `engine::session: host media loop ended: capture backend: Access is denied. (0x80070005)`
    - then HW H264 encoder fails `0x8000FFFF` → `no hardware H264 encoder — using the software encoder (CPU)`.
  So the Desktop Duplication (DXGI) path is denied for the service-launched process and the media loop
  ENDS and restarts, and encoding falls to CPU software — together that is the lag. Note the HW-encoder
  `0x8000FFFF` is machine A's known NVIDIA-MFT quirk (pre-existing, not caused by W4); the NEW regression
  is the capture Access-denied, which did not happen when the host was launched by the user directly.
* Root-cause hypotheses to confirm (the builder must verify against Microsoft docs and the code, not guess):
    1. A process launched by a LocalSystem service with `WTSQueryUserToken` + `CreateProcessAsUserW` on
       `winsta0\default` (elevation/session.rs) is not reliably associated with the active INPUT desktop,
       so `IDXGIOutput5::DuplicateOutput1` / `IDXGIOutput1::DuplicateOutput` returns E_ACCESSDENIED. A
       self-launched interactive host inherits the right desktop/station and succeeds.
    2. capture/src/lib.rs maps EVERY `E_ACCESSDENIED` to `Error::SecureDesktop` ("only the SYSTEM helper
       can duplicate it") and the media loop treats it as terminal and ENDS — so a transient denial during
       a desktop switch (UAC secure desktop up, session transition) kills capture instead of retrying.
* Deliverable:
    (1) Make the service-launched host attach to the active input desktop before duplicating: set the
        process/thread to the interactive desktop (e.g. `SetThreadDesktop(OpenInputDesktop(...))` on the
        capture thread, or launch with the correct `lpDesktop`/window station so DDA is permitted), so
        DuplicateOutput1 succeeds for the service host exactly as for a self-launched one. Research the
        exact requirement (Desktop Duplication API access rules; service-to-session launch + desktop).
    (2) Make capture resilient: distinguish a genuine secure-desktop/denied state from a TRANSIENT
        E_ACCESSDENIED (desktop switch), and RETRY/re-acquire the duplication with backoff instead of
        ending the host media loop. The loop must recover on its own when the input desktop returns.
    (3) Keep/confirm the HW→software encoder fallback (that path is correct); only note in the card that on
        machine A the HW MFT fails 0x8000FFFF and software is expected there — W8 does not fix that box's
        encoder, but with the fast capture path restored the software-encode lag should match the
        pre-service baseline. If feasible, log one line distinguishing "software encoder (no HW)" from
        "capture restarting" so a future lag report is unambiguous.
    (4) Add/adjust logging so the capture-desktop state is visible: which desktop the capture thread is on,
        and a single explanatory line when it re-acquires after a denial.
* Tests: unit-test the error-classification change (transient vs terminal) and any pure desktop-name logic;
  the real proof is on the owner's machine (checklist W8.md: install via the service, connect, confirm
  capture uses DuplicateOutput1 and the session is smooth, and that a UAC prompt / lock screen transition
  recovers capture instead of ending it).
* Acceptance (device): a service-launched host on the owner's machine captures via the fast DDA path (no
  repeating "Access is denied"/"media loop ended"), and input/typing feel as responsive as a self-launched
  host did; capture recovers after a secure-desktop transition. Checklist plans/checklists/W8.md.
* Risk/interaction: this touches elevation/session (how the service launches) and capture (desktop + error
  handling). Coordinate with W4's launch invariants — do NOT regress the elevated-window control that now
  works (UIAccess granted must stay). If a platform-api change is needed, STOP and report it.
* **Goal widened (owner, 2026-11-02):** the owner confirms the session ran "like a charm" self-launched on
  this exact machine BEFORE the service change. So the HW-encoder `0x8000FFFF` is NOT assumed to be a
  permanent box quirk — it is treated as the SAME service-context regression as the DXGI denial. Target:
  restore the FULL fast path (fast DXGI capture **and** hardware H264 encode) for the service host, to match
  the self-launched baseline. Root-cause them together: what a self-launched interactive host has that the
  service host lacks is the active-input-desktop association of its threads; the capture thread creates the
  **shared** D3D11 device that the HW MFT is then built on, so attaching that thread to the input desktop
  before the device is created is expected to fix BOTH. Only if HW encode still fails after that, on proof
  it is service-independent, is it documented as a machine-specific fallback.
* **As built (2026-11-02):**
    - (1) **Desktop attach, scoped to the capture thread.** `platform_windows::adapters::DdaCapture::new`
      now calls `attach_capture_desktop` (an `elevation::InputDesktopFollower` →
      `OpenInputDesktop`+`SetThreadDesktop`, the same primitive the input injector uses on ITS thread)
      BEFORE `capture::Duplicator::new`, so `DuplicateOutput1` is permitted for a service-launched host.
      `SetThreadDesktop` is per-thread, so the injector thread (UIAccess control) and the UI thread are
      untouched — W4/W5 invariants intact, and elevation/session.rs (the launch) is unchanged. Because the
      attach is sticky for the thread and `host_media_loop` builds the HW H264 encoder from `cap.device()`
      on that **same** thread right after, the shared D3D11 device + the MFT activation also happen in the
      input-desktop context — the expected fix for the `0x8000FFFF` HW-encode regression (device-pending —
      hypothesis; see W8.md §3).
    - (2) **Error classification + retry.** `capture::Error::SecureDesktop` (every `E_ACCESSDENIED` → terminal)
      is replaced by `capture::Error::AccessDenied` (recoverable), now also returned by the `duplicate_output`
      creation path (previously an opaque `Backend` error that ended the loop). `adapters::capture_err` maps
      both `AccessLost` and `AccessDenied` → `CaptureError::Lost`, the one error `host_media_loop` recovers
      from; `reinit` re-attaches to the (possibly new) input desktop. The initial `create_capture` is retried
      with capped backoff (`open_capture`) so a startup race / secure-desktop-up self-heals instead of ending
      the session. No `platform-api` signature change (reused `CaptureError::Lost`; `SecureDesktop` variant
      kept but no longer produced on Windows).
    - (3)/(4) **Logging.** `capture:` logs the input desktop it attached to; `host:` logs capture loss and a
      single re-acquire line, both worded "capture restart, not an encoder change" so a future lag report
      is unambiguous against the existing codec "software encoder (CPU)" line.
    - Lane-proven: `cargo fmt/check/clippy -D warnings/test` on Windows + WSL; unit test
      `adapters::access_denials_are_recoverable_backend_errors_are_terminal`. Device-only (W8.md): that a
      service install uses DuplicateOutput1 AND the hardware encoder (no "software encoder (CPU)" line), the
      session is smooth, and capture recovers across a UAC/lock transition.

### M13 — macOS: the toolbar drawer does not appear although the reveal fires  (size S–M, macOS; owner report + Mac app.log 2026-11-02)
* Problem: on the macOS viewer the top-edge drawer (the web UI toolbar with End Session / Disconnect) never
  comes down, although M12's reveal instrumentation proves the NATIVE side works. Mac app.log
  (`/Users/avrumy/Library/Logs/ShareCtrlScreen/app.log`, 2026-11-02, connected to a Linux host):
    - `platform_macos::view: video view: pointer events arrive (first move at y=526.1px)` — events DO reach the view.
    - `toolbar reveal: Reveal at y=-9.3px (scale 2, system cover 0px); view frame … height 894.0, layer frame …
      height 894.0` then `Hide at y=161.2px; view frame … height 934.0, layer frame … height 934.0`.
  So on Reveal BOTH the video view AND its layer shrink by 40 px (934→894) to open the strip, and Hide restores
  it. The native reveal + layer-frame sync (M12) are correct. Yet the user sees no drawer. => The 40 px strip the
  reveal opens is NOT showing the web toolbar underneath on macOS. This is the "layer does not reveal the webview"
  gap, macOS-specific; on Windows the same web drawer works.
* Likely cause (builder to confirm against the view hierarchy): the native video child view sits over wry's
  `WryWebViewParent`; when it shrinks 40 px from the top, the webview (which renders the toolbar) must be visible
  in that strip. It is not — candidates: the video view/layer is not actually the TOP-most in that strip (z-order),
  or its background is opaque where it shrank, or the webview's toolbar is positioned/clipped so nothing shows in
  the first 40 px on macOS, or the shrink happens on the wrong axis (macOS is bottom-left origin — the 40 px may be
  opening at the BOTTOM, not the top, so the drawer region is exposed where the toolbar isn't). The last is the
  strongest suspect: y is bottom-origin on macOS; confirm the strip opens at the visual TOP, not the bottom.
* Deliverable: on macOS, a reveal actually exposes the web toolbar at the visual top of the video. Whatever the
  cause: fix the axis/origin if the strip opens at the wrong edge; ensure the webview (toolbar) is drawn in the
  revealed strip (z-order / clear background there); make Hide restore it exactly. The End Session / Disconnect
  buttons must be clickable in the revealed strip. Keep the Windows/Linux reveal behaviour unchanged (M12/surface
  reveal.rs is shared — scope the change to the macOS view/layout only, or prove the shared change is safe on all).
* Secondary (same log, lower priority, fold in if cheap): spurious reveals at extreme y (`Reveal at y=-470px`,
  `y=-1025px`; `Hide at y=568px`) — the pointer-y maps to large out-of-range values when near/over the top edge, so
  the bar sometimes toggles on a flick far from the edge. Clamp/validate the mapped y (the `exited_through_top`
  band) so only a genuine top-edge hover toggles. Do not let this delay the main fix.
* Tests: pure reveal/axis math unit-tested in surface-wgpu/view where possible; the real proof is on a Mac
  (checklist M13.md: connect, hover the top edge, the drawer with End Session comes down and is clickable, moving
  away hides it; windowed and full-screen; the extreme-y flick no longer toggles spuriously).
* Acceptance (device, Mac viewer): the drawer appears on a top-edge hover and End Session works; app.log still
  shows the Reveal/Hide lines (unchanged) but now the user SEES the bar. Checklist plans/checklists/M13.md.
* Note: this is independent of the Mac→Linux video lag (that is a TURN-relay bandwidth problem, ~220 kbps, tracked
  separately — NOT this card).
* **As built (branch `task/M13`, in review; type-checked + clippy-clean for aarch64-apple-darwin, the workspace
  suite passes on Windows and WSL; device-pending, checklists/M13.md):**
  * Root cause (macOS-specific, in `platform-macos/src/view.rs::apply_layout`): the strip is removed from the
    VISUAL top, and M12 computed one frame — origin from `parent.isFlipped()` — and used it for BOTH the view's
    frame and the hosted layer's frame. But a view's frame lives in the parent VIEW's coordinate system while a
    hosted layer's frame lives in the parent LAYER's, and their vertical flips are independent: wry hosts the
    `WKWebView` in a content view that is unflipped (bottom-left) whose backing layer is geometry-flipped
    (top-left, how web content lays out). With the view's origin (y=0, bottom-left) the layer's 40px gap opened at
    the layer's maxY — the VISUAL BOTTOM — so the Metal video kept covering the toolbar at the top and only a
    useless strip opened at the bottom. The owner's app.log was consistent (both frames shrank 934→894) because
    only the *edge* the gap came off was wrong, not its size — which is why the reveal and the M12 layer-frame
    sync looked correct.
  * Fix: `apply_layout` now computes the layer's frame separately from the view's, taking its origin from the
    parent LAYER's own `isGeometryFlipped()` (objc2-quartz-core `CALayer::isGeometryFlipped`, safe in 0.3.2), so
    the gap is always at the visual top whatever the parent layer's flip. The view's frame still uses
    `parent.isFlipped()` (correct for views). A shared `top_inset_frame(flipped)` closure builds both. A no-op
    when the two flips agree, so it cannot regress a platform whose view and layer share a flip.
  * **Device fallback:** if the on-device log shows the parent layer's geometry-flip EQUALS the parent view's
    flip (the flips agree) and the drawer is STILL hidden, then this fix changed nothing and the cause is NOT the
    axis — it is elsewhere: z-order (the video view/layer is not top-most in the strip) or the `WKWebView` toolbar
    not filling the top 40 px. Look there, not at the axis, before another round.
  * **Shared `surface-wgpu/src/reveal.rs` is untouched** — Windows and Linux reveal exactly as before. The whole
    change is in the macOS view/layout. `contentsGravity` (pin the picture to the visual bottom so the crop is off
    the top) is unchanged.
  * Secondary (app.log spurious toggles `Reveal at y=-470/-1025px`): `mouseExited` now ignores a top exit mapped
    more than one bar height (`BAR_CSS_PX`) above the edge — a fast-flick coordinate artifact, not a genuine
    top-edge departure. Exits at or below the edge, and the shared `exited_through_top` flick handling, are
    unchanged; macOS-scoped (view.rs).
  * M12's reveal logging is intact (the once-per-show "pointer events arrive" line and the per-transition
    "toolbar reveal: …" line with the view and layer frames); the layer-sync line now also prints the parent
    layer's geometry-flip so the owner can confirm the branch taken.
  * platform-api: no change.
  * Cannot run on macOS in the lane (no AppKit): type-checked and clippy-clean for aarch64-apple-darwin; the real
    proof is the owner's Mac (checklists/M13.md).

### V1 — Stop the host overshooting a congested link (restore smooth video, keep it sharp)  (size M, all OSes; owner report 2026-11-02 "mouse stays behind", investigation ac7be488)
* Problem (ROOT CAUSE, high confidence, diagnosed read-only from two hosts' app.logs + git history):
  commit 78cd049 (2026-07-27 "video: fix the blur at its source") made the host NOT follow the
  bandwidth estimate down, creating a self-reinforcing loss loop that stutters on any constrained
  link (both Windows and Linux hosts show it; the Mac→Linux relay and the Windows software-encode
  are only amplifiers). Evidence:
    - `tuning.rs:10` MIN_BITRATE raised 400_000 → **2_500_000**, and `driver.rs:580`
      `next = bps.clamp(MIN_BITRATE, TARGET_BITRATE)` pins the encoder CBR target at 2.5 Mbps even
      when BWE reports 65 kbps (logs: `applied ~2500 while bwe 65`).
    - `tuning.rs:13` DESIRED_BITRATE raised 3_500_000 → **9_000_000** → over-aggressive str0m probe
      padding on a capacity-limited relay, itself a loss source.
    - `media/host.rs:240-251` re-introduced a hand-rolled source pacer draining against the RAW BWE
      estimate (`link`, floored 100 kbps in two places: `driver.rs:572`, `host.rs:240`); because
      egress is pinned to the current estimate, GCC/TWCC never sees the link could carry more, so a
      dipped estimate self-sustains (Linux sat ~220 kbps for minutes). At the 100 kbps floor
      `max_debt≈3125 B` vs a ~10 KB CBR frame → ~1 fps.
    - `media/host.rs:248` forced keyframes BYPASS the debt gate (`… && !force_key`) → an unpaced
      multi-second IDR burst into a ~65 kbps link → GCC spikes up then slashes down (the 65↔3778
      oscillation), loss → PLI (`driver.rs:540-547, 562-565`) → another forced keyframe → loop.
  The LAST coherent design was commit 352d56d (MIN_BITRATE 400 k, DESIRED 3.5 M, encoder follows BWE
  down, NO source pacer — str0m's own pacer metered egress). That era was smooth. 78cd049 traded
  smoothness for sharpness; this card restores smoothness WITHOUT going back to mushy full-res.
  Keep the one GOOD part of 78cd049: `fps_num = MAX_FPS = 30` (host.rs:117) is correct — do NOT revert it.
* Deliverable (the complete fix — smooth AND sharp):
  (1) **Let the encoder follow BWE down.** Lower `MIN_BITRATE` (tuning.rs:10) to ~500_000 (justify the
      exact value; 400 k was the proven-smooth 352d56d value) so the `driver.rs:580` clamp lets the
      target track BWE; lower `DESIRED_BITRATE` (tuning.rs:13) to ~`TARGET_BITRATE` (8 M) to stop the
      padding overshoot. Confirm the encoder-apply path and the `applied {N}`/`bwe {M}` log now TRACK.
  (2) **Spend the degradation on RESOLUTION, not a bitrate floor, so text stays sharp on a bad link.**
      Drive a BWE-derived scale into the encoder's `want`/`fit_within()` resolution computation
      (`media/host.rs:409`, and the live encoder-rebuild path at ~host.rs:185-219 that today reacts
      only to `view_size`): as `link`/BWE falls, step the encode resolution DOWN (a small ladder, e.g.
      full → 0.75 → 0.5 → 0.33, with hysteresis so it doesn't flap) and rebuild the encoder at the new
      size; step back up as BWE recovers. A 600 kbps stream becomes a sharp smaller image, not a mushy
      full-res one. Reuse the existing rebuild machinery; keep MAX_FPS=30 but allow fps to fall toward
      MIN_FPS=4 only as a last resort below the smallest resolution step.
  (3) **Do not let forced keyframes bypass the pacer / overshoot.** `media/host.rs:248`: let str0m pace
      the IDR packets (it already can) instead of dumping the whole IDR at once; with (1) the IDR is
      small anyway. Keep the 1/s force-key rate limit and PLI handling.
  (4) **Reconsider the raw-BWE source pacer.** Prefer leaning on str0m's own pacer (the 352d56d model)
      rather than the hand-rolled debt pacer; if the hand-rolled pacer is kept for frame-dropping, it
      must not pin egress so low that BWE can never probe upward (the self-sustain bug). Whatever the
      choice, a dipped estimate MUST be able to recover.
* Tests: the loopback harness (engine `--test loopback`) is the proof vehicle — add/extend a test that
  drives a constrained-then-recovering link and asserts the encoder target TRACKS BWE down and back up
  (not pinned at MIN_BITRATE) and that no unpaced keyframe burst is emitted; unit-test the pure
  BWE→resolution-ladder mapping (monotonic, hysteretic). Keep all existing loopback/codec tests green.
* Acceptance (device, the owner's real sessions): on a constrained link the host log shows
  `applied` TRACKING `bwe` downward (not pinned ~2500) and the `bwe` value settling instead of the
  65↔3700 oscillation, fps rising from ~1 to steady; the session feels smooth (no "mouse stays behind")
  and text is sharp via the smaller resolution, on BOTH a Windows and a Linux host. Checklist
  plans/checklists/V1.md. Quick one-knob pre-check the owner can eyeball first: with just MIN_BITRATE
  lowered, `applied` should already start tracking `bwe`.
* Risks: lowering MIN_BITRATE without the resolution ladder brings back the ORIGINAL blur complaint —
  (1) and (2) MUST ship together. Slightly slower ramp to 8 M on a fast link (acceptable). Resolution
  flapping if hysteresis is weak — tune it. No platform-api change expected; if one is needed, STOP and report.
* **As built (branch `task/V1`, Opus; lane-proven on Windows + WSL, device-pending per checklists/V1.md; NO platform-api change):**
  All four deliverables shipped together in the engine crate only.
  (1) `tuning.rs`: `MIN_BITRATE` 2_500_000 → **500_000** (just above the proven-smooth 352d56d 400 k;
      comfortably carries the smallest ladder step), `DESIRED_BITRATE` 9_000_000 → **8_000_000**
      (= `TARGET_BITRATE`, no probe padding above the ceiling). The driver's BWE handler was extracted
      to a pure `tuning::encoder_target(bwe) = bwe.clamp(MIN, TARGET)` + `driver::apply_bwe`, shared by
      the real and injected estimate; `applied` now TRACKS `bwe` down to the floor (loopback asserts it
      reaches ≤ 600 k at a 300 k link — never the old 2.5 M — and tracks 2 M and 6 M back up).
  (2) Resolution ladder in `tuning.rs` (`RESOLUTION_LADDER [1.0, 0.75, 0.5, 0.33]`, `resolution_step`
      with a down/up hysteresis band, `target_fps` which keeps 30 fps until the smallest step on a
      sub-floor link then scales to `MIN_FPS`). `media/host.rs` reads the RAW `link` estimate, scales the
      viewer-sized `want` through `fit_within`, and rebuilds the encoder via the existing resize machinery;
      fps drives the frame-rate cap. All three functions are pure and unit-tested (monotonic + hysteretic).
  (3) The `&& !force_key` pacer bypass is gone (the pacer is gone); forced IDRs ride str0m's pacer like any
      frame. The 1/s `MIN_KEYFRAME_GAP` rate limit + PLI handling are unchanged.
  (4) The hand-rolled debt source pacer is REMOVED (the 352d56d model: str0m's pacer meters egress). Both
      100-kbps `link` floors (`driver.rs`, `host.rs`) are removed so a dipped estimate can recover; the raw
      estimate now flows straight to the ladder.
  Tests: `tuning.rs` unit tests (clamp tracks + caps; ladder monotonic + hysteretic; fps floor + recovery).
  A new loopback test `constrained_link_tracks_bwe_down_and_recovers` drives a 300 k → 2 M → 6 M link via a
  `test-hooks`-gated BWE injector (`Engine::test_*`, cfg'd out of shipping builds) and asserts `applied`
  tracks down below the old floor and back up, the encode size steps down and recovers, and frames keep
  flowing with no link-trouble. Resolution is asserted from the host's configured encode size, not the
  viewer's pixels: the MOCK encoder passes the source frame's dimensions through unchanged (a real encoder
  resamples to the configured size), so the on-screen resolution drop is device-only (checklists/V1.md).
  The existing 7 loopback tests and the full workspace suite stay green on Windows and WSL.

### D10 — Viewer cursor feels laggy on a high-latency link: draw it at the LOCAL pointer, hide the OS cursor  (size M, all viewer OSes; owner report 2026-11-02)
* Problem (diagnosed with the owner from the controller screen): when controlling over a relayed/high-RTT
  link the pointer "lags behind". The controlled machine's own cursor moves smoothly (input arrives fine),
  but the controller shows TWO cursors with a visible gap = the latency made visible:
    1. the controller's own OS cursor, at the real mouse position (instant), NOT hidden over the video; and
    2. the client-side cursor SPRITE the viewer draws (render/src/lib.rs `draw_cursor`, surface-wgpu
       `cursor_rect`/`set_cursor_sprite`, D9 sprite) — drawn at the **host's out-of-band `PointerPosition`**
       (render/src/lib.rs:88 claims this "feels instant", true only on a LAN; on a TURN relay the host echo
       is a full round trip behind the real mouse).
  The capture is NOT the cause: every host backend already keeps the cursor OUT of the video and reports it
  out-of-band (capture/src/lib.rs "out-of-band cursor info", macOS `showsCursor=false`, Linux "Cursor never
  part of the picture"). So the video has no baked cursor; the lag is purely that the DRAWN sprite follows
  the host's delayed echo while the local OS cursor is instant.
* Deliverable (make the pointer feel instant, one cursor, correct host shape):
  (1) **Drive the drawn cursor from the LOCAL pointer when the viewer is controlling.** The viewer already
      knows the local pointer position (it is the one being SENT to the host as input). When this session has
      CONTROL and the local pointer is over the video, position the cursor sprite at that LOCAL position
      (instant) instead of the host's out-of-band `PointerPosition`. Keep using the host's out-of-band
      position when the viewer is VIEW-ONLY, or for host-initiated moves while the local pointer is not over
      the surface (so the host's own cursor movement is still reflected). The sprite SHAPE stays the host's
      (D9) — only the POSITION source changes. This is standard client-side cursor prediction (AnyDesk/RDP).
  (2) **Hide the controller's OS cursor while the local pointer is over the video surface** so only the drawn
      sprite shows (no duplicate). Per viewer OS: macOS `NSCursor::hide/unhide` (balance hide/unhide on
      enter/exit and focus loss so the OS cursor can never get stuck hidden); Windows (the `render` D3D11
      viewer) the tao/winit window cursor-visible API or `SetCursor(NULL)` within the client area; Linux the
      same winit/tao path. Restore the OS cursor the instant the pointer leaves the video or the window loses
      focus. If hiding the OS cursor is not reliable on a platform, fall back to keeping it but STILL draw the
      sprite at the local position (so at least the two cursors coincide and there is no visible lag gap).
  (3) Must not regress D9 (the real host cursor SHAPE still shows, I-beam in a terminal etc.), the fallback
      arrow when no shape, `visible==false` hiding, the reveal/toolbar, or input. HiDPI/letterbox mapping of
      the local pointer to the normalized video position must reuse the existing geometry (surface-wgpu
      `geometry.rs` pointer→normalized map; render viewport mapping) so the sprite lands exactly under the
      real mouse.
* Non-goals: changing the host capture (cursor is already out-of-band); input/injection latency (fine);
  video-frame latency.
* Tests: unit-test the "which position source" selection (control+over-surface → local; view-only or
  pointer-not-over → host out-of-band) and the local-pointer→normalized mapping (reuse/extend the geometry
  tests). The OS-cursor hide/restore is platform UI — device-verified; add a checklist. Keep all existing
  render/surface-wgpu/loopback tests green.
* Acceptance (device, the owner's real relayed session): controlling over the relay, the pointer feels
  immediate — ONE cursor that tracks the real mouse with no trailing second cursor and no visible lag gap,
  with the correct host shape; view-only still shows the host's cursor moving; the OS cursor returns when the
  pointer leaves the video; nothing stuck. Checklist plans/checklists/D10.md. This is the fix for the owner's
  "mouse lags behind" even though input and video are fine.
* **As built (branch `task/D10`, in review; Windows + WSL lane green, platform-macos type-checked for
  aarch64-apple-darwin; no real relayed session yet — I1–I3):**
  * **platform-api (orchestrator-approved, additive + defaulted, object-safe)** — two new
    `VideoSurface` methods in `platform-api/src/surface.rs`:
    * `fn local_pointer(&self) -> Option<(f64, f64)> { None }` — the viewer's LOCAL pointer over the
      video, normalized `[0,1]` over the letterbox, or `None` when it is not over the surface.
    * `fn set_cursor_control(&self, _active: bool) {}` — tells the surface whether this viewer controls, so
      it hides the OS cursor over the video and restores it on leave/focus-loss.
    Both default (so `MockSurface` and every backend compile unchanged); placed on `VideoSurface`, not
    `Presenter`, because OS-cursor visibility is a window/view concern (the card's "presenter" wording was
    loose). **Only the POSITION source changes; the D9 SHAPE stays the host's.**
  * **Position switch (engine, all OSes)** — `engine/src/media/viewer.rs` `cursor_position(control, local,
    host)`: when controlling AND the local pointer is over the video AND the host still shows a cursor, the
    sprite is drawn at the LOCAL pointer (instant, client-side prediction à la AnyDesk/RDP); otherwise the
    host's out-of-band position (view-only, or a host-initiated move while the pointer is elsewhere). The
    host position is still the visibility-filtered one, so D9's `visible == false` hide is preserved (draws
    nothing). Reuses the existing pointer→normalized geometry, so the sprite lands exactly under the real
    mouse (HiDPI/letterbox correct). 4 unit tests.
  * **Control state (engine)** — `Shared.control: AtomicBool`, seeded from the granted `Permission` in
    `start_viewer`, and the viewer now handles inbound `ControlMsg::Perm` (previously ignored on the viewer):
    a mid-session grant/revoke updates `shared.control` and calls `surface.set_cursor_control(..)`, so
    view-only reverts to the host position and restores the OS cursor. Teardown calls
    `set_cursor_control(false)`.
  * **Shared surface state** — `surface-wgpu/src/state.rs`: `pointer_inside` + last normalized position +
    `cursor_control` atomics; `local_pointer()` returns the position only while inside (focus intentionally
    NOT required for the sprite — an unfocused hover shows the OS cursor, and the coinciding sprite merely
    overlaps it). Used by macOS and Linux. 1 unit test.
  * **Per-OS OS-cursor hide:**
    * **Windows** (`render/src/window.rs`, D3D11 viewer): `WM_SETCURSOR` → `SetCursor(None)` while
      controlling and the hit-test is `HTCLIENT`; pointer-driven so it is self-balancing (the toolbar strip
      belongs to the web view and keeps its own cursor). `WM_MOUSEMOVE` + `TrackMouseEvent(TME_LEAVE)` /
      `WM_MOUSELEAVE` (in `Win32::UI::Controls`, so `Win32_UI_Controls` was added to the crate features)
      track inside for `local_pointer`; `hide()` resets the flags. `ChildWindowSurface` delegates both
      methods.
    * **macOS** (`platform-macos/src/view.rs`, `surface.rs`): `NSCursor::hide`/`unhide`, balanced by a
      `Cell<bool>` so exactly one hide per unhide — it can never stick. Desired-hidden = control && pointer
      inside && key focus, re-evaluated on enter/leave/move/drag/button and on **every** focus change
      (`publish_key_focus`, `windowDidResignKey`, `resignFirstResponder`, `viewDidHide`) and on
      `reset_reveal`; `Drop` force-restores. Cmd+Tab away → `windowDidResignKey` → unhide; back →
      `windowDidBecomeKey` → re-hide.
    * **Linux X11** (`platform-linux/src/surface/x11.rs`): a transparent cursor (1×1 depth-1 pixmap cleared
      to 0, source+mask) kept beside the arrow; `set_cursor_control` swaps the window's cursor attribute —
      a per-window attribute, inherently balanced, shown only while the pointer is over our video, restored
      to the arrow on release and both freed in `Drop`. **Caveat:** it hides whenever controlling regardless
      of key focus (the attribute is window-scoped), so an unfocused-but-hovered window shows no cursor
      until control ends — minor, in the checklist.
    * **Linux Wayland** (`platform-linux/src/surface/wayland/`): `local_pointer` is fed (instant sprite) but
      `set_cursor_control` stays the default no-op — there is no portable GDK/`wl_pointer` cursor-hide here,
      so the sprite simply coincides with the OS cursor (the card's allowed fallback; no lag gap).
  * **Mock** — `platform-api/src/mock.rs`: `pointer_inside`/`cursor_control` fields + `set_pointer_inside`
    helper + the two overrides, so the selection is unit-testable.
  * **Tests:** unit — the position-source selection (`cursor_position`: control+over→local, view-only→host,
    not-over→host, host-invisible→None) and `SurfaceState::local_pointer` gating. Device-only (I1–I3,
    checklists/D10.md) — the real relayed "one instant cursor" feel and the OS-cursor hide/restore on each
    OS. All existing render/surface-wgpu/loopback/engine tests stay green (Windows + WSL lane).
  * **Not changed:** host capture (cursor already out-of-band), input/injection, the D9 shape path, the
    fallback arrow, `visible == false` hiding, the reveal/toolbar.
  * **References** (in the touched `//! ## References`): NSCursor `hide`/`unhide`
    (<https://developer.apple.com/documentation/appkit/nscursor>); Windows `SetCursor(NULL)` +
    `TrackMouseEvent`/`WM_MOUSELEAVE`; winit/tao `set_cursor_visible`
    (<https://docs.rs/winit/latest/winit/window/struct.Window.html#method.set_cursor_visible>); X11
    transparent cursor via an empty pixmap; client-side cursor prediction (AnyDesk/Microsoft RDP) as the
    rationale for drawing at the local pointer.

### D11 — Controller cursor freezes between video frames (decouple the drawn cursor from frame arrival)  (size M, all viewer OSes; owner report after D10)
* Problem (root cause, confirmed): the viewer repainted the drawn cursor sprite ONLY when a new video
  frame was presented — the present lived solely inside the decoded-frame branch, and the
  `recv_timeout(100 ms)` idle branch did nothing. So on a static host screen (few/no frames) or a starved
  link the LOCAL pointer kept moving but the sprite was not repainted: it froze and snapped forward when the
  next frame arrived. D10 made it visible (the sprite follows the local pointer and the OS cursor is hidden,
  so the gap shows as a dead cursor).
* Deliverable: repaint the cursor independently of new frames (hold the last frame, re-present it with the
  current cursor on an idle tick when the cursor moved); fix `cursor_position` so a controlling viewer with
  the pointer inside never loses the sprite on a momentary host-cursor blip; harden Windows `pointer_inside`
  against sticking `false` after a focus change / control toggle. No platform-api change.
* **As built (branch `task/D11`, in review; Windows + WSL lane green incl. loopback×3 with `test-hooks`,
  platform-macos type-checked for aarch64-apple-darwin; device-pending checklists/D11.md; NO platform-api
  change — the `Presenter`/`VideoDecoder`/`VideoSurface` traits are untouched):**
  * **Idle re-present (engine, all OSes)** — `engine/src/media/viewer.rs`. The present loop now holds the
    newest decoded frame as `last_frame: Option<Frame<'_>>`. The decoded `Frame` borrows the decoder's
    current surface (valid only until the next `decode`), so `last_frame = None` is set BEFORE the decode
    path re-borrows the decoder; the held frame is therefore dropped exactly when the decoder is about to
    overwrite it (no leak, no race — single-threaded loop, the decoder keeps one extra texture alive at
    most). To store the newest frame without the borrow checker tying the loan across a re-borrow, the
    batch decode is split: a `for _ in 1..count` reference-only loop (decode to carry references, present
    nothing), then a single newest-frame block that presents and stores it.
  * **Cadence** — `recv_timeout` shortened from 100 ms to `IDLE_REPAINT_INTERVAL = 16 ms` (~60 Hz). When
    frames flow, `recv_timeout` returns at once on the queued data, so this adds NO video latency; only a
    static/starved stream reaches the timeout branch. The timeout branch re-presents `last_frame` with the
    CURRENT cursor ONLY when it differs from `last_cursor` (the position of the last present) — so a truly
    idle screen with a stationary pointer does zero extra GPU work, and a moving pointer tracks at ~60 Hz.
  * **Reuse** — both the decoded-frame path and the idle path go through one `present_frame(..)` helper
    (sprite `sync` + `present` + the non-silent render-error logging) and one `current_cursor(..)` helper
    (D10 position selection + host shape id), so the sprite/size/error behaviour is identical on both. The
    catch-up/backlog/keyframe-watchdog logic is unchanged (the no-picture watchdog is now the shared
    `on_no_picture(..)`; decode heartbeat logging is `note_decoded(..)`).
  * **`cursor_position` blip fix (engine, all OSes)** — previously `(control=true, local=Some, host=None)`
    returned `None`, so the sprite VANISHED whenever the host's out-of-band cursor report momentarily
    blipped to absent (a dropped unreliable-channel update, or the host hiding its own cursor) while
    controlling with the pointer inside. Now: controlling AND `local.is_some()` → ALWAYS the local pointer;
    otherwise the host position (view-only, or controlling with the pointer not over the surface). On the
    host path the position is still the visibility-filtered one, so an invisible host cursor still draws
    nothing in view-only. Unit tests: the new fixed quadrant
    (`controlling_over_the_video_survives_a_host_cursor_blip`) plus the reworked view-only invisible case;
    the other three D10 quadrant tests stay green.
  * **Windows `pointer_inside` hardening** — `render/src/window.rs`. `POINTER_INSIDE` is driven by
    `WM_MOUSEMOVE`/`WM_MOUSELEAVE`, so after a focus change or control toggle with no mouse move since it
    could stick `false` and drop the sprite to the stale host position. New `reevaluate_pointer_inside(hwnd)`
    (`GetCursorPos` → `ScreenToClient` → `GetClientRect`, honouring the revealed-toolbar clip) sets
    `POINTER_INSIDE = true` and refreshes `LAST_POS` + the `TME_LEAVE` request when the OS pointer is
    physically over the client area. Called from `WM_SETFOCUS` (focus regain) and from the `WM_REFRESH_CURSOR`
    handler when `CURSOR_CONTROL` just turned on. It only ever SETS inside — clearing stays `WM_MOUSELEAVE`'s
    job — so it cannot leave the OS cursor stuck hidden.
  * **No regression on macOS/Linux** — the idle re-present is in the shared engine loop; those presenters
    re-present `last_frame` (whose backing surface the decoder still owns, since the held frame keeps it
    alive) exactly as the decoded path does. macOS/Linux cursor-hide behaviour is untouched.
  * **Not changed:** platform-api traits, host capture/input/injection, the D9 shape path, the catch-up /
    backlog / size-report logic, the reveal/toolbar.
* Acceptance (device, checklists/D11.md): controlling over the relay with the host screen static, the cursor
  tracks the local pointer smoothly with NO multi-second freeze; correct host shape; view-only still shows the
  host cursor; nothing vanishes when the host cursor report blips.

### U2 — Settings/Update is unreachable while hosting a session  (size XS–S, UI; owner report 2026-11-02)
* Problem: the Updates control (U1) lives in the Settings modal, opened by the gear button. But the gear
  exists only on `screen-home` (`btn-settings`, index.html:25) and on the viewer screen (`btn-view-settings`,
  index.html:104). The HOST sharing screen `screen-host` (index.html:62, "Sharing screen with …", the
  Permission dropdown + End session) has NO gear — so while this machine is hosting/being-controlled there is
  no way to reach Settings → Updates without ending the session. The owner could not find the Update button
  for exactly this reason (they were on the host sharing screen).
* Deliverable: add a Settings gear to `screen-host`'s header (mirroring `btn-view-settings` on `screen-view`:
  same ⚙ glyph, `btn-ghost btn-sm`, `title="Settings"`), wired in app.js to the SAME `openSettings()` the
  other two gears use (add the id to the click-binding that currently binds btn-settings / btn-view-settings).
  `openSettings()` already hides the native video surface so the modal paints above it and restores it on
  close (app.js ~229/234) — confirm that path works from the host screen too (the host also has a native
  video child window). The Updates section, "Check for updates", and the service-aware "Update & restart"
  (U1) then work from an active host session; a live-session update still shows the U1 "this ends the
  session" warning.
* Non-goals: the controller-triggers-host remote update (that is a separate, larger feature — note it as a
  follow-up "remote update", the owner asked for it earlier); any change to the updater logic itself (U1).
* Tests: no Rust logic to unit-test; the change is index.html + app.js. Lane `cargo fmt --all --check` +
  workspace check/clippy/test must stay green (no Rust touched, but run it to confirm the tree is clean).
  Device/manual: from an active host session, the gear opens Settings, Updates is reachable, Check for
  updates works, and closing Settings restores the video.
* Acceptance: while hosting, the gear on the sharing screen opens Settings and the Update flow is reachable
  without ending the session. Checklist note in plans/checklists (or fold into U1.md).
* **As built (branch `task/U2`, in review; UI-only — no Rust touched, tree confirmed clean in the lane):**
  * **HTML:** `native/app/ui/index.html` — added `<button id="btn-host-settings" class="btn btn-ghost btn-sm"
    title="Settings">&#9881;</button>` to `screen-host`'s `topbar`, right after `#host-status` (the same
    end-of-header position `btn-view-settings` occupies in the viewer bar). Same glyph/classes/title as the
    viewer gear.
  * **JS:** `native/app/ui/app.js` — the viewer gear's inline click handler (hide the native video, then
    `openSettings()`) is now a named `openSettingsOverVideo()`; **both** `btn-view-settings` and
    `btn-host-settings` bind to it (no duplicated logic, `openSettings()` itself unchanged). `closeSettings()`'s
    restore was gated on `state === 'VIEW_ACTIVE'`; it now uses the existing `sessionLive()` helper
    (`HOST_ACTIVE || VIEW_ACTIVE`) so the video is restored after Settings closes from either session screen.
  * **Host video is a no-op, not a bug:** `engine::set_video_visible` (crates/engine/src/lib.rs:822) acts only
    on the viewer's receive `surface()`; the host has no such surface, so hide/restore are harmless no-ops on
    the host screen and the host webview has no native overlay to obscure the modal. The hide-then-open and
    restore paths are now generalised anyway (consistency + future-proofing).
  * **Updater (U1) untouched**; a mid-session update from the host still shows U1's "End session & update"
    warning. No `platform-api` / Rust change. Device note: `plans/checklists/U1.md` (U2 section).
  * Follow-up (owner asked earlier, out of scope here): controller-triggers-host "remote update".

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
  locally — delivered as Meta+Tab by M6 and translated to Alt+Tab by **D8** (owner decision 2026-11-01; before D8 Cmd arrives as the
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
* As built: proven locally against the PipeWire 0.3.48 headers (Ubuntu 22.04); CI proof is
  `bundle / linux` on the next push. Inline SPA helpers reimplemented, `flags` gated (task
  L1-fix). The breakage was in pipewire-rs's `libspa`
  0.10.1 itself, not in our code. Every upstream release uses `spa_meta_first` and
  `spa_meta_region_is_valid`, which are macros before PipeWire 0.3.59 (commits 90c955c2,
  671a7102), so bindgen has nothing to bind. It also uses `spa_video_info_raw.flags` and
  the `uint64_t` modifier, both from 0.3.65 (commit c61b9a09). Upstream effectively needs
  0.3.65. The fix vendors libspa 0.10.1 at `native/vendor/libspa` (MIT, `[patch.crates-io]`,
  excluded from the workspace) with three patches. The helpers are rewritten in Rust; the
  damage bounds check is one region wide. `VideoInfoRaw::new` is built without naming
  `flags`, the `flags` accessors sit behind the crate's `v0_3_65` feature, and the modifier
  is cast for either signedness. `stream.rs` tests cover damage iteration and the
  raw-format parse. Local proof: the jammy 0.3.48 `-dev` headers through the lane
  (L1-fix-01).

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

### D8 — Modifier translation per host OS  (size S–M, any OS; owner decision 2026-11-01, closes M6's blocked acceptance line)
* Problem: the viewer forwards keys literally. Cmd is `Meta` in the shared keymap, so a Mac
  viewer's Cmd+Tab reaches a Windows host as Win+Tab (Task View) and Cmd+C as Win+C; a
  Windows/Linux viewer's Ctrl+C reaches a Mac host as Ctrl+C, which is not Copy there.
  AnyDesk and Microsoft Remote Desktop translate by default; users expect it.
* Deliverable: a pure translation layer on the **viewer** side of the engine (the host stays
  dumb), applied to every forwarded key event (normal keys and M6/L7/W-lane captured
  shortcuts alike) when the viewer OS and the host OS differ:
  * Mac viewer → Windows/Linux host: Cmd ↔ Ctrl swapped (`MetaLeft/Right` ↔ `ControlLeft/Right`);
    Cmd+Tab (and Cmd+Shift+Tab) → Alt+Tab (Alt+Shift+Tab); Cmd+\` → Alt+\`; Option stays Alt.
  * Windows/Linux viewer → Mac host: Ctrl ↔ Cmd swapped; Alt+Tab → Cmd+Tab; the Win/Super
    key stays Meta (= Cmd) only when it is not already produced by the swap — document the
    choice.
  * Same-OS pairs and Linux ↔ Windows: no translation.
  * A viewer setting "Translate modifiers for the remote OS" (default **on**) in Settings →
    Advanced next to the existing toggles (`native/app/ui`), persisted like the other
    settings, read by the engine per session; when off, keys are literal (today's behaviour).
  * Modifier **state** must stay consistent across the swap: a Cmd pressed before the toggle
    flips or before the host OS is known must be released as the same code it was sent as
    (track sent codes, not pressed codes); `release_all`/focus-loss release what was sent.
  * The host OS must be known to the viewer before the first key: use the platform info
    already exchanged (D4 `get_platform_info` / the signaling hello); if the protocol does not
    carry it yet, add one additive field (`protocol` crate, backwards-compatible: absent =
    unknown = no translation) and document it in `plans/04-TARGET-ARCHITECTURE.md`.
  * Loopback test (D1 harness, mocks) proves: Mac→Windows Cmd+C arrives as Ctrl+C and
    Cmd+Tab as Alt+Tab; Windows→Mac Ctrl+C arrives as Cmd+C; toggle off = literal; a key
    held across a toggle flip is released as sent; same-OS pairs untouched. Unit tests for
    the pure mapping table.
* Acceptance (CI, loopback): the tests above green on all three OS legs. Device (I2): a Mac
  viewer's Cmd+Tab switches apps on the Windows host; Cmd+C/Cmd+V copy/paste there; the
  toggle off restores literal keys.
* Owner decision record: option 1 of three (1 = this task; 2 = amend M6 to "delivered as
  Meta+Tab"; 3 = 2 now and 1 later). M6's acceptance line now reads "delivered as Meta+Tab;
  translated to Alt+Tab by D8".
* **As built (done, Opus-reviewed; I2 device pass pending):**
  * `engine/src/modifiers.rs`: `OsFamily` + a pure mapping keyed by (viewer OS, host OS,
    setting) + `KeyTranslator`. One translator per viewer session, behind a mutex, shared
    by the surface input thread and the shortcut-hook forwarder, so M6/L7/W-lane captured
    keys and normal keys go through the same held-key state. It emits while holding the
    lock, so the host gets events in the order the translator decided them.
  * Mapping: both directions swap `MetaLeft/Right` ↔ `ControlLeft/Right`. Mac → PC chords:
    Cmd+Tab and Cmd+\` → **AltLeft**+Tab / +\` for either Cmd key (a right Alt is AltGr
    on many layouts, and AltGr+Tab does not switch apps). PC → Mac: Alt+Tab → Cmd(Left)+Tab.
    Shift passes through, so Cmd+Shift+Tab → Alt+Shift+Tab. **Decision, Win/Super → Mac
    Control:** Ctrl already produces Cmd. Leaving Win as Meta would give two keys for Cmd
    and none for Control (Terminal Ctrl+C, Ctrl+arrows for Spaces). **Addition beyond the
    card:** Ctrl+Tab is kept as Ctrl+Tab in both directions. The bare swap would make it
    Win+Tab (Task View) / Cmd+Tab (app switcher), the exact problem this card fixes.
    Same-OS pairs, Linux ↔ Windows, an unknown host OS, and the setting off are identity.
  * State: each held physical key remembers the code the host received. Key-up releases
    that code, auto-repeat repeats it, and a toggle flip affects only new presses. A chord
    rewrite turns a held modifier into another code **make-before-break** (new down, old
    up). WHY: a lone Win press and release opens the Start menu, and a Ctrl inside it
    suppresses that (AutoHotkey `#MenuMaskKey`). The rewritten modifier stays rewritten
    until it is released, so Cmd held across several Tabs keeps the Alt+Tab switcher open.
    Two physical keys sharing a sent code (Option held while Cmd+Tab makes Cmd an Alt)
    produce one press and one release on the host. An up the translator never saw pressed
    goes out under the current mapping, unless a held key owns that code. Host
    `release_all` and the surface's focus-loss key-ups therefore release exactly what was
    sent. Cases covered by unit tests: Tab up before Cmd, Cmd up before Tab, Cmd across
    several Tabs, Shift added mid-chord, Tab before Cmd (no chord), Option + Cmd+Tab, flips.
  * Host OS: `PlatformInfo` did not reach the viewer, so `SignalMsg::ConnectResponse`
    gained the additive `os: Option<String>` (`"windows"|"macos"|"linux"`, from the host's
    `PlatformInfo::name`; absent or unknown = identity). It arrives before the host's offer,
    so it is always known before the first key. Documented in plans/04 §6.
  * Setting `translateModifiers` (`Config::translate_modifiers`, default true; older
    files keep the default). Checkbox in Settings → Advanced, persisted like the others
    (the `save_settings` command → `Engine::update_config`), which also publishes it to an `AtomicBool`
    that the running session reads on every key, so a change applies to the next key.
  * Paste chord: unchanged. It is already host-side (`InputInjector::paste_chord`; macOS
    overrides to Cmd+V) and does not go through the translator.
  * Mock: `MockPlatform.os_name` (default `"mock"` = unknown). Loopback tests
    (`engine/tests/loopback.rs`) cover: Mac → Windows Cmd+C = Ctrl+C and Cmd+Tab = Alt+Tab,
    toggle off = literal, Cmd held across a flip released as the Ctrl it was sent as,
    nothing left held; Windows → Mac Ctrl+C = Cmd+C and Alt+Tab = Cmd+Tab; Mac → Mac and
    Linux → Windows literal. No platform-api trait change.
  * **Not verified (I2 device pass):** a Mac viewer on the Windows host. Cmd+Tab and
    Cmd+Shift+Tab switch apps (needs M6 shortcut capture on). Cmd+C/Cmd+V copy and paste
    in Notepad. Ctrl+Tab changes browser tabs. Toggle off mid-session = literal (Cmd+Tab
    opens Task View), with no stuck keys afterwards. A lone Control tap on the Mac viewer
    opens Start on the Windows host (expected: Mac Control is the Win key; on a Linux GNOME
    host it opens Activities), and Control+click arrives as Win+click (no right-click
    mapping). Reverse direction: Windows Ctrl+C copies on the Mac host, Alt+Tab switches
    apps there.
  * Risks: on a Mac viewer, Cmd comes through the surface path and Tab through the M6 tap.
    Their relative order is M6's guarantee (it forwards Cmd itself when the surface has
    not). If Tab ever reached the translator first, the host would get Tab, then Ctrl.
    (Closed by M12: hook keys now join the surface's FIFO channel, and the tap forwards
    every modifier the surface has not sent yet.)
    A Mac user's habits around Control change meaning on a PC host: a lone Control tap is a
    Win/Super tap (Start menu, GNOME Activities), and Control+click is Win+click, not the
    macOS secondary click (nothing maps it to a right-click).

### M12 — macOS: take every Cmd shortcut while the video has the keyboard  (size S–M; owner feedback 2026-11-01 from a Mac viewer on an Ubuntu host)
* Live-test observation 2026-11-02 (2.1.3, Mac viewer → Ubuntu host, windowed): the toolbar never appears on hover, but after resizing the window to about half and then entering full screen it shows for a moment with no pointer near the top. Reading: the reveal state flips on layout changes (`apply_layout`) while `mouseMoved` never reaches the view — hypothesis 1 of §7. Confirm from the tester's `app.log` ("pointer events arrive" line absent) before the fix task.
* Problem: M6's tap takes a fixed set (Cmd+Tab/`/Space, Ctrl+arrows). Everything else with Cmd
  goes through the app's responder chain first (`performKeyEquivalent`: menu bar, WebKit's
  own Cmd+C/V/W/Q…), so most Cmd combinations never reach the host at all ("Cmd key does
  not work" in the live test), and the ones macOS itself owns (Cmd+Space variants, Cmd+Tab)
  were the only ones working. The owner wants ALL macOS global shortcuts forwarded.
* Deliverable: while our window is key AND the video view has the keyboard (same focus
  rule as M6), the session CGEventTap takes **every key-down/up carrying Cmd** (any other
  modifiers too) and every M6 combination, and forwards the DOM codes to the host via the
  existing shortcut-hook channel; nothing Cmd-based reaches the menu bar or WebKit while
  captured. Exceptions, documented on the card and in the M6/M11 checklists: (1) shortcuts
  macOS never delivers to a tap at all (Cmd+Ctrl+Q lock screen, Cmd+Opt+Esc force quit,
  Touch ID / power): list them as "cannot be taken"; (2) **Cmd+Q stays local** as the one
  escape hatch (the user can always quit; document it in README's macOS passthrough row);
  (3) the sentinel/own-injection rule and `kCGEventTapDisabledByTimeout` re-enable stay.
  Clicking any other window, or the video losing the keyboard, releases everything held
  (release_all through the same path M6 uses) and restores local behaviour instantly.
  Plain key-ups for keys pressed before capture started are forwarded, not swallowed (the
  M6 "already held" rule). A setting is NOT added: this is the behaviour the owner wants.
* The D8 translation still applies downstream (Mac→Windows/Linux swaps Cmd↔Ctrl by default;
  the toggle in Settings → Advanced makes Cmd arrive literally as Super/Win). The card must
  say so and the README macOS row must explain the toggle in one sentence.
* Tests: the pure filter (`shortcut_filter.rs`) gets table tests for: any Cmd+key taken,
  Cmd+Q not taken, M6 set still taken, Ctrl+arrows taken, plain keys untouched, modifier
  key-up for a Cmd pressed before capture forwarded, reserved combos listed. `cargo check`
  and clippy for aarch64-apple-darwin in the lane; the live check is I2.
* Acceptance (I2, real Mac → Ubuntu/Windows): Cmd+Enter, Cmd+Shift+4, Cmd+Opt+D, Cmd+C/V
  arrive on the host (translated or literal per the toggle); Cmd+Q still quits the viewer;
  clicking the desktop restores local shortcuts; nothing stuck.
* **As built (branch `task/M12`, in review; type-checked for aarch64-apple-darwin, the pure
  filter tests run on every OS; device-pending, checklists/M6.md §6):**
  * What is taken (`platform-macos/src/shortcut_filter.rs`, pure): every key-down carrying
    Command, with any other modifiers, plus Ctrl+arrows (with any other modifiers), while
    the video view has the keyboard (M6's focus rule, unchanged). WHY a tap for all of them:
    AppKit offers a Command chord to `performKeyEquivalent:` down the key window's views
    (WKWebView takes Cmd+C/V/A/Z) and then to the menu bar before `keyDown:`, and the Dock,
    Spotlight, Mission Control and the screenshot service act on theirs before the app sees
    them (Cocoa Event Handling Guide, "The Path of Key Events"); only a session tap is ahead
    of all of them.
  * **Cmd+Q stays local**: Command alone + the key the layout types as "q" (AppKit matches
    menu key equivalents by character; physical ANSI Q when the layout types no Latin letter
    there). The tap passes it, and the video view's key monitor (`view.rs`), which used to
    send Cmd+Q to the remote, now lets it through to the menu bar's Quit (Tauri's default
    macOS menu) — with or without shortcut capture. Cmd+Shift+Q and Cmd+Option+Q go to the
    remote.
  * **Cannot be taken** (listed in `NEVER_TAKEN`, never claimed even if delivered): Force
    Quit Cmd+Option+Esc (also with Shift), Lock Screen Cmd+Ctrl+Q. Never delivered to a
    tap at all: Touch ID / power, media and brightness keys (system-defined events below
    the tap's mask; "Lower levels of the system trap and handle some events early"), and
    every key while Secure Input is on. The video view's key monitor lets Cmd+Q and the
    `NEVER_TAKEN` chords through to macOS too (`shortcut_filter::stays_local`, tested;
    review round 1), so neither path sends them to the remote. Screenshots (Cmd+Shift+3/4/5) are now taken — M6
    left them local; the M12 acceptance wants Cmd+Shift+4 on the host.
  * Sentinel (own injected events, `Key::own`) and the `kCGEventTapDisabledByTimeout`
    re-enable + health check are unchanged; the sentinel decision moved into the pure
    filter so it is tested.
  * **Ordering (the D8 reviewer's open risk), two parts.** (1) The engine's shortcut-hook
    forwarder no longer locks the translator from the hook thread: it sends
    `VideoInput::Key` into the **same FIFO channel** as the surface (`engine/src/session/mod.rs`
    `hook_forwarder`). Before, a surface Cmd waiting in that channel (behind the input
    thread's up-to-10 ms move pacing) could reach the translator after the hook's C. Codes
    outside the shared keymap, and sends after the input thread ended, fall back to the
    translator directly; the input loop now drains queued keys when the session stops.
    (2) M6's "forward the chord's modifier unless the surface already holds it" now covers
    all eight modifier keys (Cmd, Ctrl, Option, Shift, both sides) both ways: a modifier
    down in the event's flags that the surface has not sent is forwarded first (and released
    by the tap later); one the surface still holds but the flags no longer carry is released
    first (a later duplicate up from the surface is harmless). Cmd+Shift+4 can no longer
    reach the host as Cmd+4.
  * Focus loss: besides the view's `release_all` (surface-held keys), the filter releases
    every key and modifier it holds the moment focus goes (checked on every tap event and
    every 50 ms run-loop turn); a taken key still physically held stays swallowed, silently,
    until its key-up (the app that now has the keyboard never saw it go down). Key-ups of
    keys pressed before capture pass (the monitor forwards them). No new setting.
  * D8 still applies downstream: with "Translate modifiers" on (Settings → Advanced), Cmd
    arrives on Windows/Linux as Ctrl (Cmd+C = Ctrl+C, Cmd+Tab = Alt+Tab); off, as Super/Win.
  * Tests: `tests/shortcut_filter.rs` (runs on Windows/Linux too): any Cmd chord taken
    (incl. Cmd+Enter, Cmd+Shift+4, Cmd+Opt+D, Cmd+Ctrl+F, F3, Esc), Ctrl+arrows with extra
    modifiers, plain/non-Cmd chords untouched, Cmd+Q and its layout rule, reserved combos
    listed and never claimed, sentinel ignored (also for flagsChanged and key-up), modifier
    synchronisation both ways, a Cmd pressed before capture released on the host, focus loss
    releases at once with orphans swallowed, stale orphans, release_all. Engine:
    `hook_keys_queue_behind_the_surface_keys_sent_before_them`, the fallback, the stop drain.
    macOS unit test pins the filter's modifier table to `keymap::MODIFIERS`.
  * Toolbar reveal (scope addition, owner: "hovering the top edge does nothing", window not
    full screen, flick out the top does nothing): not reproducible here. Added (a) a debug
    line on every reveal transition with the view and layer frames, and a once-per-show
    "pointer events arrive" line, so app.log tells "events never arrive" from "transition
    fires but nothing moves"; (b) `apply_layout` now sets the layer's frame to the view's
    when it did not follow (layer-hosting view; only when the layer sits directly in the
    parent's layer); (c) `exited_through_top` (surface-wgpu, pure, tested): an exit within
    8 CSS px of the top and nearer it than any side counts; (d) native full screen: the
    menu bar + title bar overlay (`system_cover_pt`) moves the hide depth below it
    (`Reveal::on_pointer_under_cover`, tested), otherwise the bar hid in the move that
    uncovered it. Diagnosis and device steps: checklists/M6.md §7.
  * platform-api: no change. New objc2-app-kit feature `NSScreen` (no new crate).

### D9 — Draw the host's real cursor sprite on the viewer  (size M, all OSes; owner feedback 2026-11-01)
* Problem: every viewer draws a synthetic white triangle at the out-of-band pointer position
  (`render/src/lib.rs` cur_ps, and the surface-wgpu presenter's cursor quad). The hosts
  already produce `CursorShape` (W1 DDA shape, M1 ScreenCaptureKit, L1 portal metadata,
  L2 XFixes) and `CursorUpdate.shape` reaches the engine, but no shape is ever sent to the
  viewer, so the user sees an arrow-ish blob instead of the I-beam, hand, resize or
  Yaru/macOS/Windows cursor the host shows ("the mouse looks weird").
* Deliverable: (1) protocol: an additive cursor-shape message on the existing cursor channel
  (BGRA premultiplied or straight alpha stated explicitly, width/height, hotspot; sent only
  when the shape changes, capped at 256×256, dedup by content hash; older viewers ignore
  it, older hosts never send it and the viewer keeps the current quad as fallback);
  (2) host: send the shape when `CursorUpdate.shape` changes and once at session start;
  (3) viewer: upload the sprite to a texture and draw it at `position - hotspot`, scaled
  with the video (Retina/HiDPI aware: the hotspot and size are in host pixels), with
  correct alpha; hidden when `visible == false`; (4) both presenters: surface-wgpu (macOS,
  Linux, and Windows if it uses it) and the D3D11 `render` crate; (5) the loopback harness
  proves shape delivery + a pixel check that the sprite's opaque pixels land at the right
  place; unit tests for hotspot/scale math; (6) README feature table row.
* Non-goals: cursor animation (animated cursors send their first frame), cursor trails,
  software cursor smoothing. Keep the out-of-band position path untouched (latency).
* Acceptance (CI + I1–I3): loopback test green on all three OS legs; on a real session the
  viewer shows the host's actual cursor images including I-beam in a terminal.
* As built (branch `task/D9`, in review; no real session yet):
  * Wire (`protocol/src/cursor.rs`, recorded in plans/04 §2): additive `ControlMsg::CursorShape`
    (`"t":"curshape"`) on the reliable ordered **`ctl`** channel, not the `cursor` channel
    (0 retransmits: a lost image would stay wrong until the next change; decision accepted by
    the orchestrator). Premultiplied BGRA as hex, 16 KiB of pixels per part (≤ 16 parts,
    ~32 KiB per message), header repeated per part; the viewer's `Assembler` bounds sizes
    before allocating, requires in-order parts and clamps colour ≤ alpha. Positions stay on
    the `cursor` channel unchanged; their existing `shape` field names the image id (`None` =
    built-in arrow, which is all an older host ever sends). Older viewers fail to parse
    `curshape` and drop it.
  * Host (`engine/src/media/cursor.rs` `HostCursor`, used in `media/host.rs`): straight →
    premultiplied once, FNV-1a content id over size, hotspot, host frame size and pixels;
    sent only for an id not sent this session (FIFO of 32 mirroring the viewer's cache), and
    again when the frame size changes; > 256 px per side is not sent (log line, viewer draws
    the arrow; accepted instead of scaling). Per session, so a reconnect sends it again; the
    capture backends report the current shape on a fresh session (DDA/SCK/XFixes/portal).
  * Viewer (`ViewerCursors` in `Shared`, `InstalledSprite` in `media/viewer.rs`): the presenter
    is given the image only when the named id changes; an id still arriving keeps the current
    picture; hidden (`visible == false`) draws nothing.
  * `platform-api` (approved additive): `Presenter::set_cursor_sprite` (default no-op),
    `CursorSprite` (+ `placement`: top-left = pointer − hotspot, size = image / host frame
    size, so Retina/HiDPI on either side needs nothing more), `MAX_CURSOR_SIDE`; doc fix:
    `CursorUpdate.position` is the hotspot. **Windows fix:** DXGI's pointer position is the
    image's top-left (DesktopDuplication sample), so `DdaCapture` now adds the last hotspot
    (`dda_hotspot_position`); before, every non-arrow cursor was offset by its hotspot.
  * Presenters: Windows uses the D3D11 `render` crate (premultiplied blend state, immutable
    BGRA texture at t2, `spr_ps`; the fallback arrow redrawn tip-on-hotspot like wgpu's);
    macOS and Linux use `surface-wgpu` (binding 4, `fs_sprite`, `PREMULTIPLIED_ALPHA_BLENDING`
    pipeline, `geometry::sprite_rect`). Both keep the arrow until the first image arrives.
  * Tests: protocol (round trip through JSON at 256², message size < 64 KiB, hostile/out-of-order
    parts, clamping); engine (dedup, switch-back by id, frame-size re-send, oversize fallback,
    hotspot clamp, host/viewer caches in step past capacity); platform-api (`placement` math,
    mock CPU compositor); render (`cursor_quad` math, HLSL entry points compile); surface-wgpu
    (geometry + a headless GPU pixel test: opaque texels on the hotspot at 1:1 and 2×, a
    half-alpha texel blending to ≈ 192, arrow fallback); platform-windows (DDA hotspot
    conversion); loopback `host_cursor_image_reaches_the_viewer_and_lands_on_the_hotspot`
    (image delivered once with the host's geometry, and its opaque pixels cover exactly the
    expected 4×6 pixels at every unclipped pointer position, through the mock presenter's CPU
    compositor). No D3D11 pixel test: the `render` presenter needs an HWND swapchain.
  * Review round 1 fixes:
    * **ctl send queue** (`engine/src/session/ctl_queue.rs`): `Transport::send_ctl` now returns
      `Ok(false)` when str0m refuses a message (its 128 KiB send buffer, shared by all
      channels, is full until the peer acknowledges). Before, the driver ignored that and the
      message was lost; every cursor image ≥ 128 px (4+ parts) lost its tail and was never
      re-sent. Every ctl send (media-thread messages, ping/pong, keyframe requests, bye) now
      goes through one ordered queue that keeps refused messages at its head and retries them
      each driver pass, before newer ones; it also replaces the pre-open backlog. A message
      larger than the whole buffer is dropped with a warning (it could never be accepted).
      Loopback `maximum_size_cursor_images_in_a_burst_all_arrive_intact`: six 256×256 images
      (16 parts each, 96 parts, ≈ 3.2 MB on ctl) all arrive, the last byte-exact and drawn.
    * Windows monochrome cursors: inverting pixels (the standard I-beam) are black with a 1-px
      white rim (`outline_xor` since round 2, as WebRTC's `AddCursorOutline`), so they show on a dark
      terminal.
    * A 1:1 image is snapped to whole target pixels in both presenters (a centred letterbox
      can start at x.5 and the bilinear sampler would blur it); the mock compositor's doc says
      it samples nearest and does not snap.
  * Review round 2 fixes:
    * Liveness: every inbound ctl message now refreshes the peer-silence timer, not only
      Ping/Pong. The peer's pong waits behind its cursor-image backlog in the ordered ctl
      queue (MBs on a slow uplink: an animated large cursor can mean many 132–527 KB images),
      and counting only pongs showed "link trouble" — or ended the session past the 15 s
      grace — while video and parts kept arriving. The burst loopback asserts neither side
      reports link trouble; on localhost the drain is milliseconds, so the slow-uplink case
      rests on the rule itself (no link shaper in the harness).
    * Teardown: the queue is cleared before the final Bye, so a cursor backlog cannot keep the
      Bye out of the 250 ms pump.
    * Windows masked-colour cursors: non-black XOR pixels get the same 1-px rim as monochrome
      inverting pixels, in the contrasting colour (white beside dark, black beside light), so a
      white-XOR I-beam shows on a white page (`outline_xor`).
  * Not done (accepted): the host does not drop a queued image when a newer shape supersedes
    it, so on a slow uplink a burst of new large images delays later ctl messages (perm,
    clipboard) until it drains; liveness is no longer affected.
  * Seen while fixing, not changed (outside D9), follow-ups:
    * The viewer's file push sends `bulk` frames while `bulk_buffered() < 256 KiB`, but
      `send_bulk` ignores the same `Ok(false)`, and str0m refuses anything past 128 KiB
      buffered across all channels, so a large file transfer can lose chunks silently. Needs
      the same treatment as ctl.
    * Clipboard text is capped at `MAX_TEXT_BYTES` (256 KiB, `platform-api/src/os.rs`), but a
      ctl message over str0m's 128 KiB send buffer can never be accepted. Between two copies
      of this app that buffer is the real limit (str0m peers advertise a 256 KiB
      `max-message-size`, `LOCAL_MAX_MESSAGE_SIZE`); only a peer advertising none would cap
      messages at the 64 KiB default. Such text was silently lost before D9 and is now dropped
      with a warning. Cap the clipboard at what fits, or chunk it.
  * Not verified: real sessions (I1–I3): arrow/I-beam/hand/resize on each host, DXGI top-left
    semantics on hardware, portal cursor scale under fractional scaling, Retina scale.

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
* First CI run (main #46, 2026-11-01): `wayland-gnome` green; `wayland-sway` reached the tests, L5 `wayland_surface` passed, and L1 `portal_capture` was refused instantly ("A backend call failed: Operation not permitted" in portal.log, nothing in portal-wlr.log). Script changes after that run: both portals started by the script with `--replace` and at full log level, awaited through `busctl list` only (the `busctl introspect` probe had D-Bus-activated a second frontend whose output went to the bus daemon); `XDPW_PERSIST_MODE=permanent` exported (xdg-desktop-portal-wlr forces persist_mode to none otherwise, which the restore-token check needs); the moving client runs under a restart loop with its own log (`simple-shm.log`; the first run saw it exit); the output list is printed. The next run is the check. Second run (main #49): the stack came up and the backend TRACE showed the cause: xdg-desktop-portal-wlr 0.7.1 requires a `linux_dmabuf` screencopy frame event whenever the compositor advertises screencopy v3 (`start_screencast`: `screencast_version >= 3 && screencopy_frame_info[DMABUF].format == DRM_FORMAT_INVALID` -> "unable to receive a valid format from wlr_screencopy", -1, "Operation not permitted" at the frontend); a headless pixman sway never sends one. **Decision (2026-11-01):** `wayland-sway` runs L5 and L6 and skips `portal_capture` with a `::warning` annotation; L1 portal capture on wlroots is a device-pass item like GNOME. Ways to close, unverified: `modprobe vkms` + Mesa kms_swrast + `WLR_RENDERER=gles2` so output buffers are dmabufs, or a newer xdg-desktop-portal-wlr accepting shm-only v3 frames.
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
