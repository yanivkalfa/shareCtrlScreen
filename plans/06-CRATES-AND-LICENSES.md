# 06 — Crates, versions, licences (verified 2026-10-31 against crates.io)

All fit the existing `deny.toml` allow-list (MIT / Apache-2.0 / BSD / Zlib / MPL-2.0).
`deny.toml [graph].targets` and `about.toml` must add `aarch64-apple-darwin`,
`x86_64-apple-darwin`, `x86_64-unknown-linux-gnu` (task T1) or the gate silently skips
every crate below.

## Shared

| Crate | Version | Licence | Used for |
|---|---|---|---|
| `wgpu` + `wgpu-hal` | **30** (30.0.1, pinned by M4; MSRV 1.87) | MIT OR Apache-2.0 | presenter on macOS (Metal) and Linux (Vulkan/GL); `create_texture_from_hal` for zero-copy import. Shared shader/renderer in `crates/surface-wgpu` |
| `pollster` | 0.4 | MIT OR Apache-2.0 | block on wgpu's adapter/device futures (presenter setup, headless tests) |
| `naga` (dev only) | 30 | MIT OR Apache-2.0 | GPU-less WGSL validation test in `surface-wgpu` |
| `raw-window-handle` | 0.6 (what Tauri 2.11 re-exports) | MIT/Apache/Zlib | surface parent handles |
| `openh264` | 0.9.9 | BSD-2 (wrapper) / BSD-2 (lib) | software H.264, `libloading` feature only (Cisco binary downloaded at runtime; see 04 §5) |
| `keyring` | 4.2.0 | MIT OR Apache-2.0 | Keychain / Secret Service (`macos`, `sync-secret-service` features) |
| `arboard` | 3.6.1 | MIT OR Apache-2.0 | clipboard text on macOS/Linux (X11 + Wayland) |
| `auto-launch` | 0.6.0 | MIT | LaunchAgent / XDG autostart (Windows Run key already implemented in-tree) |
| `dirs` | 6 | MIT OR Apache-2.0 | config/log/downloads dirs |
| `keycode` table source | Chromium `dom_code_data.inc` | BSD-3-Clause (Chromium) | generated keymap (task D3); keep the copyright header |

## macOS (`objc2` family, all `Zlib OR Apache-2.0 OR MIT`, from the project Tauri/wry already use)

| Crate | Version | Used for |
|---|---|---|
| `objc2`, `objc2-foundation`, `objc2-app-kit` | 0.6.x / 0.3.2 | NSView subclass, NSPasteboard, NSRunningApplication, NSEvent |
| `objc2-screen-capture-kit` | 0.3.2 (2025-10-04) | SCStream capture |
| `objc2-core-media`, `objc2-core-video` | 0.3.2 | CMSampleBuffer, CVPixelBuffer, CVMetalTextureCache |
| `objc2-video-toolbox` | 0.3.2 (2025-10-04) | VTCompressionSession / VTDecompressionSession (`kVTVideoEncoderSpecification_EnableLowLatencyRateControl` present in generated `VTCompressionProperties.rs`) |
| `objc2-core-graphics` | 0.3.2 | CGEvent, CGEventTap, CGDisplay, CGWindowList, CGPreflightScreenCaptureAccess |
| `objc2-metal`, `objc2-quartz-core` | 0.3.2 | CAMetalLayer, MTLTexture import |
| `block2`, `dispatch2` | 0.6 / 0.3 | blocks for `NSEvent` local monitors; main-queue hops (`DispatchQueue::main`, `MainThreadBound`) — same objc2 project, `block2` is MIT, `dispatch2` Zlib OR Apache-2.0 OR MIT |
| `screencapturekit` (reference only) | 11.1.1 (MIT OR Apache-2.0, 2026-10-26, macOS 13+) | safe wrapper to crib from; not a dependency (Swift bridge) |
| `videotoolbox` (reference only) | docs.rs | shows the encoder-spec builder shape |
| `sha2`, `hex` (dev only) | workspace versions (`sha2` 0.10, `hex` 0.4) | SHA-256 of decoded NV12: platform-macos's frame hash in the VideoToolbox golden test (`decode::tests`, task T2), and `crates/codec-goldens` (task I5), the in-tree dev-only workspace crate behind every decoder's golden matrix (only `sha2` + `hex`, no new third-party dependency). Already in the workspace via protocol and engine; platform-linux no longer lists them (it uses `codec-goldens`) |

## Linux

