# ShareCtrlScreen — Native Rust Rewrite

AnyDesk/Parsec-class remote desktop for Windows, built per
[`pans/04-NATIVE-REWRITE-PLAN.md`](../pans/04-NATIVE-REWRITE-PLAN.md). Proprietary,
closed-source, permissively-licensed dependencies only. Electron is gone; this is a
native Tauri + Rust app targeting ~20–40 ms glass-to-glass latency.

The Cloudflare signaling server (`../server/`) and the JSON WebSocket protocol
(`../pans/00-OVERVIEW-AND-PROTOCOL.md`) are reused **verbatim** — only the opaque
`signal.data` payloads change (str0m's ICE/DTLS instead of browser SDP).

## Workspace layout (§4)

| Crate | Responsibility |
|---|---|
| `crates/protocol` | Shared types: config schema + challenge-response, signaling / input / control message enums. Pure, cross-platform, unit-tested. |
| `crates/platform-api` | **The OS boundary**: traits for capture, codecs, video surface + presenter, input injection, shortcut hook, clipboard, secrets, autostart, paths, permissions. No OS dependencies. Feature `mock` = in-memory backends for tests. |
| `crates/platform-windows` | Windows implementation of `platform-api`, wrapping `capture`/`codec`/`render`/`input`/`elevation` + DPAPI, Run key, Win32 clipboard. |
| `crates/platform-macos`, `crates/platform-linux` | macOS / Linux implementations, filled in task by task (on macOS capture, the video surface, input injection, clipboard, Keychain, autostart and permissions are real; on Linux X11 screen capture — MIT-SHM + DAMAGE, `X11Capture` — and the X11 display list are real); a capability whose task has not landed fails safely — see `../plans/05-TASKS.md`. |
| `crates/capture` | DXGI Desktop Duplication (§5a): shared `ID3D11Texture2D` + dirty/move rects + out-of-band cursor; `ACCESS_LOST` re-init. |
| `crates/codec` | Media Foundation HW H.264/HEVC encode (§5b, zero-copy, LTR, the low-latency `ICodecAPI` recipe) + D3D11-backed decode (§5c). |
| `crates/transport` | str0m WebRTC (§6): video on an RTP media track with congestion control (BWE), ctl (reliable) / cursor (unreliable) / bulk (reliable) data channels. Unit-tested. |
| `crates/signaling` | Async WebSocket client to the Cloudflare Worker: register, byte-exact ping/pong watchdog, backoff reconnect. |
| `crates/input` | `SendInput` injection + adapter over the shared generated keymap (`platform_api::keymap`, Set-1 column) + `WH_KEYBOARD_LL` shortcut hook. Ported from `input.js`/`keyhook.js`. Unit-tested. |
| `crates/render` | D3D11 flip-model present (§7): waitable swapchain, `ALLOW_TEARING`, NV12→RGB BT.709 shader. |
| `crates/elevation` | SYSTEM service primitives (§8b): session detection, `WTSQueryUserToken`, `CreateProcessAsUser`, input-desktop follow, service install/uninstall. |
| `crates/engine` | OS-independent orchestration: handshake state machine (approve/password, live view↔control), and the session (ICE, transport driver, media threads, clipboard/file sync) — all OS access via `platform-api`. |
| `app/` | Tauri 2 binary: `#[tauri::command]`s, WebView2 UI host, native D3D child-HWND video, ported vanilla UI in `app/ui/`. |
| `service/` | Thin SYSTEM service binary: SCM dispatcher + session-follow launch loop. |

## Build

```sh
cd native
cargo build --workspace --release
cargo test  --workspace          # all unit tests; runs on Windows, macOS and Linux
```

Two shippable binaries: `sharectrl.exe` (the Tauri app/engine) and
`sharectrl-service.exe` (the SYSTEM service).

Requires the Rust `x86_64-pc-windows-msvc` toolchain and the WebView2 runtime
(present on Windows 11; the installer ships the bootstrapper otherwise).

## The go/no-go: latency smoke-test (§12)

The single validation that the premise holds. On target hardware:

```sh
cargo run -p smoketest --release
```

It wires `capture → encode → decode` on one machine and prints per-stage timings
against the §2 budget. If capture+encode+decode isn't in the low-single-digit-ms
range, ~40 ms glass-to-glass is unreachable and the design must change **before**
polishing transport/UI/elevation.

It works on every OS (through the `platform-api` traits, with the same cfg
selection as the app). Flags: `--mock` (in-memory platform; runs anywhere and in
CI), `--software` (software codec factory, once task D2 lands), `--display <id>`,
`--seconds <n>`, `--size <W>x<H>`. Keep something moving on screen while it runs.
It prints avg / p50 / p95 / max per stage and exits non-zero when p95 of encode or
decode misses the §2 target (capture is printed but not gated: it includes waiting
for the screen to change).

## Install (§8b, §10)

```sh
sharectrl-service.exe --install-service     # admin, once — registers LocalSystem service
sharectrl-service.exe --uninstall-service   # admin
```

The service is mandatory to reach the secure desktop (UAC / lock / login). UIAccess is
deliberately **not** used, so the app runs **unsigned** — the only cost is a first-install
SmartScreen "Run anyway" prompt (§8b/§11.4). No code-signing certificate, no codec
license, no cloud bill: `$0` to build and ship (§11.4).

## macOS install

Requires macOS 13 (Ventura) or later; the DMG is universal (Apple Silicon and Intel). The
app is signed with a self-signed certificate, **not** notarized, so macOS blocks the first
launch. This is expected and happens once per downloaded build:

1. Open the DMG and drag `ShareCtrlScreen` onto `Applications`.
2. Open it from `Applications`. macOS says *"Apple could not verify ShareCtrlScreen is free
   of malware…"*. Click **Done** (not *Move to Trash*).
3. System Settings → **Privacy & Security** → scroll to *Security* → **Open Anyway**
   next to ShareCtrlScreen → enter your password → **Open Anyway**.
   (macOS 15 Sequoia removed the Control-click → Open shortcut; this is the only GUI way.)
4. Or in Terminal, instead of 2–3: `xattr -dr com.apple.quarantine /Applications/ShareCtrlScreen.app`
5. Grant **Screen & System Audio Recording** and **Accessibility** when asked (the app
   shows a checklist). Screen Recording needs a restart of the app after granting.
   Capturing system shortcuts (Cmd+Tab, Cmd+Space, Ctrl+arrows) for the remote while
   viewing needs **Input Monitoring** and **Accessibility**.

*"ShareCtrlScreen is damaged and can't be opened"* means the quarantine flag is set on a
bundle whose signature is missing or broken (an unsigned or re-zipped copy). Re-download
the DMG; if it persists, run the `xattr` command above.

Updating: replace `/Applications/ShareCtrlScreen.app` with the new build (same path).
Permissions stay granted because every build is signed by the same certificate. If they
vanish after an update, the certificate changed, see `../plans/checklists/M10.md`.
macOS 15+ additionally asks about once a month to confirm screen recording; that prompt
comes from macOS and cannot be turned off by the app.

Maintainers: `scripts/macos-make-signing-cert.sh` creates the certificate (once),
`scripts/macos-sign.sh` signs a `.app`, and `scripts/release-assets.sh package macos` builds,
signs and verifies the DMG (run by `.github/workflows/build-assets.yml`, which both
`mac-build.yml` and `release.yml` call). The two repository secrets are `MAC_SIGN_P12_B64`
and `MAC_SIGN_P12_PASS`; without them the build falls back to an ad-hoc signature
(permissions are lost on every update) and says so in a warning and in the release notes.
With them, a signature that is not certificate-based fails the build.
`app/icons/icon.icns` is regenerated with `cargo tauri icon` (command in `../plans/checklists/M10.md`).

## Linux install

Three packages are built on Ubuntu 22.04 (glibc 2.35, WebKitGTK 4.1) by the `linux-build`
workflow, so they run on Ubuntu 22.04 and newer, Fedora 40 and comparable distributions:

| Package | Install | Notes |
|---|---|---|
| `.deb` | `sudo apt install ./ShareCtrlScreen_*_amd64.deb` | Debian 12+, Ubuntu 22.04+. `apt` pulls the dependencies. |
| `.rpm` | `sudo dnf install ./ShareCtrlScreen-*.x86_64.rpm` | Fedora 40+. |
| `.AppImage` | `chmod +x ShareCtrlScreen_*.AppImage && ./ShareCtrlScreen_*.AppImage` | Needs FUSE 2 (`libfuse2` / `fuse-libs`), or run with `--appimage-extract-and-run`. |

**Runtime dependencies.** Required (declared as package `Depends`): WebKitGTK 4.1, GTK 3,
PipeWire 0.3 client library, libxkbcommon. Recommended (not required): `libva2` +
`libva-drm2` (hardware H.264 through VA-API; loaded with `dlopen`, so the app starts and
falls back to software encoding and decoding without them) and `xdg-desktop-portal` with a backend for
your desktop (`-gnome`, `-kde`, `-wlr`, `-hyprland`), which Wayland screen sharing needs.

**GPU video acceleration.** The driver comes from your distribution, not from this app:
Intel `intel-media-va-driver-non-free` (or `intel-media-va-driver`), AMD `mesa-va-drivers`,
NVIDIA proprietary driver. Check with `vainfo` (package `vainfo` / `libva-utils`): it must
list `VAProfileH264Main` or `VAProfileH264High` with `VAEntrypointEncSlice` (or
`VAEntrypointEncSliceLP`) for hosting and with `VAEntrypointVLD` for viewing.
`Failed to initialize display` or no H.264 entrypoint means the app uses software H.264
for that side. NVIDIA GPUs encode with NVENC and decode with NVDEC (`libnvcuvid.so.1`,
part of the proprietary driver); NVDEC also decodes HEVC/AV1 where the GPU supports it.

**AppImage caveats.** The AppImage bundles WebKitGTK, GTK and their data but not glibc,
the GPU/VA drivers or the PipeWire daemon, which always come from the host. GStreamer media
plugins are not bundled (`bundleMediaFramework` is off): the UI plays no media, and video
is rendered natively. A host that is older than Ubuntu 22.04 cannot run it.

**Remote control without the portal (uinput).** On Wayland, input is injected through the
desktop's RemoteDesktop portal. Where the portal cannot do that, Settings offers
"Enable uinput access", which asks for your password once (polkit) and runs
`platform_linux::install_uinput_access`: it writes `/etc/udev/rules.d/70-sharectrl-uinput.rules`
(`/dev/uinput` to group `input`, mode 0660), adds your user to `input`, loads the `uinput`
module now and at boot, and reloads udev. Log out and back in afterwards. To do the same
by hand, or to undo it:

```sh
sudo install -m 0644 /dev/stdin /etc/udev/rules.d/70-sharectrl-uinput.rules <<'EOF'
KERNEL=="uinput", SUBSYSTEM=="misc", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"
EOF
echo uinput | sudo tee /etc/modules-load.d/sharectrl-uinput.conf
sudo usermod -aG input "$USER" && sudo modprobe uinput && sudo udevadm control --reload-rules && sudo udevadm trigger
# undo: sudo rm /etc/udev/rules.d/70-sharectrl-uinput.rules /etc/modules-load.d/sharectrl-uinput.conf; sudo gpasswd -d "$USER" input
```

Membership of `input` also allows reading every input device (keystrokes) of the machine,
so enable it only on machines where you accept that.

**Logs.** `~/.local/state/sharectrl/app.log` (`$XDG_STATE_HOME/sharectrl/app.log`).
Config lives in `~/.config/sharectrl/`.

**Building the packages.** On Ubuntu 22.04: install the packages listed in
`.github/workflows/build-assets.yml` (Linux step), the Tauri CLI and cargo-about at the
versions pinned there, then run `scripts/release-dry-run.sh linux`. The release assets
land in `native/dist/`. The full Linux recipe needs a real Ubuntu (or another machine
where the AppImage tooling runs; the script sets `APPIMAGE_EXTRACT_AND_RUN=1`). Under WSL
the AppImage step fails (no FUSE), so the recipe and its `package` phase cannot complete
there. To check the deb and rpm there, run by hand from `app/` the first command
`scripts/release-dry-run.sh --print-build-steps linux` prints (the notices page the
bundle includes), then `cargo tauri build --config tauri.release.conf.json --bundles deb,rpm`. The icon PNGs in `app/icons/` are
regenerated with `python3 app/icons/generate-linux-icons.py`.

## Licensing (§11 — CI-enforced)

- `deny.toml` — `cargo deny check` allowlist + banned-crate list (AGPL capture crates,
  no-license `hwcodec`, GPL x264/x265). Runs in CI.
- `about.toml` / `about.hbs` — `cargo about generate`, run per OS by
  `scripts/release-assets.sh` → `THIRD-PARTY-LICENSES.html` inside each installer and
  `THIRD-PARTY-LICENSES-<os>.html` on each release. Generated, never committed.
- Clean-room: **zero lines** of RustDesk (AGPL). Every module implemented from
  Microsoft/vendor primary docs.

## Releasing

Push the tag `v<version>` matching `[workspace.package] version` (see the root README,
*Packaging*). The pipeline: `.github/workflows/release.yml` → `build-assets.yml` (one
reusable workflow per OS) → `scripts/release-assets.sh` (the recipe). Local equivalent:
`scripts/release-dry-run.sh [--print-build-steps | --skip-build] [windows|macos|linux]`.

## Status / what needs on-hardware validation

**Fully wired end-to-end** (no stubbed integration points): the host pipeline
(capture → GPU BGRA→NV12 → HW encode → FEC-fragmented transport), the viewer
pipeline (transport → HW decode → D3D11 present with a client-side cursor
sprite), input (viewer captures over the native video window → host injects,
permission-gated, with input-desktop follow), codec negotiation from the
viewer's caps, dirty-rect adaptive frame rate, and the optional `WH_KEYBOARD_LL`
shortcut hook. Control plane (handshake, challenge-response, config, session
state, FEC, packetization, signaling watchdog) is unit-tested and portable.

What still needs a real GPU + two machines to validate (not stubs — timing and
vendor quirks):
- **Glass-to-glass latency** against the §2 budget — run the smoke-test above.
- **Vendor-MFT quirks**: some encoders silently no-op low-latency props (§5b);
  the async event pump and D3D11VA decode want per-GPU checking.
- **NAT traversal**: the transport advertises a host candidate for the LAN direct
  path; STUN/TURN for restrictive NATs is the documented §6 fallback, not yet
  wired into the ICE gathering.
- **BWE**: data-channel transport has no TWCC, so the adaptive-bitrate loop is
  plumbed (encoder reads a shared target each frame) but the estimate is static
  until a loss/RTT signal is added — a known consequence of the §6 data-channel
  choice.
- **Secure-desktop injection** requires running the engine via the installed
  SYSTEM service (§8b); portable mode reaches normal windows only.
```
