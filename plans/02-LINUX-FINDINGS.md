# 02 — Linux findings

Researched 2026-10-31. Linux is two platforms: **X11** (simple, fully featured, no
permission dialogs) and **Wayland** (GNOME/KDE default since ~2022; everything goes through
`xdg-desktop-portal` + PipeWire with user consent dialogs). Both must ship; the backend
picks at runtime from `WAYLAND_DISPLAY` / `XDG_SESSION_TYPE`.

## TL;DR

* **Nothing needs an account, a signature, or a store.** Ship an AppImage + .deb (+ .rpm)
  from Tauri's bundler on `ubuntu-22.04` (oldest glibc you want to support).
* **Your WSL2 Ubuntu 24.04 already compiles the workspace** (verified today after
  installing Tauri's GTK deps): only two errors remain, both expected —
  `engine/src/secret.rs` (DPAPI, 2× `windows::` paths, ungated) and `service` (no `main`
  because of `#![cfg(windows)]`). See [03-TESTING-AND-MACHINES.md](03-TESTING-AND-MACHINES.md).
* **Wayland host = portal + PipeWire (capture) and portal RemoteDesktop/libei (input).**
  Unattended re-connects need the portal **restore token** (`persist_mode = 2`), supported
  by GNOME ≥ 45/46 and KDE Plasma 6, with known KDE bugs; wlroots (Sway, Hyprland) has
  ScreenCast but Hyprland's portal has no RemoteDesktop yet → view-only there unless the
  user enables the **uinput fallback** (udev rule).
* **Hardware codecs:** VA-API via `cros-libva` (BSD-3) covers Intel/AMD; NVIDIA via NVENC
  bindings (MIT) or NVIDIA's VA-API shim. Software H.264 fallback (Cisco OpenH264 runtime
  download) is mandatory for VMs/CI/odd GPUs.
* **The viewer surface is the risky part** on Wayland (needs a `wl_subsurface` under the
  GTK/WebKitGTK window — spike first). X11 is a straight port of the Windows child-window
  design.

## 1. Capture

### Wayland — xdg-desktop-portal ScreenCast → PipeWire
* Crates: `ashpd` 0.13.13 (MIT, 2026-07) for the portal D-Bus dance (`ScreenCast::create_session`,
  `select_sources(types=MONITOR, cursor_mode=METADATA|HIDDEN, persist_mode=ExplicitlyRevoked,
  restore_token)`, `start` → returns `restore_token` + PipeWire node id, `open_pipe_wire_remote`
  fd); `pipewire` 0.10.1 (MIT, 2026-08, needs `libpipewire-0.3-dev`) for the stream. Use
  ashpd **without** its `pipewire` feature (it pins 0.9 and conflicts) — projects in the wild
  do exactly this.
* Buffer negotiation: offer `DmaBuf` (with modifiers, `SPA_DATA_DmaBuf`) first and `MemFd`/
  `MemPtr` as fallback; several projects shipped MemFd-only first because DMA-BUF modifier
  negotiation against real compositors is the least-validated path (and one Hyprland report
  shows the portal wedging on DMA-BUF failures). Do the same: **MemFd first, DMA-BUF as the
  zero-copy stretch task**.
* Cursor: `cursor_mode = METADATA` gives position + bitmap in `spa_meta_cursor` per buffer —
  maps directly to `CursorUpdate`. Dirty regions: `spa_meta_video_damage` (optional; not all
  compositors emit it; absent → treat the whole frame as dirty, adaptive fps still works via
  "no new buffer").
