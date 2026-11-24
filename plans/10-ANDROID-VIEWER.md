# 10 — Android viewer: see and control your computer from an Android phone

Written 2026-11-24 against `main` @ `0091c93` (v2.3.3). Read-only research of the
`platform-api` traits, the engine's connect and viewer media paths, the app crate and the
crate manifests. Paths are relative to `native/` unless they start with `server/` or `plans/`.
**Nothing here is built yet.** The structure document (`10a-STRUCTURE-AND-FLOWS.md`) is written
once the owner has answered the decisions in §6. Several of those decisions change its shape.

## Progress

| Step | What | Status |
|---|---|---|
| S | This plan + structure (10a), adversarial review folded in, decisions recorded | **in progress**: plan written; waiting on the owner's answers to §6; 10a not started |
| A0 | Spikes: the Rust core on Android, MediaCodec → SurfaceView latency, view embedding | not started |
| A1 | Engine: viewer-only platforms, surface loss and return | not started |
| A2 | `platform-android`: decode, video view, secrets, paths, clipboard | not started |
| A3 | Android app: connect, password, view-only session, CI, signed APK | not started |
| A4 | Touch control, zoom and pan, keyboards | not started |
| A5 | Typing any character: optional `text` input message, host side on all three OSes | not started |
| A6 | Account sign-in, synced remote list, clipboard, settings | not started |
| Final | Whole-feature review, checklists, release | not started |

## 1. What the owner asked for (2026-11-24)

- Use ShareCtrlScreen **from an Android phone to a desktop**: see the desktop's screen on the
  phone and control it (mouse and keyboard) from the phone.
- **Viewer only.** The phone is never a host: its own screen is not shared and nothing
  controls the phone.
- It should feel like the desktop app on a phone, which means AnyDesk-like (plans/00 guiding
  rule).

## 2. Where we are today (facts the design must respect)

1. **No Android anything.** The workspace builds Windows, macOS and Linux only. The platform is
   picked by `cfg(target_os)` in `app/src/main.rs:89-95`, and nothing in `plans/` or `pans/`
   mentions Android or mobile.
2. **The core is OS-free.** `protocol`, `signaling`, `transport` and `engine` depend on no OS
   crate (their `Cargo.toml`s: str0m, tokio, tokio-tungstenite, rustls with `aws_lc_rs`,
   reed-solomon-simd, if-addrs, hashes). They talk to the OS only through `platform_api::Platform`
   (`crates/platform-api/src/platform.rs:113`). This is the reason an Android viewer is
   realistic: an Android build of the core is a new `Platform` plus a new UI, not a new engine.
   It is **unverified** that each dependency cross-compiles for `aarch64-linux-android`; A0
   proves it. The likely sore spot is `aws-lc-rs` in `signaling`, which needs the NDK's clang
   and CMake.
3. **What a viewer needs from `Platform`.** It needs `codecs().viewer_decodable()` and
   `new_decoder`, `create_surface` (`platform.rs:128`), `secrets` (saved passwords), `paths`
   and `create_clipboard`. It does not need `create_capture` (`:124`), `create_injector`
   (`:152`), `create_shortcut_hook`, `autostart` or `displays`. Fail-safe stand-ins already
   exist in `crates/platform-api/src/unavailable.rs` (`NoSecretStore`, `NoClipboard`,
   `NoAutostart`, `NoCodecs`).
4. **The surface contract assumes a desktop window.** `VideoSurface`
   (`crates/platform-api/src/surface.rs:96`) is a native view created on the UI thread from a
   `RawWindowHandle` that lives as long as the session. `raw-window-handle` 0.6 has an
   `AndroidNdk` variant (an `ANativeWindow`). Android, however, **destroys a SurfaceView's
   surface** whenever the app goes to the background, and on any configuration change it does
   not handle itself. Today's contract has no "the surface went away and came back" state. The
   viewer media thread creates its presenter once and its decoder on that presenter's device
   (`crates/engine/src/media/viewer.rs:60-63`). §4.3 designs the change.