| Crate | Version | Licence | Used for |
|---|---|---|---|
| `ashpd` | 0.13.13 (2026-07-17) | MIT | portal ScreenCast + RemoteDesktop (+ restore tokens); **do not** enable its `pipewire` feature |
| `pipewire` (pipewire-rs) | 0.10.1 (2026-08-19) | MIT | PipeWire stream (needs `libpipewire-0.3-dev` ≥ 0.3.x) |
| `libspa` (pipewire-rs, vendored) | 0.10.1, patched | MIT (unchanged) | dependency of `pipewire`. Vendored at `native/vendor/libspa` and applied with `[patch.crates-io]` (source = path; excluded from the workspace): upstream uses symbols that PipeWire 0.3.48 (Ubuntu 22.04) lacks, so it effectively needs 0.3.65. Three sites patched, PipeWire commits cited in its `src/lib.rs`. No new crates. Drop the patch once an upstream release builds on 0.3.48 |
| `reis` | 0.7.1 (2026-07-30) | MIT | pure-Rust libei client for input via the RemoteDesktop portal |
| `x11rb` | 0.14.0 (2026-07-16) | MIT OR Apache-2.0 | X11 capture (`shm`, `damage`, `xfixes`, `randr`), input (`xtest`), clip (`shape`), window child |
| `cros-libva` | 0.0.13 (2024-12-06) | BSD-3-Clause | VA-API bindings (libva ≥ 1.20) |
| `cros-codecs` | 0.0.6 (2025-06-18) | BSD-3-Clause | VA-API H.264/VP9/AV1 encoders, H.264/HEVC/VP8/VP9/AV1 decoders — evaluate in L3/L4; low release cadence, consider vendoring if API churn bites |
| `nvidia-video-codec-sdk` | 0.4.0 | MIT | NVENC/NVDEC bindings (needs SDK headers at build, `cudarc`); alternative: in-tree `dlopen` binding over MIT `nv-codec-headers` |
| `wayland-client`, `wayland-protocols`, `wayland-protocols-wlr` | 0.31+ | MIT | subsurface, shortcuts-inhibit, fractional-scale, wlr virtual input |
| `wayland-backend` | 0.3 (already transitive via wayland-client) | MIT | `client_system` feature: libwayland-client backend, needed to join GDK's `wl_display` (`from_foreign_display`; L5, L7) |
| `wayland-protocols-misc` | 0.3 (approved for L6, 2026-11-01) | MIT | `zwp_virtual_keyboard_v1` (wlroots virtual keyboard) lives here, not in `-wlr`; same Smithay family |
| `input-linux` / `mouse-keyboard-input` | 0.7 / 0.9 | MIT | uinput fallback (L6 uses `input-linux`; pulls `nix` 0.29, MIT) |
| `rustix` | 1 (approved for L6, 2026-11-01) | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | `CLOCK_MONOTONIC` for EI frame timestamps, `memfd_create` for the default keymap; already in the graph through `x11rb` and `reis`, so no new crate |
| `wl-clipboard-rs` (via arboard) | — | MIT | Wayland clipboard |
| `gtk`, `gdkx11`, `gdkwayland-sys` (gtk-rs) | 0.18 (the versions tauri 2.11 locks; another minor would load a second gtk-rs type registry) | MIT (bindings; GTK itself is the system's LGPL library, dynamically linked as Tauri already does) | video surface (L5): find the app window's GTK toplevel, follow its layout, take keys from WebKit, XID / `wl_surface` of GDK windows |
| `wayland-backend` (`client_system`), `wayland-sys` (`client`) | 0.3 / 0.31 (already in the tree via wl-clipboard-rs) | MIT | L5: share GDK's `wl_display` (libwayland guest mode) and create the video `wl_surface` with GDK-safe user data |
| `x11` (dev only, `xlib` feature) | 2.21 (already in the tree via gdkx11/tao) | MIT | L5 test: `tests/x11_surface.rs` opens its own Xlib `Display` for the window handle, as tao does for the app |
| `glow`, `khronos-egl` (via wgpu `gles`) | as resolved by wgpu 30 | MIT OR Apache-2.0 (glow also Zlib) | L5: wgpu's GL/EGL fallback for machines without Vulkan |
| `xcap` (reference only) | 0.9.8 (2026-08-01) | Apache-2.0 | X11 screenshot code to crib from; Wayland unimplemented |
| `ash` (future) | 0.38+ | MIT OR Apache-2.0 | Vulkan Video unification (not v1) |

Banned/avoid (already in `deny.toml` or incompatible): `scrap`/`hwcodec` (RustDesk, AGPL /
no licence), `x264`/`x265` (GPL), ffmpeg/`ffmpeg-next` (LGPL/GPL builds, and it would drag
x264 in), GStreamer bindings (LGPL). `libdbus` is MIT/AFL (fine; already in the Tauri tree).

## Software-codec legal note (04 §5)

Cisco's OpenH264 **binary** is royalty-covered only when "separately downloaded to an end
user's device", not combined with the app before it reaches the user; the app must show
"OpenH264 Video Codec provided by Cisco Systems, Inc." and let users enable/disable it
([BINARY_LICENSE.txt](https://www.openh264.org/BINARY_LICENSE.txt)). Building from source
(the crate's default `source` feature) leaves H.264 patent exposure with the distributor —
many H.264 patents have expired but Via LA says most essential ones have not (last US
expiry estimates ~2027). Royalty-free alternatives if you ever want to drop the download:
SVT-AV1 (BSD-3-Clear, `svt-av1-enc`), dav1d (BSD-2, `dav1d` crate), libvpx VP8/VP9 (BSD-3,
`vpx-rs`/`shiguredo_libvpx` 2026.1.0).