* Persistence: store the token at `$XDG_STATE_HOME/sharectrl/portal.token` (0600). The token
  is single-use: each `start` returns a new one — always overwrite. Restore can still fail
  on KDE (bugs 480235, TigerVNC report on Plasma 6.7.2 "non-UUID token rejected after
  service restart"); on failure the user sees the normal dialog once.
* Compositor matrix: GNOME (`xdg-desktop-portal-gnome`), KDE (`-kde`), wlroots (`-wlr`,
  needs `portals.conf` with `ScreenCast=wlr`), Hyprland (`-hyprland`). CI: headless
  `sway` or `mutter --headless --virtual-monitor` + `xdg-desktop-portal-wlr`/`-gnome`.
* Screen-share indicator: GNOME shows an orange "screen is being shared" icon the user can
  stop from → map stream `closed` to `CaptureError::Lost`, then re-run the portal flow.

### X11 — XShm + XDamage
* `x11rb` 0.14 (MIT/Apache) with features `shm`, `damage`, `xfixes`, `randr`, `xtest`,
  `shape`. Loop: `XDamageCreate(root, DamageReportNonEmpty)` → on `DamageNotify` collect
  rects (`XFixesCreateRegion`/`XDamageSubtract`) → `XShmGetImage` of the root (or only the
  dirty bounding box) into a shared segment → `GpuFrame::Cpu(Bgra8)`. Cursor: `XFixesGetCursorImage`
  (ARGB + hotspot) + `XQueryPointer`. No permission needed. `xcap` 0.9.8 (Apache-2.0) is an
  existing X11 screenshot crate usable as reference (its Wayland path is unimplemented).
* Under XWayland (e.g. your WSLg `:0`), root capture of an *rootless* Xwayland gives
  garbage — the backend must refuse X11 capture when `WAYLAND_DISPLAY` is set and use the
  portal.

## 2. Hardware encode / decode

| GPU | Encode | Decode | Crates / notes |
|---|---|---|---|
| Intel / AMD (and NVIDIA via `nvidia-vaapi-driver` shim, decode only) | **VA-API** H.264/HEVC/AV1(Arc, RDNA3+) | VA-API | `cros-libva` 0.0.13 (BSD-3, ChromeOS, needs libva ≥ 1.20, `libva-dev`) low-level; `cros-codecs` 0.0.6 (BSD-3) higher-level with VA-API H.264/VP9/AV1 encoders and H.264/HEVC/VP8/VP9/AV1 decoders — evaluate in the spike; worst case write the encode pipeline on `cros-libva` directly (Chromium does). Zero-copy in: `vaCreateSurfaces` with `VASurfaceAttribMemoryType = DRM_PRIME_2` importing the PipeWire DMA-BUF (iHD rejects RGB with size 0 — pass real sizes); VPP (`vaBeginPicture` on a VPP context) BGRA→NV12. |
| NVIDIA | **NVENC** via `libnvidia-encode.so.1` (driver-provided) | NVDEC via `libnvcuvid.so.1` | `nvidia-video-codec-sdk` 0.4 (MIT; links the driver libs, needs SDK headers at build via `NVIDIA_VIDEO_CODEC_SDK_PATH`, depends on `cudarc`). Alternative: a ~400-line `dlopen` binding over the MIT `nv-codec-headers` (what ffmpeg does) — no CUDA toolkit needed at build; recommended so the NVIDIA path is optional at runtime. Input: CUDA device pointer or (zero-copy) `NV_ENC_INPUT_RESOURCE_TYPE_OPENGL_TEX`/Vulkan via `cuImportExternalMemory` from a DMA-BUF. v1: CPU upload. |
| Any (future) | Vulkan Video `VK_KHR_video_encode_h264` | `VK_KHR_video_decode_h264` | Mesa RADV (24.1+, `RADV_PERFTEST=video_encode`), ANV (Mesa 26.2 re-enabled Alchemist only), NVIDIA (shipping). One API for all vendors *and* Windows, via `ash` (MIT). Not mature enough to be the v1 path; keep as the unification task. |
| None / VM / CI | software H.264 (Cisco OpenH264 runtime download) | openh264 decoder | `openh264` 0.9.9 crate, `libloading` feature. See 04 §5 for the licence conditions. |

Capability probing at startup: `vaInitialize` on `/dev/dri/renderD128` → `vaQueryConfigProfiles`
/ `vaQueryConfigEntrypoints(VAEntrypointEncSliceLP|EncSlice)`; NVENC: `dlopen` +
`NvEncodeAPIGetMaxSupportedVersion`. Both are fast and need no display.

## 3. Input injection

| Session | Primary | Notes |
|---|---|---|
| X11 | **XTest** (`x11rb` `xtest` feature): `XTestFakeMotionEvent` (absolute, root coords across RandR), `XTestFakeButtonEvent` (buttons 1/2/3, wheel = buttons 4/5/6/7), `XTestFakeKeyEvent(keycode = evdev + 8)` | No permission. `focus_window_at`: `XQueryPointer` → `_NET_ACTIVE_WINDOW` client message (no click). Key auto-repeat: inject repeats ourselves (as today). |
| Wayland GNOME ≥ 46 / KDE 6 | **portal RemoteDesktop v2 + libei**: `ashpd` RemoteDesktop (`select_devices(KEYBOARD|POINTER, persist_mode, restore_token)`, `start`, `connect_to_eis`) → `reis` 0.7.1 (MIT, pure-Rust libei) sender context: `NotifyPointerMotionAbsolute`, `NotifyPointerButton`, `NotifyPointerAxis`, `NotifyKeyboardKeycode(evdev)` | One combined ScreenCast+RemoteDesktop session gives capture **and** input with one consent dialog and one restore token ("Allow remote control?"). KDE restore has known bugs (see above). `wdotool` is a working Rust reference (caches token at `$XDG_STATE_HOME/wdotool/portal.token`). |
| Wayland wlroots / Hyprland | `wlr-virtual-pointer-unstable-v1` + `zwp-virtual-keyboard-unstable-v1` (`wayland-client`/`wayland-protocols-wlr`) | No dialog; only on wlroots-based compositors. |
| Any Linux (fallback, user opt-in) | **uinput** virtual mouse+keyboard (`input-linux` or `mouse-keyboard-input`, MIT) | Needs `/dev/uinput` access: udev rule `KERNEL=="uinput", GROUP="input", MODE="0660"` + user in `input` group, or a polkit/pkexec one-time setup from the Settings panel. Absolute pointer: create the device with `ABS_X/ABS_Y` (0..65535) so multi-monitor mapping works. Works on every compositor including login-less setups. |
| focus_window_at on Wayland | **not possible** generically (no global window list / activation for third parties; `xdg-activation` needs a token from the focused app) | Dropped file stays on the clipboard with a toast — the code already handles `focus_window_at == false`. |

Keymap: `KeyId` → evdev code straight from the Chromium table; evdev main block equals
Set-1 so the existing scancode table is already 80 % of it.

## 4. Shortcut capture (viewer side)

* X11: `XGrabKeyboard` on our video child window while it has focus (swallows Alt+Tab and
  Super for the session; the WM never sees them). Release on focus-out (escape hatch like
  Windows "click away").
* Wayland: `zwp_keyboard_shortcuts_inhibit_manager_v1` on the toplevel surface — supported by
  GNOME, KDE, wlroots; the compositor keeps one escape combo and may show a prompt; GTK3's
  Wayland backend only triggers this for toplevel keyboard grabs (it guesses), so call the
  protocol directly via `wayland-client` on the GTK surface. Super-key-opens-Activities on
  GNOME is still swallowed by the compositor in some versions → report `ShortcutHook::install
  == false` when the inhibitor is refused and show the existing "unsupported here" hint.

## 5. Clipboard, secrets, autostart, paths

| Need | Implementation | Crate |
|---|---|---|
| Text | `arboard` 3.6 (MIT/Apache; X11 + Wayland via `wl-clipboard-rs`). **No change counter on Linux**: implement `change_count()` as a content hash of `get_text()` polled every 300 ms (the loop already polls at 300 ms) and make `set_text` return the hash it just wrote → echo guard still holds. | `arboard` |
| File list | X11: own the CLIPBOARD selection with targets `text/uri-list` + `x-special/gnome-copied-files` (`copy\nfile:///…`) + `application/x-kde-cutselection`; Wayland: same MIME set via `wl-clipboard-rs`. **The selection dies with the process** on Linux → keep the clipboard thread alive for the session (it already is). | `x11rb` / `wl-clipboard-rs` |
| Saved passwords | Secret Service (GNOME Keyring / KWallet via `org.freedesktop.secrets`); if no daemon → return `None` (no save) — never a plaintext file | `keyring` 4.2 (`sync-secret-service` feature, needs `libdbus-1-dev`) |
| Autostart | `~/.config/autostart/sharectrl.desktop` with `Exec="<path>" --autostart` | `auto-launch` 0.6 |
| Dirs | `$XDG_CONFIG_HOME/sharectrl`, `$XDG_STATE_HOME/sharectrl/app.log`, `$XDG_DOWNLOAD_DIR/ShareCtrlScreen` | `dirs` 6 |

## 6. The video surface (see 04 §4)

* X11: child window of the GTK toplevel's XID, above WebKitGTK; wgpu (Vulkan, GL fallback
  for VMs) renders into it; `XShape` for the hover-bar clip. Proven by wry's `wgpu.rs`
  example (which panics on Wayland — hence the spike).