5. **Input is pointer and physical-key based.** `VideoInput` (`surface.rs:16`) and `InputMsg`
   (`crates/protocol/src/input.rs:41`) carry normalized pointer moves, buttons, wheel notches
   and **DOM `KeyboardEvent.code`** (physical keys). A phone's on-screen keyboard (IME) commits
   *text*, not physical keys, so there is no exact mapping. Hardware (Bluetooth/USB) keyboards
   report Linux evdev scan codes through `KeyEvent.getScanCode()`, and the generated keymap
   already maps evdev (`crates/platform-api/src/keymap.rs:790` `from_evdev`).
6. **The host encodes to the viewer's view size.** `ControlMsg::ViewSize`
   (`crates/protocol/src/control.rs:62`) is sent by the viewer media thread after 600 ms of a
   stable size ≥ 320×240 (`media/viewer.rs:150-162`). The host encodes to exactly that size and
   never upscales past its own screen. A phone in portrait is about 1080 px wide, so the host
   would send a desktop squeezed to 1080 px. This is right when the whole desktop is shown, and
   wrong once the user zooms in to read text. §4.4 uses it deliberately.
7. **Every phone has a UUID and registers.** A phone running the engine registers with the
   signaling server under its own UUID, like any install, so a peer could try to connect *to*
   it. Today an incoming request goes to approval or a password challenge
   (`crates/engine/src/sessions.rs:137-195`). A refusal reason a peer does not know is shown as
   "connection {reason}" (`sessions.rs:659-669`).
8. **Backgrounding already has a wire answer.** A session that is not on screen is paused at
   the host with `ControlMsg::VideoPause` when the host advertises `vpause` (plans/08 P6;
   `crates/protocol/src/signaling.rs:153` `HostCaps`; `Engine::set_screen`, `sessions.rs:810`).
   An app in the background is exactly "no session on screen".
9. **The desktop app crate is desktop-only.** `app/` is a Tauri binary with the updater and
   process plugins, tray, native menus, detachable windows, pop-outs and the Windows service
   handover (`app/src/main.rs`, `app/Cargo.toml`). The account client (sign-in, synced remote
   list) lives **in the app crate** (`app/src/accounts.rs:727` `account_sign_in` and
   neighbours) and uses reqwest. A phone app that signs in needs that client in a shared crate
   first.
