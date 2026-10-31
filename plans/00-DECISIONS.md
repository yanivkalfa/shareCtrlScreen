# 00 — Locked decisions (2026-10-31)

Guiding rule from the owner: **it should feel like AnyDesk.** When a task card is ambiguous,
pick the behaviour closest to AnyDesk on that OS.

| # | Decision |
|---|---|
| 1 | Software codec fallback = **Cisco OpenH264 binary downloaded at runtime from Cisco** (never bundled). Required by Cisco's licence: the notice "OpenH264 Video Codec provided by Cisco Systems, Inc." and a user enable/disable toggle — put it under Settings → Advanced, on by default. Used only when no hardware codec is available. |
| 2 | Renderer: **wgpu** on macOS (Metal) and Linux (Vulkan, GL fallback), one WGSL shader. **Windows stays D3D11.** |
| 3 | Video surface = native child surface inside the app window on every OS (AnyDesk layout). On Wayland this needs the `wl_subsurface` spike (3-day box). **Pre-authorized exit:** if the spike fails, Wayland alone opens the remote screen in a separate undecorated window; do not stop to ask. |
| 4 | Linux input: XTest (X11) + portal RemoteDesktop/libei (GNOME/KDE Wayland) + wlroots virtual input + **opt-in uinput** fallback with a udev-rule installer in Settings. |
| 5 | NVIDIA on Linux: in-tree `dlopen` binding for NVENC/NVDEC (optional feature, no CUDA toolkit at build). |
| 6 | macOS **13 Ventura minimum, universal build** (arm64 + x86_64). |
| 7 | Linux floor **Ubuntu 22.04** (Tauri 2 needs WebKitGTK 4.1; 20.04 is impossible). Bundles: AppImage + deb + rpm, built on ubuntu-22.04. **libva is loaded dynamically**; libva < 2.20 (22.04 ships 2.14) → hardware path off, software codec used. |
| 8 | Lock screen with user logged in: **in scope** on all OSes. Login window with nobody logged in: Windows only (SYSTEM service), excluded on macOS/Linux — same as AnyDesk/TeamViewer on Mac. UI states this. |
| 9 | macOS signing: **self-signed certificate** (owner creates it; `.p12` + password in GitHub secrets `MAC_SIGN_P12_B64`, `MAC_SIGN_P12_PASS`). Ad-hoc `"-"` only as the fallback when the secret is absent (dev builds). Notarization ($99) is a later drop-in. |
| 10 | macOS hardware testing: a friend's Mac, using CI-built DMGs and the I2 checklist; agents never touch it. |
| 11 | Linux hardware testing: a friend's Ubuntu machine (upgraded to 22.04/24.04), CI-built AppImage/deb + I3 checklist. Machine A WSL2 for NVENC. |
| 12 | Models: Opus for F1, F2, M4, L5(+spike), D1, M5, M6, L6, L7; Sonnet for M1–M3, L1, L3, L4, W1; Haiku for the rest (tier tag on each card). |
| 13 | Git: `plans/` on `main`; all port work on branch **`port/multi-os`**, one branch per task merged by PR; `main` stays the shippable Windows build. |
| 14 | Agents in git worktrees; cargo build/test/run pre-allowed. |
| 15 | Gates: F1 trait freeze reviewed before fan-out; hardware tasks are "done" only after a real-device pass. |
| — | Keep config key `startWithWindows` on disk; UI label "Start at sign-in". Keymap generated from Chromium `dom_code_data.inc`. Codec preference AV1 → HEVC → H.264. Permissions preflight modal. Small bugs found by the sweep are fixed inside the files being split. |