* Wayland: `wl_subsurface` of the GTK toplevel surface, `place_above`, desynchronized;
  wgpu surface from `RawWindowHandle::Wayland{surface}` + `RawDisplayHandle::Wayland`.
  Pointer input arrives on the subsurface; keyboard via `gtk_window().connect_key_press_event`
  returning `Inhibit(true)` during a session. Fractional scaling: use
  `wp_fractional_scale_v1` + `wp_viewport` so the subsurface is pixel-exact.
* WebKitGTK transparency/DMABUF-renderer bugs on NVIDIA (several Tauri issues) don't affect
  us because the webview stays opaque; the video is a separate surface above it.

## 7. Packaging and runtime dependencies

* Tauri 2 Linux prerequisites (build host): `libwebkit2gtk-4.1-dev build-essential curl wget
  file libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev` (+ ours:
  `libpipewire-0.3-dev libva-dev libdbus-1-dev libxkbcommon-dev`). Runtime (.deb `Depends`):
  `libwebkit2gtk-4.1-0, libgtk-3-0, libpipewire-0.3-0, libva2, libva-drm2, libxkbcommon0`.
  Build on Ubuntu 22.04 for glibc 2.35 compatibility; AppImage bundles everything except
  glibc and the GPU/VA drivers.
* GPU drivers come from the distro: `intel-media-va-driver-non-free` / `mesa-va-drivers`
  (AMD) / NVIDIA proprietary. Document `vainfo` as the diagnostic.