10. **Tauri 2's Android target cannot officially host a native view.** Tauri 2 builds for
    Android, and a Kotlin plugin gets the `Activity` and the `WebView`. A Tauri maintainer has
    stated, however, that there is currently no supported way to put native UI components
    beside the webview ([tauri-apps/tauri#14484](https://github.com/tauri-apps/tauri/pull/14484);
    [Mobile Plugin Development](https://v2.tauri.app/develop/plugins/develop-mobile/)). Our
    video is a native surface by design (plans/00 decision 3), so this weighs on decision D1.
11. **The software codec fallback has Android builds.** Cisco publishes OpenH264 for Android
    (`libopenh264-2.6.0-android-arm64.8.so.bz2` among the v2.6.0 binaries,
    [release listing](https://newreleases.io/project/github/cisco/openh264/release/v2.6.0)). The
    file list we download and verify is `crates/codec-openh264/src/binary.rs:67-99`, with
    hashes taken from `openh264-sys2`'s `blobs/hashes.txt`. **Unverified:** whether
    `openh264-sys2` 0.9.9 lists the Android files, and whether the Cisco build is aligned to
    Android's 16 KB pages (upstream has a commit "Align remaining Android libraries to 16 KB
    pages" whose release is not confirmed). A0 checks both.
12. **Licence gate.** `deny.toml` allows permissive licences plus file-level MPL-2.0. Gradle
    (Kotlin/AndroidX) dependencies are outside `cargo deny`, and need their own licence gate
    (§5).

## 3. Scope

**In:**
- Android 10+ phones and tablets (the API level is decision D4), arm64. x86_64 is built for
  the emulator only.
- Connect by ID, password and saved password, approval-mode wait, recents.
- View with view↔control switching, as on the desktop.
- Touch control: pointer, clicks, right-click, drag, scroll and zoom/pan.
- Keyboards: on-screen keyboard plus an extra-keys bar, and hardware keyboards.
- Text clipboard both ways.
- Account sign-in and the synced remote list.
- A signed APK from the release workflow.

**Out (each could be its own later plan):**
- The phone as a host: sharing or controlling the phone's screen.
- iOS.
- File transfer to and from the phone.
- Several sessions at once (tabs), detachable windows, pop-out displays.
- Audio (the product has none today).
- Start at sign-in and the Windows service.

## 4. Design

### 4.1 Shape (depends on D1; recommendation shown)

```
┌──────────── Android app (Kotlin, Jetpack Compose) ────────────┐
│ Home (my ID, connect box, recents/account list) · Password    │
│ Session screen: SurfaceView (video) + gesture layer + toolbar │
│ Settings · Account sign-in                                    │
└──────────────┬──────────────────────────────┬────────────────┘
               │ generated Kotlin bindings     │ Surface → ANativeWindow
               ▼                               ▼
┌──── crates/mobile (cdylib, `libsharectrl.so`) ────────────────┐
│ The phone's "commands.rs + bridge.rs": Engine facade, events  │
└──────────────┬────────────────────────────────────────────────┘
               ▼
 engine · signaling · transport · protocol · accounts (moved out of app/)
               ▼
 crates/platform-android: AMediaCodec decoder (+ OpenH264 fallback),
 video surface/presenter on ANativeWindow, Keystore secrets, paths,
 clipboard; capture / injector / shortcut hook / autostart = unavailable
```

The phone runs the same engine, wire protocol and server as the desktop. **The server and
desktop hosts need no change for A0–A4 and A6.** A5 (typing any character) is the one
capability-gated host change.

### 4.2 The phone is never a host

`PlatformInfo` gains a field saying the platform can host (default `true` on every existing
backend, so desktop behaviour is unchanged). When it is `false`, the engine answers every
incoming connect-request with an immediate refusal. It sends no approval event, no password
challenge and no admission claim, so a stranger who knows the phone's ID cannot raise prompts
on it. The reason sent is decision D6. The ID is still needed (it is how the phone registers),
but the UI shows it only in Settings.

### 4.3 The video surface on Android, and surface loss

- The session screen is a `SurfaceView`. Its `Surface` becomes an `ANativeWindow` in Rust, and
  `platform-android` wraps it as the `VideoSurface`.
- **Hardware path:** `AMediaCodec` (NDK) decodes straight into that window. Frames are never
  copied to the CPU, and the platform's compositor scales them. Low-latency mode is requested
  where the decoder offers it. `viewer_decodable` lists what `MediaCodecList` says this device
  decodes in hardware (H.264 always; HEVC/AV1 only when present), matching the
  `CodecFactory` contract.
- **Software path:** Cisco OpenH264 produces CPU NV12, which a presenter draws into the same
  window. Whether that presenter is `surface-wgpu` on Vulkan or a CPU blit is an A0 spike
  result. Only one presenter ever owns the window.
- **Cursor:** the host's cursor sprite and the D10 local cursor are drawn in a separate overlay
  view above the video. A decoder rendering straight to the window leaves no frame for us to
  draw on.
- **Surface loss:** Android destroys the `Surface` on background, screen-off and some
  configuration changes. The engine contract gains an explicit "surface lost / surface back"
  state:
  - While lost, the session counts as off screen, so the host gets `VideoPause` (fact 8).
  - The decoder is detached from the dead window.
  - On return, output is re-attached (`AMediaCodec_setOutputSurface` or a new decoder) and a
    keyframe is requested.
  - The desktop backends never enter that state.

  This touches the engine's media thread, so it is phase A1 with loopback tests, and it lands
  with no behaviour change on desktop.

### 4.4 Zoom, pan and the encode size

- Zoom is done by **making the video rectangle larger than the screen and moving it**. The
  surface's `client_size` is the zoomed rectangle, capped at the host's own screen size.
- Through the existing `ViewSize` (fact 6), a zoomed-in phone therefore asks the host for more
  pixels and text stays sharp. Zoomed out, the host sends only what the phone can show, which
  saves bandwidth on mobile data.
- The existing 600 ms debounce keeps a pinch from causing a burst of re-encodes.
- No protocol change.

### 4.5 Touch and keyboards

- **Two touch modes** (default is D5):
  - *Touchpad*: a finger moves a cursor relative to where it was, tap clicks at the cursor.
    This is precise on small text.
  - *Direct*: tap clicks where you touch.

  Both modes have the same gestures:

  | Gesture | Action |
  |---|---|
  | Long-press | Right-click |
  | Double-tap-and-hold | Drag |
  | Two-finger drag | Wheel |
  | Pinch | Zoom |
  | Three-finger tap | Show or hide the toolbar |

  Everything becomes the existing `VideoInput` events, so the engine and hosts see a mouse.
- **Hardware keyboards:** `KeyEvent.getScanCode()` → `keymap::from_evdev` → DOM code. These
  are physical keys, exactly like the desktop.
- **On-screen keyboard (A4):** committed characters are mapped to key presses on a US layout
  (Shift added for capitals and symbols). This is exact for ASCII on a US-compatible host
  layout. Other characters are dropped with a visible notice.
- **Extra-keys bar:** Esc, Tab, Ctrl, Alt, Win/Cmd, arrows, F1–F12 and Ctrl+Alt+Del. Ctrl and
  Alt can be held on, so Ctrl+C works with the on-screen keyboard.
- **Typing any character (A5):** a new optional `InputMsg` carrying Unicode text, sent only to
  a host that advertises it in `HostCaps`. Old hosts keep getting key presses. Each host
  injects Unicode natively: Windows `SendInput` with `KEYEVENTF_UNICODE`, macOS
  `CGEventKeyboardSetUnicodeString`, and on Linux the injector's per-backend method, which 10a
  details per X11 and Wayland path. This is the only host change in the plan, and it ships host
  first.

### 4.6 App lifecycle

- **Foreground with a session:** streaming. The screen stays on while a session is shown.
- **Background:** the surface is lost, so the session is off screen and video is paused at the
  host (§4.3). The connection stays up for as long as Android keeps the process. What happens
  after that is decision D7.
- **Back button on the session screen:** asks "Disconnect?". No session ever ends silently on a
  stray swipe.
- **Rotation:** portrait and landscape both. The activity handles the configuration change
  itself so the surface is resized, not destroyed (decision D8).

### 4.7 Secrets, paths and config

- **Saved passwords:** an Android Keystore AES-GCM key that cannot leave the device seals
  them. This implements `SecretStore`; nothing reaches `config.json` in plain text.
- **Config and logs:** in the app's private files directory, so `app.log` exists for diagnosis
  exactly as on the desktop. The same `config.json` schema is used, so the same migrations and
  UUID rules apply.
- **Not supported on the phone:** start-at-login (`NoAutostart`) and the downloads directory
  (no file transfer in scope).

## 5. Build, test and CI

- **Rust:** add the `aarch64-linux-android` and `x86_64-linux-android` targets and build with
  `cargo-ndk`. Workspace lints and `cargo deny` apply unchanged.
  - `platform-android` and `mobile` are workspace members compiled only for Android; on other
    targets they compile to nothing, so `cargo test --workspace` stays green on all three OSes.
  - New Rust dependencies need verified licences in `plans/06`: `ndk`/`jni` (MIT/Apache) and
    the binding generator (D2; uniffi is MPL-2.0, which `deny.toml` allows).
- **Android project:** a Gradle project at `native/android/` (D9).
  - Kotlin, AndroidX and Compose dependencies are all Apache-2.0.
  - A Gradle licence check fails the build on anything not permissive, as the Gradle-side
    counterpart of `cargo deny`.
- **New CI job (Linux runner):** NDK build, Gradle build, unit tests, and an **emulator smoke
  test** on x86_64:
  - The engine connects over loopback to a mock host.
  - The emulator's software `MediaCodec` decodes the committed golden H.264 stream
    (`testdata/golden`), so the decoder joins the `codec-goldens` matrix in CI.
  - A required-env variable, `SCT_ANDROID_TESTS_REQUIRED`, turns an emulator skip into a
    failure, following the `SCT_*_REQUIRED` convention.
- **Release:** the release workflow also builds a signed APK.
  - The signing key and password live in GitHub secrets, named in 10a.
  - The version is the workspace version, and `versionCode` is derived from it.
  - Distribution is D3.
- **Hardware:** checklists `plans/checklists/ANDROID-A3.md` (view), `-A4.md` (touch and
  keyboards) and `-A6.md` (account, clipboard), run on the owner's phone against a Windows
  host. Hardware decoders, latency, rotation and background behaviour count as working only
  after these pass.

## 6. Decisions for the owner

Each decision lists the options; **the recommendation comes first**.

| # | Question | Options |
|---|---|---|
| D1 | How is the phone app's UI built? | **(a) Native Android UI in Kotlin + Jetpack Compose over the shared Rust core.** Full control of the video view, touch, keyboard and lifecycle; the standard way to build an Android app; the phone UI is a different design from the desktop anyway. Costs a second small UI (home, password, session, settings, sign-in). (b) Tauri 2 Android reusing `app/ui`: one web UI, but no supported way to place the native video view (fact 10), and touches over the video would have to cross the webview. The desktop UI's tabs, menus, pop-outs and window layout do not fit a phone either. |
| D2 | How does Kotlin call Rust? | **(a) uniffi-generated bindings** (MPL-2.0, allowed): typed Kotlin API, callbacks for engine events, no hand-written JNI for the app API; raw `jni`/`ndk` only for the `Surface` and Keystore. (b) Hand-written JNI throughout: no generator, much more unsafe glue to review. |
| D3 | How do people install it? | **(a) Signed APK on GitHub Releases first** (like the desktop builds), with an in-app "new version" check that opens the release page; Google Play later as a drop-in. (b) Google Play from day one (needs a Play developer account, Play signing, a store listing and review). (c) Both at once. |
| D4 | Oldest Android supported? | **(a) Android 10 (API 29):** modern NDK media and Keystore APIs, and the large majority of phones in use. Confirm against the current distribution before locking it. (b) Android 8 (API 26): more old phones, more workarounds. |
| D5 | Default touch mode? | **(a) Touchpad mode**, with a toolbar toggle to Direct (precise on small desktop text). (b) Direct touch by default. |
| D6 | What does a peer see if it tries to connect *to* a phone? | **(a) "connection denied"**: the existing reason, so every old peer understands it. (b) A new reason such as `viewer-only`, which old peers would show as "connection viewer-only". |
| D7 | The app is in the background mid-session? | **(a) Video pauses at once; the connection stays up as long as Android allows**; if Android stops the app, the host sees the session end normally (ping watchdog). No persistent notification. (b) Keep the session alive in the background with a foreground-service notification. (c) Disconnect as soon as the app leaves the screen. |
| D8 | Orientation? | **(a) Portrait and landscape, follow the phone.** (b) Landscape only during a session. |
| D9 | Where does the Android project live? | **(a) `native/android/`** beside the Rust workspace it builds. (b) A top-level `android/`. |
| D10 | Which features does the first shipped version (A3 + A4) include? | **(a) View and control with touch and keyboards**; account sign-in and clipboard follow in A6; typing any character in A5. (b) Hold the first release until A6 is done too. |

## 7. Phases

Each phase ships on its own, leaves `main` releasable, and is marked complete here in the
commit that finishes it.

| Phase | What | Needs | Done when |
|---|---|---|---|
| **S** | 10a structure doc (ownership, file layout, types, the Kotlin↔Rust API, the surface state machine, every flow, invariants); adversarial review; decisions recorded | owner's answers to §6 | review findings folded in, 10a agreed |
| **A0** | Time-boxed spikes (3 days): (1) `engine`, `signaling`, `transport`, `codec-openh264` cross-compile for Android and an engine loopback runs on the emulator; (2) AMediaCodec → SurfaceView from Rust, decode latency on the golden stream and on a real phone; (3) the software presenter (wgpu on Vulkan vs CPU blit); (4) Cisco Android binary in `openh264-sys2`'s hash list and its 16 KB alignment | S | a written result per spike in `plans/spikes/`, with design changes folded into 10a |
| **A1** | Engine: `can_host` + immediate refusal; surface lost/back state; loopback tests with the mock platform; no desktop behaviour change | S | CI green on all three OSes |
| **A2** | `crates/platform-android`: decoder (MediaCodec + OpenH264), surface/presenter/overlay, Keystore secrets, paths, clipboard; codec-goldens on the emulator | A0, A1 | emulator CI green; decoder bit-exact on the goldens |
| **A3** | `crates/mobile` + the Android app: home, connect, password, approval wait, recents, view-only session, Settings (ID, OpenH264 toggle and Cisco notice, log export); CI job; signed APK | A2 | `checklists/ANDROID-A3.md` passes on the owner's phone |
| **A4** | Touch modes, gestures, zoom/pan with `ViewSize`, local cursor, extra-keys bar, on-screen and hardware keyboards; control↔view switching | A3 | `checklists/ANDROID-A4.md` passes |
| **A5** | Optional Unicode `text` input message + `HostCaps` flag; host injection on Windows, macOS, Linux; phone sends it when offered | A4 | unit and loopback tests; host checklists on each OS; old host gets key presses |
| **A6** | Move the account client out of `app/` into a shared crate (no behaviour change first), then sign-in and synced remote list on the phone; clipboard text both ways | A3 | `checklists/ANDROID-A6.md` passes; desktop accounts unchanged |
| **Final** | Whole-feature review, README/CLAUDE.md updates, version bump, release with the APK | all | gate green, release published |

## 8. Risks and how the spikes answer them

| Risk | Answer |
|---|---|
| A Rust dependency does not build for Android (most likely `aws-lc-rs`) | A0 (1). Fallback: switch `signaling`'s rustls provider to `ring`, which the app crate already uses |
| Hardware decoders buffer frames on some phones (latency) | A0 (2) measures. Request low-latency mode; feed one access unit at a time; report per-device results in the checklist |
| The software path is slow on older phones | A0 (3) picks the presenter; OpenH264 is only the fallback |
| The surface vanishes mid-frame (background, rotation) | A1 makes loss an explicit state with tests; A2 implements it; A3 checklist covers background, screen-off and rotation |
| The on-screen keyboard cannot type non-ASCII on old hosts | A5 adds the capability-gated text message; until then a visible notice, never a silent wrong key |
| Gradle dependencies slip past the licence gate | The Gradle licence check in CI (§5) |

## 9. What is unverified as of this plan

Everything is unverified until it is built, and every point below is verified by an A0 spike
or a phase checklist:

- Cross-compilation of the core for Android.
- MediaCodec latency on real phones.
- Whether `openh264-sys2` 0.9.9 carries the Android hashes, and whether Cisco's Android
  binaries are 16 KB aligned.
- Embedding behaviour for D1(b).
- The Android version share behind D4.
