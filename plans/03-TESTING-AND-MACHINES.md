# 03 — Testing strategy, CI, and which machines you need

Answers to the three questions: *do I need a Mac?*, *can we test the pipeline?*, *what runs
where?* — plus what was actually verified on your hardware today.

## 1. Do I need a Mac?

| Activity | Needs a real Mac? | Where it runs |
|---|---|---|
| Compile the macOS backend, run unit tests, build the DMG, sign with the self-signed cert | **No** | GitHub-hosted `macos-latest` runners — **free and unmetered for your public repo** (`yanivkalfa/shareCtrlScreen` is public; private repos would burn ~10× minutes) |
| Test VideoToolbox encode/decode (M2, M3), NSPasteboard (M7), Keychain (M8), presenter math (M4 offscreen) | **No** | same runners — these APIs need no TCC permission |
| Test ScreenCaptureKit capture, CGEvent injection, the event tap, the NSView surface UX, the Gatekeeper "Open Anyway" flow, the TCC persistence across updates | **Yes** — interactive GUI session | a rented Mac mini over VNC, or any Mac you can borrow. GitHub runners cannot grant Screen Recording/Accessibility (SIP on; `tccutil`/TCC.db edits need SIP off; see runner-images #8951/#1567) |
| macOS VM on your Windows PC | **No, don't** | violates Apple's EULA on non-Apple hardware **and** has no GPU → no VideoToolbox, no SCK; useless for exactly the parts you'd need it for |

Renting (all give VNC/Screen Sharing into the GUI, which is where you click the TCC
prompts — Scaleway's own Reemo tutorial does this):

| Provider | Price (2026, approx.) | Minimum | Notes |
|---|---|---|---|
| Scaleway Apple Silicon (M1/M2/M4) | ~€0.10–0.22/h, €69–149/mo | 24 h | VNC port on the Overview page; keep FileVault off |
| AWS EC2 mac2 | ~$1.23/h | 24 h ($30) | most expensive for short use |
| MacinCloud / Juuz / MacStadium | $25–130/mo | monthly | shared tiers lack admin rights — need a dedicated tier |
| Used M1 Mac mini | ~$250–350 once | — | the cheapest option if this port is going to live |

Budget reality: **2–3 days of rented Mac time** (≈ €15–30 on Scaleway) is enough for the
I2 integration pass if the macOS backend was developed test-first against the mocks and
the runner-testable parts.

## 2. Do I need a Linux machine?

* **WSL2 on machine A is already a Linux build+test box.** Verified today: Ubuntu 24.04,
  rustc 1.99, after `apt install libwebkit2gtk-4.1-dev libxdo-dev libayatana-appindicator3-dev
  librsvg2-dev libdbus-1-dev libgtk-3-dev build-essential cmake pkg-config libssl-dev` the
  whole workspace type-checks except the two expected Windows-only spots
  ([07](07-CODEBASE-SEAMS.md)); with `secret.rs` stubbed, `app` compiles and 15/16 engine
  unit tests pass (see §5). WSLg provides a Wayland display (`wayland-0`) and XWayland (`:0`),
  so the X11 code paths and the GTK/Tauri window can be exercised; **the portal/PipeWire
  path cannot** (WSLg's Weston has no `xdg-desktop-portal` backend) — use a nested
  `sway --headless`/`mutter --headless` inside WSL for that (same recipe as CI, T4).
* **GPU in WSL2:** `/usr/lib/wsl/lib` exposes `libnvidia-encode.so.1` and `libnvcuvid.so.1`
  from your GTX 1080 Ti driver (572.83) → **NVENC/NVDEC should work in WSL2** (a June-2026
  report confirms encode works; an old forum thread reports the opposite — verify in L3).
  VA-API via Mesa's `d3d12` driver needs `/dev/dri/card0`, which is **absent** on your
  WSL today (`/dev/dxg` exists) — likely a `wsl --update`/kernel-module issue, not worth
  chasing: VA-API testing belongs on real Intel/AMD hardware.
* **Interactive desktop testing (GNOME/KDE/Sway, portals, restore tokens):** a VM on machine
  A — VirtualBox or VMware Workstation Pro (free for personal use) with Ubuntu 24.04 (GNOME
  46, Wayland default), Kubuntu 24.10+/Fedora KDE (Plasma 6), and a Sway live image. No
  hardware codecs in VMs → the software H.264 path (D2) carries these tests, which is
  exactly why D2 is on the critical path.
* **Hardware encode/decode on Linux:** machine B booted from an Ubuntu live USB (if it has
  an Intel/AMD iGPU — the memory note says it has no HW *decoder* on Windows, so check
  `vainfo`), or machine A's NVIDIA GPU via WSL2 (NVENC) / a dual-boot.

## 3. Can we test the pipeline without real screens?  Yes — this is the main design win of the trait split

```
                    ┌──────────── CI on every OS, no GPU, no permissions ────────────┐
MockCapture ──► VideoEncoder (OpenH264 sw | MockCodec) ──► transport (real str0m, real UDP 127.0.0.1)
                                                                      │
MockSurface ◄── Presenter(Cpu) ◄── VideoDecoder (OpenH264 sw | MockCodec) ◄──┘
   │ scripted VideoInput ──► session ──► ctl channel ──► host session ──► MockInjector (asserted)
   └ MockClipboard / MockSecretStore / temp downloads dir
```
Task **D1** builds this harness (`engine/tests/loopback.rs`). It covers everything the
Windows-only integration test covers today *except* the OS calls themselves — handshake,
ICE, DTLS/SCTP, RTP video, FEC, keyframe gating, liveness, bitrate policy, input routing,
permission gate, file push, clipboard echo guard, teardown. It runs in ~5 s on a 2-core
runner. Each OS backend is then tested in isolation against the trait contract:

| Backend | Testable in CI? | How |
|---|---|---|
| VideoToolbox enc/dec (M2/M3) | **yes** (macOS runner, hardware) | synthetic frames, PSNR, bitrate tracking, IDR-on-demand |
| ScreenCaptureKit (M1) | no (TCC) | manual on a real Mac; smoke script prints fps/CPU |
| CGEvent injector / tap (M5/M6) | no (Accessibility) | manual script types into TextEdit |
| NSView surface (M4) | partially (offscreen Metal render compared pixel-wise) | UX manual |
| X11 capture/inject/surface/grab (L2/L5/L6/L7) | **yes** (`Xvfb` + `xterm` + `xdotool`, software GL) | T3 |
| Wayland portal capture + libei input (L1/L6) | **yes** (headless `sway` + `xdg-desktop-portal-wlr` + `pipewire`; `mutter --headless` + `-gnome` for libei) | T4 — copy the DeskShare `linux-wayland-e2e` Docker recipe |
| uinput (L6) | **yes** (runner has passwordless sudo: `modprobe uinput`) | T3 |
| VA-API / NVENC (L3/L4) | no (no GPU on runners) | manual on machine B/A; golden `.h264` files cross-checked by every decoder in CI (I5) |
| OpenH264 software codec (D2) | **yes**, all three OSes | round-trip + speed budget |
| Windows DDA/MF/D3D11 (W1) | no (no GPU on `windows-latest`) | your two machines, as today |
| Secret stores (M8/L9) | **yes** (Keychain on runner; `gnome-keyring` under `dbus-run-session` on Linux) | |
| Autostart (M9/L10) | **yes** (plist / `.desktop` in a temp `$HOME`) | |
| Latency budget (§2 of plan 04) | per OS on real hardware | T5 generalises `latency_smoketest` |

## 4. CI matrix (task T1–T4)

```yaml
jobs:
  build-test:            # fmt, clippy -D warnings, test (mocks + software codec)
    strategy: { matrix: { os: [windows-latest, macos-latest, ubuntu-22.04] } }
  x11-e2e:  ubuntu-22.04 + Xvfb (+ xterm, xdotool, LIBGL_ALWAYS_SOFTWARE=1, sudo modprobe uinput)
  wayland-e2e: ubuntu-24.04 container: sway(headless/pixman) + pipewire + xdg-desktop-portal(-wlr); second job mutter --headless + xdg-desktop-portal-gnome
  mac-hw-codec: macos-latest (VideoToolbox tests; skip when VTIsHardwareDecodeSupported is false)
  license-gate: cargo deny check (targets: win-msvc, apple-darwin x2, linux-gnu)
  release: matrix → NSIS | DMG (self-signed, arm64 + x86_64) | AppImage/deb/rpm
```
Cost: $0 (public repo). If the repo ever goes private, macOS minutes are the expensive
ones — keep the macOS job on `push` to `main` + tags only.

## 5. What was verified on your machines today

* Windows → `cargo check --target x86_64-unknown-linux-gnu` / `aarch64-apple-darwin`: fails
  at `aws-lc-sys` (needs `x86_64-linux-gnu-gcc` / `cc`): **cross-checking from Windows is
  not a usable workflow**; check natively (WSL, runners).
* WSL2 Ubuntu 24.04 (native): `cargo check --workspace` → only `engine` (`secret.rs`,
  2 errors) and `service` (no `main`) fail. With `secret.rs` stubbed: `app` **compiles**;
  `cargo test -p engine --lib` → **15 passed, 1 failed** (`roundtrips_through_dpapi`, see
  note below); `protocol`/`transport`/`input`/`signaling` tests pass.
* WSL GPU libs present: `libnvidia-encode.so.1`, `libnvcuvid.so.1`, `libcuda.so.1`,
  `libd3d12.so`; `/dev/dri` absent; `nvidia-smi` sees the GTX 1080 Ti (driver 572.83).
  CPU: i9-12900K, 24 threads, 62 GB visible to WSL — plenty for the VM-based desktop tests.
* Tauri 2.11.5 is the resolved version; `WebviewWindow::{hwnd, ns_window, ns_view, gtk_window}`
  exist (`webview_window.rs:1835-1862`).

> The one failing Linux engine test is `secret::tests::roundtrips_through_dpapi` — expected,
> since the temporary stub returns `None`; it becomes a per-backend test in F3. Notably
> `transfer::tests::traversal_name_stays_inside_the_directory` **passes** on Linux:
> `protocol::sanitize_file_name` already treats `\` as a separator on every OS.

## 6. Suggested machine plan

| Phase | Machines |
|---|---|
| Foundation + Windows lanes (F, W, D, T) | machine A (Windows + WSL2), machine B |
| macOS lane development | GitHub macOS runners (compile/tests on every push) + 1 rented Mac for 1–2 days mid-lane (M1/M4/M5/M6 smoke) |
| Linux lane development | WSL2 for X11/software; VirtualBox VMs (GNOME 46, Plasma 6, Sway) on machine A for portals; machine B live-USB or machine A NVIDIA/WSL for hardware codecs |
| Integration (I1–I5) | all of the above + 2–3 rented-Mac days |

Sources: [GitHub Actions pricing 2026](https://cicdpipelinecost.com/github-actions-pricing),
[macOS runner cost](https://macrun.dev/guides/github-actions-macos-minutes-cost),
[Scaleway Mac mini](https://www.scaleway.com/en/docs/apple-silicon/how-to/access-remote-desktop-mac-mini/),
[Mac cloud pricing comparison](https://www.macrent.cloud/blog/cloud-mac-rental-compared),
[AWS EC2 Mac pricing](https://macyou.co/blog/aws-ec2-mac-pricing-explained),
[WSL D3D12 video acceleration](https://devblogs.microsoft.com/commandline/d3d12-gpu-video-acceleration-in-the-windows-subsystem-for-linux-now-available/),
[NVENC in WSL2 (2026)](https://github.com/games-on-whales/wolf/discussions/448),
[runner-images #8951](https://github.com/actions/runner-images/issues/8951).