* No service/daemon for v1 (see 04 §9). A `systemd --user` unit is optional for "start at
  login" (the XDG autostart file is enough).

## Sources

* ashpd/pipewire field reports: [FOLO PR 49](https://github.com/D-pranoy/FOLO/pull/49), [linux-quest PR 55](https://github.com/Josh-Archer/linux-quest/pull/55), [A-Eye PR 18](https://github.com/EvanGUTK/A-Eye/pull/18), [twicecat #22](https://github.com/ga-lep/twicecat/issues/22), [xdph DMA-BUF wedge #434](https://github.com/hyprwm/xdg-desktop-portal-hyprland/issues/434), [lamco-pipewire](https://crates.io/crates/lamco-pipewire)
* RemoteDesktop/libei: [portal spec](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html), [persistent sessions PR 1004](https://github.com/flatpak/xdg-desktop-portal/pull/1004), [KDE bug 480235](https://bugs.kde.org/show_bug.cgi?id=480235), [TigerVNC token report](https://groups.google.com/g/tigervnc-users/c/cdv-YPjQvkU/m/xmdZsqwFAwAJ), [wdotool](https://github.com/iTrooz/wdotool), [libei docs](https://libinput.pages.freedesktop.org/libei/api/index.html), [xdg-desktop-portal-generic](https://docs.rs/xdg-desktop-portal-generic), [linux-quest backends #79](https://github.com/Josh-Archer/linux-quest/issues/79), [XTEST PR 86](https://github.com/Josh-Archer/linux-quest/pull/86)
* VA-API/NVENC/Vulkan: [cros-libva](https://github.com/chromeos/cros-libva), [cros-codecs](https://crates.io/crates/cros-codecs), [moq-vaapi dmabuf.rs](https://docs.rs/crate/moq-vaapi/0.1.0/source/src/dmabuf.rs), [nvidia-video-codec-sdk](https://github.com/ViliamVadocz/nvidia-video-codec-sdk), [Vulkan video status (Igalia)](https://blogs.igalia.com/vjaquez/vulkan-video-status/), [RADV encode](https://www.phoronix.com/news/RADV-Vulkan-VIdeo-H265-H264), [ANV encode](https://www.phoronix.com/news/Intel-ANV-Gen125-H265-Encode)
* Shortcut inhibit: [protocol](https://cgit.freedesktop.org/wayland/wayland-protocols/tree/unstable/keyboard-shortcuts-inhibit/keyboard-shortcuts-inhibit-unstable-v1.xml), [GTK3 grab prompt thread](https://mail.gnome.org/archives/desktop-devel-list/2018-January/msg00053.html), [wlroots #1817](https://github.com/swaywm/wlroots/issues/1817)
* Surface/Tauri on Linux: [wry wgpu example](https://github.com/tauri-apps/wry/blob/dev/examples/wgpu.rs), [cade CANVAS-001](https://github.com/a11smiles/cade/issues/38), [tauri #15050 (Sway blank)](https://github.com/tauri-apps/tauri/issues/15050), [NVIDIA transparent #320](https://github.com/gustavosett/Windows-11-Clipboard-History-For-Linux/issues/320)
* Tauri Linux packaging: [prerequisites](https://v2.tauri.app/start/prerequisites/), [Debian](https://v2.tauri.app/distribute/debian/), [AppImage glibc discussion](https://github.com/orgs/tauri-apps/discussions/10026), [RHEL compat #9039](https://github.com/tauri-apps/tauri/issues/9039)
* CI compositors: [DeskShare Wayland e2e](https://github.com/zcloudcz/DeskShare/pull/13), [xdg-desktop-portal-wlr](https://github.com/emersion/xdg-desktop-portal-wlr), [waydriver (headless mutter)](https://docs.rs/crate/waydriver/latest), [mutter headless MR](https://gitlab.gnome.org/GNOME/mutter/-/merge_requests/1698), [pygobject CI guide](https://pygobject.gnome.org/guide/testing.html)
* Chromium keycode table: [dom_code_data.inc](https://chromium.googlesource.com/chromium/src/+/lkgr/ui/events/keycodes/dom/dom_code_data.inc)
