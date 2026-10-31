# 01 — macOS findings (no Apple Developer account)

Researched 2026-10-31. Every claim below is either verified in the code, in crate metadata
(crates.io API), or in the linked sources. Items marked **unverified** need the spike noted
in [05-TASKS.md](05-TASKS.md).

## TL;DR

* **You do not need an Apple Developer account** to build, run, test, or distribute the app.
  What you give up: notarization. Consequence: every user does a one-time
  *System Settings → Privacy & Security → "Open Anyway"* after the first blocked launch
  (Sequoia removed the Control-click-Open shortcut), and the app must be at least
  **ad-hoc signed** (Tauri: `bundle.macOS.signingIdentity = "-"`) or macOS calls the DMG
  "damaged". Nothing else is blocked: ScreenCaptureKit, VideoToolbox, CGEvent, Keychain,
  LaunchAgents all work for unsigned/ad-hoc apps.
* **Sign with a self-signed certificate, not plain ad-hoc.** With ad-hoc signing the TCC
  grants (Screen Recording, Accessibility) are keyed to the binary's `cdhash` and are
  **lost on every update/rebuild**. With a self-signed cert (created once in Keychain or
  with `openssl`, stored as a CI secret) the designated requirement becomes
  `identifier "dev.netameta.sharectrl" and certificate leaf = H"…"`, so grants survive
  updates. Gatekeeper still treats it as unidentified ("Open Anyway" once). Verified via
  `codesign -dr -` output in field reports. ([lan-screenshare #51](https://github.com/nsfxu/lan-screenshare/issues/51),
  [macos-app-template #61](https://github.com/tomada1114/macos-app-template/issues/61),
  [hermes-agent #74331](https://github.com/NousResearch/hermes-agent/issues/74331),
  [OpenClaw signing docs](https://docs.openclaw.ai/platforms/mac/signing.md))
* **You need interactive access to a real Mac for the capture/injection/permission parts**
  — not for building. GitHub-hosted macOS runners (free for your public repo) can build,
  run unit tests, and even exercise VideoToolbox encode/decode (no permission needed), but
  they **cannot grant Screen Recording/Accessibility** (SIP on, TCC not editable; the
  runner-images issues show no working method). A macOS VM on your Windows box is both an
  EULA violation and useless for this work (no GPU → no VideoToolbox/SCK). Cheapest real
  options: a rented Mac mini by VNC (Scaleway: ~€0.10–0.22/h, 24 h minimum, VNC into the
  GUI where you click the TCC prompts — their own Reemo tutorial does exactly that), or a
  used M1 Mac mini. See [03-TESTING-AND-MACHINES.md](03-TESTING-AND-MACHINES.md).

## 1. Distribution without Apple

| Step | Without an Apple account | Notes |
|---|---|---|
| Build `.app`/`.dmg` | `cargo tauri build` on a macOS host (GitHub `macos-latest`, Xcode preinstalled, free for public repos) | Tauri's bundler cannot make `.app`/`.dmg` on Linux (PR #16342 closed unmerged 2026-10-26). Cross-compiling with zig + Apple SDK is also an SDK-license gray zone. |
| Code signature | **required on Apple Silicon** (unsigned arm64 binaries are killed). Tauri: `"signingIdentity": "-"` for ad-hoc, or your self-signed identity name. Tauri skips DMG signing for `"-"`; a non-empty `APPLE_SIGNING_IDENTITY` env var overrides the config. | Self-signed cert: `security create-keychain` + import `.p12` in CI; mark it trusted for code signing in the temp keychain; `codesign --deep --force --sign "ShareCtrlScreen Dev" --options runtime? ` (do **not** enable hardened runtime unless you need it — it is irrelevant without notarization). |
| Gatekeeper | First launch is blocked: "Apple could not verify…". User: System Settings → Privacy & Security → scroll down → **Open Anyway** → admin password. Or `xattr -dr com.apple.quarantine ShareCtrlScreen.app`. | Sequoia (15) removed the Control-click-Open bypass ([Michael Tsai](https://mjtsai.com/blog/2024/07/05/sequoia-removes-gatekeeper-contextual-menu-override/), [Tietze](https://christiantietze.de/posts/2024/11/running-unsigned-applications-macos-sequoia/)). Put this in the README and in a first-run page. |
| Updates | Replacing the `.app` in place keeps TCC only with the self-signed cert. | Keep bundle id, cert, and install path (`/Applications/ShareCtrlScreen.app`) stable. If the key leaks, anyone can sign an app that inherits your users' Screen Recording grant — keep the `.p12` only in CI secrets + offline backup. |
| Minimum macOS | 13.0 (Ventura) — the `screencapturekit` crate's floor; SCK itself is 12.3+. Set `bundle.macOS.minimumSystemVersion = "13.0"`. | Tauri's default is 10.13. |

## 2. Permissions (TCC) the app needs

| Feature | Permission | How it is granted | Notes |
|---|---|---|---|
| Host: screen capture (ScreenCaptureKit) | **Screen & System Audio Recording** | First `SCShareableContent` call prompts once; afterwards only System Settings. A **refused** app is never prompted again. | **Sequoia+ re-prompts monthly** ("… is requesting to bypass the system private window picker") for every app that captures without the system picker; cannot be disabled by the developer, only MDM-managed grants skip it ([9to5Mac](https://9to5mac.com/2024/08/14/macos-sequoia-screen-recording-prompt-monthly/), [TidBITS](https://tidbits.com/2024/09/23/how-to-avoid-sequoias-repetitive-screen-recording-permissions-prompts/)). Unattended hosts will stop streaming until someone clicks "Allow" — the UI must detect `SCShareableContent` failure and toast it. **Unverified:** whether `SCContentSharingPicker` avoids the nag; it needs a user click per session, so it is unusable for unattended hosts anyway. |
| Host: inject input (`CGEventPost`) | **Accessibility** (`AXIsProcessTrustedWithOptions` prompts) | System Settings → Privacy & Security → Accessibility | Not available inside the App Sandbox (we are not sandboxed). |
| Viewer: swallow Cmd+Tab / Cmd while focused (`CGEventTap`, default tap) | **Accessibility** (+ **Input Monitoring** for listen-only taps on 10.15+) | System Settings | Taps can be silently disabled by the OS when the callback is slow (`kCGEventTapDisabledByTimeout`) — re-enable in the callback. Re-signing changes identity → re-grant ([Raffel 2026](https://danielraffel.me/til/2026/02/19/cgevent-taps-and-code-signing-the-silent-disable-race/)). |
| Clipboard (`NSPasteboard`) | none | | macOS 15.4+ shows a one-time "paste from other app" prompt only for *programmatic reads of another app's pasteboard contents* in sandboxed apps; not applicable. |
| Keychain (saved passwords) | none for the app's own generic-password items | | `keyring` 4.2 (MIT/Apache) → `macos` feature. |
| LaunchAgent autostart | none | `~/Library/LaunchAgents/dev.netameta.sharectrl.plist`, `RunAtLoad`, `ProgramArguments: [exe, --autostart]`. `auto-launch` 0.6 (MIT) does this. | Login Items UI shows it under "Allow in the Background" on 13+. |

Deep links for the preflight UI: `x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture`, `…?Privacy_Accessibility`, `…?Privacy_ListenEvent` (Input Monitoring).

## 3. Capture — ScreenCaptureKit

* Crates: `screencapturekit` 11.1.1 (MIT/Apache, updated 2026-10-26, safe wrapper, macOS 13+)
  **or** the raw `objc2-screen-capture-kit` 0.3.2 (Zlib/Apache/MIT, from the objc2 project —
  the same family Tauri/wry use). Recommendation: **objc2-\*** for capture, VideoToolbox and
  CoreVideo so the whole macOS backend is one binding family with no Swift bridge; use
  `screencapturekit` only as reference code.
* Setup: `SCStreamConfiguration` with `minimumFrameInterval = 1/60` (0 = native refresh),
  `queueDepth` 3–5 (never > 8), `pixelFormat = '420v'` (NV12, lets VideoToolbox skip the
  colour convert) or `BGRA`, `showsCursor = false` (we send the cursor out-of-band, as on
  Windows), `scalesToFit`, `width/height` = the viewer's requested size (the compositor
  scales for free, replacing the Windows `fit_within` re-encode).
* Each `CMSampleBuffer` carries an IOSurface-backed `CVPixelBuffer`; attachments include
  `SCStreamFrameInfoDirtyRects`, `SCStreamFrameInfoContentRect`, `SCStreamFrameInfoStatus`
  (`.idle` → map to `acquire() == Ok(None)`). Dirty rects feed the adaptive-fps logic exactly
  like DDA. Frames must be released back within `minimumFrameInterval × (queueDepth−1)` or
  SCK stalls (WWDC22).
* Cursor: `CGEvent`-free: poll `NSEvent.mouseLocation` + `NSCursor.currentSystem` shape
  (image + hotspot) on the capture thread; convert to BGRA for `CursorShape`.
* Display loss / permission revoked: `SCStreamDelegate.stream(_:didStopWithError:)` → map to
  `CaptureError::Lost` (reinit) or `PermissionDenied`.
* macOS caps SCK at **60 fps for full-screen capture** even on 120 Hz panels (forum report).

## 4. Encode/decode — VideoToolbox

* Crate: `objc2-video-toolbox` 0.3.2 (+ `objc2-core-media`, `objc2-core-video`). All
  generated bindings; constants like `kVTVideoEncoderSpecification_EnableLowLatencyRateControl`
  and `kVTCompressionPropertyKey_*` exist in `VTCompressionProperties.rs`.
* Encoder recipe (mirrors the MF recipe in `codec/src/encode.rs:665-718`):
  `VTCompressionSessionCreate` with encoder spec `EnableLowLatencyRateControl = true`,
  `RequireHardwareAcceleratedVideoEncoder = true`; properties `RealTime = true`,
  `AllowFrameReordering = false` (no B-frames), `ProfileLevel = H264_Main_AutoLevel`,
  `MaxKeyFrameInterval = huge` + `MaxKeyFrameIntervalDuration = 0` (keyframes on demand via
  `kVTEncodeFrameOptionKey_ForceKeyFrame`), `AverageBitRate` + `DataRateLimits` (CBR-ish;
  update live with `VTSessionSetProperty`), `MaxAllowedFrameQP` (macOS 13+, = our MaxQP 32
  floor), `PrioritizeEncodingSpeedOverQuality = true`, `ExpectedFrameRate = 60`.
  Low-latency RC fails with `-12902` on some Intel Macs for HEVC → fall back to a session
  without the key ([Sunshine #5977](https://github.com/LizardByte/Sunshine/issues/5977),
  [Renderd PR 159](https://github.com/Ad1th/Renderd/pull/159)). Measured effect in one
  report: p95 encode 40 ms → 10 ms ([appium PR 96](https://github.com/appium/appium-ios/pull/96)).
* Output is AVCC (length-prefixed) with parameter sets in the format description → convert
  to Annex-B and prepend SPS/PPS on keyframes (the transport expects Annex-B,
  `transport/src/lib.rs:191`).
* Capability: H.264 + HEVC encode on every Apple Silicon and most Intel Macs with T2/Intel
  QuickSync; **AV1: decode only, M3 and later; no AV1 encode on any Mac** →
  `host_encodable = [HEVC, H264]`, `viewer_decodable = [AV1 if M3+, HEVC, H264]`.
* Decoder: `VTDecompressionSession` with `kVTDecompressionPropertyKey_RealTime`, output
  `kCVPixelBufferPixelFormatTypeKey = 420v` + `kCVPixelBufferMetalCompatibilityKey = true`;
  AV1 needs an `av1C` atom in the format description extensions ([Apple forum](https://developer.apple.com/forums/thread/722933)).
* Zero-copy present: `CVMetalTextureCacheCreateTextureFromImage` per plane (`R8Unorm`,
  `RG8Unorm` half-size) → `MTLTexture` → `wgpu::Device::create_texture_from_hal`
  (`wgpu-hal::metal::Device::texture_from_raw`); keep the `CVMetalTexture` alive until the
  frame is presented ([moq-video](https://docs.rs/crate/moq-video/latest/source/src/render/metal.rs),
  [frust-gpu](https://docs.rs/crate/frust-gpu/0.6.2/source/src/import/apple.rs)).

## 5. Input injection and shortcut capture

* `objc2-core-graphics` 0.3.2 (`CGEvent`, `CGEventPost(kCGHIDEventTap)`,
  `CGEventCreateKeyboardEvent(kVK)`, `CGEventCreateMouseEvent`, `CGEventCreateScrollWheelEvent`).
  Coordinates are **points in the global display space with origin top-left of the main
  display** for `CGEvent` (not flipped like AppKit). `TargetRect` comes from
  `CGDisplayBounds` × backing scale.
* Mark our own events with `CGEventSetIntegerValueField(kCGEventSourceUserData, SENTINEL)`
  so the viewer's tap ignores them (same trick as `INJECT_SENTINEL`).
* `focus_window_at`: `CGWindowListCopyWindowInfo(onScreenOnly)` → first window whose bounds
  contain the point → `NSRunningApplication(pid).activate()` (no click). Needs Accessibility
  for cross-app activation reliability.
* Shortcut hook: `CGEventTapCreate(kCGSessionEventTap, kCGHeadInsertEventTap, defaultTap,
  keyDown|keyUp|flagsChanged)`; return `NULL` to swallow Cmd+Tab/Cmd+Space/Cmd+\` while our
  window is key; requires Accessibility (several open-source Cmd+Tab replacements do exactly
  this). The tap runs on a `CFRunLoop` on the hook thread (`run(stop)`).
* Keymap: macOS `kVK_*` are positional; generate from Chromium's table (task D3).

## 6. Clipboard, secrets, autostart, paths

| Need | Implementation | Crate |
|---|---|---|
| Text + change counter | `NSPasteboard.general.changeCount` (monotonic, bumps on our write — matches the echo-guard contract) | `objc2-app-kit` or `arboard` 3.6 (MIT/Apache; no changeCount API → use objc2 for that one call) |
| File list for paste | `writeObjects([NSURL fileURL])` → Finder/Mail/Slack accept | `objc2-app-kit` |
| Saved passwords | Keychain generic password, service `dev.netameta.sharectrl` | `keyring` 4.2 |
| Autostart | LaunchAgent plist, `--autostart` flag preserved | `auto-launch` 0.6 |
| Dirs | `~/Library/Application Support/ShareCtrlScreen`, logs `~/Library/Logs/ShareCtrlScreen`, `~/Downloads/ShareCtrlScreen` | `dirs` 6 |

## 7. The video surface (see 04 §4)

`WebviewWindow::ns_view()` returns the window's content `NSView` (`tauri-2.11.5
webview_window.rs:1841`). Add our `NSView` subclass (layer-backed, `CAMetalLayer`) as a
sibling **above** the `WKWebView`. Tauri's transparent-webview private API is **not** needed
(it is also unreliable in bundled builds per issue #13415). Hover-bar reveal: shrink the
view's frame from the top (keeps `client_size()` from the stored full size).

## 8. Things that will bite

* **The TCC prompt appears only once.** If a tester clicks "Don't Allow", only
  `tccutil reset ScreenCapture dev.netameta.sharectrl` (or System Settings) brings it back.
* **Every self-signed-cert change = every user re-grants.** Treat the `.p12` like a
  production key.
* **Monthly Sequoia re-approval** can silently break an unattended host; detect and toast.
* **Retina:** capture is in pixels (SCK `width/height`), input in points; `TargetRect` must
  carry the backing scale per display.
* **Secure Input** (password fields, iTerm "Secure Keyboard Entry") blocks event taps
  system-wide; detect `IsSecureEventInputEnabled()` and show why shortcut capture is off.
* **Sleep/display off:** SCK stops with an error; `reinit()` must retry with backoff.

## Sources (beyond those inline)

* Tauri signing/ad-hoc: [tauri commit 2a06d10](https://github.com/tauri-apps/tauri/commit/2a06d10066a806e392efe8bfb16d943ee0b0b61d), [tauri commit 9dac286](https://github.com/tauri-apps/tauri/commit/9dac2863afa70fb0bcddf859b284afba917f28ae), [phase-rs PR 5605](https://github.com/phase-rs/phase/pull/5605)
* Cross-compile status: [tauri issue #16340](https://github.com/tauri-apps/tauri/issues/16340), [PR #16342](https://github.com/tauri-apps/tauri/pull/16342)
* Runner TCC: [runner-images #8951](https://github.com/actions/runner-images/issues/8951), [#1567](https://github.com/actions/runner-images/issues/1567)
* Scaleway VNC: [Scaleway remote desktop](https://www.scaleway.com/en/docs/apple-silicon/how-to/access-remote-desktop-mac-mini/), [Reemo tutorial (TCC via GUI)](https://www.scaleway.com/en/docs/tutorials/access-mac-mini-with-reemo.md)
* SCK: [minimumFrameInterval](https://developer.apple.com/documentation/screencapturekit/scstreamconfiguration/minimumframeinterval.md), [queueDepth](https://developer.apple.com/documentation/screencapturekit/scstreamconfiguration/queuedepth.md), [WWDC22](https://developer.apple.com/videos/play/wwdc2022/10155/), [openstream PR 76 (zero-copy SCK→VT)](https://github.com/ankitpipalia/openstream/pull/76)
* VT low latency: [Apple doc](https://developer.apple.com/documentation/videotoolbox/kvtvideoencoderspecification_enablelowlatencyratecontrol), [WWDC21](https://developer.apple.com/videos/play/wwdc2021/10158/), [ffmpeg patch (HEVC LL on Apple Silicon)](https://www.mail-archive.com/ffmpeg-devel@ffmpeg.org/msg185545.html)
* Event taps: [tauri-plugin-macos-input-monitor](https://github.com/yigitkonur/tauri-plugin-macos-input-monitor), [HackTricks TCC overview](https://hacktricks.wiki/en/macos-hardening/macos-security-and-privilege-escalation/macos-security-protections/macos-input-monitoring-screen-capture-accessibility.html)
* VM legality: [Apple community](https://discussions.apple.com/thread/250646417), [HackMD summary](https://hackmd.io/@Ronnieftd/rJGgE9AlGg)
