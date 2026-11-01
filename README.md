# ShareCtrlScreen

Screen sharing + remote control between two machines, over the internet. Windows is
the shipped platform; macOS and Linux are being ported (see
[What works where](#what-works-where)). A viewer watches the host's screen (WebRTC
video) and — when the host grants it — drives the host's mouse and keyboard (data
channel → Win32 `SendInput` on Windows).

Each installation generates a UUID v4 once and keeps it; that UUID is the
machine's address. To connect somewhere, you type its UUID.

- `server/` — signaling/rendezvous server (Cloudflare Worker + Durable Object). Relays JSON only; never sees pixels or input.
- `app/` — Electron desktop app. Both roles (host and viewer) live in the same binary.
- `native/` — **native Rust rewrite** (Tauri + DXGI/Media Foundation/str0m) targeting AnyDesk-class latency, per `pans/04-NATIVE-REWRITE-PLAN.md`. Reuses the same signaling server and protocol. See [`native/README.md`](native/README.md).
- `pans/` — the design documents. `00-OVERVIEW-AND-PROTOCOL.md` is the wire contract.

## Install

The first time the app opens on a new machine, the notes in
[`docs/FIRST-RUN.md`](docs/FIRST-RUN.md) show each system dialog and what to answer.
Downloads are on the [Releases page](https://github.com/yanivkalfa/shareCtrlScreen/releases).
The macOS and Linux builds are still being ported. The steps below are the intended ones,
and they are checked on real hardware before each release (see
[`plans/checklists/`](plans/checklists/)).

### Windows

Unchanged from earlier releases: run the installer from the Releases page.

- The build is not code-signed, so Windows SmartScreen warns on the first install.
  Click **More info**, then **Run anyway**. This is expected.
- For UAC prompts, the lock screen and the sign-in screen, install the Windows service
  once from a terminal opened as administrator:
  `sharectrl-service.exe --install-service` (and `--uninstall-service` to remove it).
  Details are in [`native/README.md`](native/README.md).

### macOS 13 (Ventura) or later

The macOS build runs on Apple silicon and Intel Macs. It is **not notarized by Apple**,
so the first launch needs one extra step.

1. Open the `.dmg` and drag **ShareCtrlScreen** into **Applications**. Run it from there.
   Keep it there. The permissions in step 4 are tied to the app's identity and its install
   path.
2. macOS blocks the first launch with a dialog that starts **"Apple could not verify"**.
   Dismiss it.
3. Open **System Settings → Privacy & Security**, scroll down to the message about
   ShareCtrlScreen, click **Open Anyway**, and enter your administrator password.
   Open the app again. (Terminal alternative:
   `xattr -dr com.apple.quarantine /Applications/ShareCtrlScreen.app`.)
4. Grant the permissions in **System Settings → Privacy & Security**:
   - **Screen & System Audio Recording**: needed to share this Mac's screen.
   - **Accessibility**: needed for the remote side to control this Mac, and for Cmd+Tab
     passthrough when this Mac controls another computer.
   - **Input Monitoring**: needed together with Accessibility for Cmd+Tab passthrough when this
     Mac controls another computer.
5. On macOS 15 (Sequoia) and later, macOS asks about once a month whether the app may
   keep recording the screen. Click **Allow**. A host that is left unattended stops
   sharing until someone does. The app shows a notice when sharing stops for this reason.

Builds without the signing certificate (development builds) are ad-hoc signed. On those,
macOS forgets the permissions in step 4 after every update, so they must be granted
again.

### Linux (Ubuntu 22.04 or later)

Ubuntu 22.04 is the oldest supported release. Packages are built on 22.04, which keeps
them compatible with newer releases. The AppImage is planned to be tested on Ubuntu 22.04,
Ubuntu 24.04 and Fedora 40.

- **Ubuntu / Debian (`.deb`)**: `sudo apt install ./<file>.deb`. apt installs the runtime
  packages the app needs: `libwebkit2gtk-4.1-0`, `libgtk-3-0`, `libpipewire-0.3-0`,
  `libva2`, `libva-drm2` and `libxkbcommon0`.
- **Fedora and other RPM distributions (`.rpm`)**: `sudo dnf install ./<file>.rpm`.
- **Any distribution (`.AppImage`)**: `chmod +x <file>.AppImage`, then run it. The
  AppImage bundles most libraries. The C library and the GPU drivers come from your system.

Hardware video needs the GPU driver packages. Install the one for your GPU:
`intel-media-va-driver-non-free` (Intel), `mesa-va-drivers` (AMD), or NVIDIA's
proprietary driver (NVIDIA). Package names are for Ubuntu.

Check the hardware video support with `vainfo`. Near the top it prints a line such as
`vainfo: VA-API version: 1.20 (libva 2.20.1)`. The number in brackets is the libva version,
and hardware video needs 2.20 or newer. Below that it lists the video profiles your GPU
offers. Ubuntu 22.04 ships libva 2.14, so there the app uses software video whatever the
GPU is.

**Remote control on Wayland without the portal.** Some Wayland desktops, such as Weston,
give apps no way to control input. On those, remote control needs the optional uinput
setting. It is off by default; turn it on only if you need it. The app's installer for it
asks for your password once (through `pkexec`). To set it up by hand: put
`KERNEL=="uinput", GROUP="input", MODE="0660"` in `/etc/udev/rules.d/99-uinput.rules`, run
`sudo udevadm control --reload-rules && sudo udevadm trigger`, then
`sudo usermod -aG input $USER`, and log out and back in. If `/dev/uinput` is missing, run
`sudo modprobe uinput` first. Members of the `input` group can read and send input on every
device, so do this only on a machine you trust.

**Wayland consent.** On a Wayland session the first share opens the desktop's consent
dialog. Allow it. The choice is remembered in `~/.local/state/sharectrl/portal.token`, so
later sessions usually do not ask again. If the desktop refuses the remembered choice, the
dialog appears once more.

Installer and driver details, and the detailed troubleshooting steps, are in
[`native/README.md`](native/README.md).

## What works where

Windows is the shipped build. The macOS and Linux columns describe what the port is built
to do. An entry becomes true for a release only after the hardware checks for that OS
pass (see [`plans/checklists/`](plans/checklists/)). Entries marked *unverified* have not
been tested on hardware.

| Feature | Windows | macOS 13+ | Linux, X11 | Linux, Wayland |
|---|---|---|---|---|
| Screen capture | Yes (DXGI) | Yes, with Screen Recording permission (ScreenCaptureKit) | Yes, no prompt | Yes, after the portal consent dialog (ScreenCast) |
| Remote mouse and keyboard | Yes. Elevated windows and UAC prompts need the Windows service or a host run as administrator | Yes, with Accessibility permission | Yes (XTest) | Yes on GNOME 46+ and KDE Plasma 6 (RemoteDesktop portal). Sway and Hyprland: through wlroots virtual input, no dialog (unverified). Others, such as Weston: view only unless the uinput setting is on |
| Clipboard text, both ways | Yes | Yes | Yes. The clipboard lives only while the app runs | Yes, same as X11 |
| Clipboard files, pasted on this side | Yes | Yes | Yes | Yes |
| File push, controller to controlled | Yes | Yes | Yes | Yes |
| Drop a file on a remote window | Yes | Yes, with Accessibility permission | Yes | No. The file stays on the clipboard and a notice shows |
| Shortcut passthrough (Alt+Tab, Cmd+Tab, Super) | Experimental, off by default. Ctrl+Alt+Del and Win+L cannot be captured | Cmd+Tab, Cmd+Space, Cmd+\` and Ctrl+arrows (Spaces, Mission Control) while the session window is focused; Force Quit (Cmd+Option+Esc) always stays local. Not while Secure Input is on (password fields) | Yes, the window grabs the keyboard | Depends on the compositor. If refused, the app says shortcuts are unsupported here |
| Modifier translation for the remote OS (default on, Settings → Advanced) | From a Mac viewer: Cmd is sent as Ctrl, Cmd+Tab as Alt+Tab, Mac Control as Win. To a Mac host: Ctrl is sent as Cmd, Win as Control, Alt+Tab as Cmd+Tab | Same | Same | Same |
| Lock screen (a user is logged in) | Yes, with the Windows service | In scope. Unverified | In scope. Unverified | In scope. Unverified |
| Login window (nobody logged in) | Yes, with the Windows service | No | No | No |
| Hardware video encode | Yes (Media Foundation, H.264 and HEVC) | Yes (VideoToolbox, H.264 and HEVC) | Intel and AMD via VA-API, when libva is 2.20 or newer. NVIDIA via NVENC (optional) | Same as X11 |
| Hardware video decode | Yes (Direct3D 11) | Yes (VideoToolbox). AV1 on M3 and later | Intel and AMD via VA-API, when libva is 2.20 or newer. NVIDIA via NVDEC (optional) | Same as X11 |
| Software video (OpenH264) | Not yet (planned: downloaded from Cisco on first use) | Yes, downloaded from Cisco on first use | Yes, downloaded from Cisco on first use | Yes, downloaded from Cisco on first use |
| Saved passwords | Yes (Windows DPAPI) | Yes (Keychain) | Yes with a Secret Service keyring (GNOME Keyring or KWallet). Without one, nothing is saved | Same as X11 |
| Start at sign-in | Yes | Yes (LaunchAgent) | Yes (XDG autostart) | Yes (XDG autostart) |

Software video is used whenever no hardware codec is available. Its download needs an
internet connection, and it can be switched off in Settings under Advanced.

## Running it locally

**1. Start the signaling server**

```bash
cd server
npx wrangler dev --port 8787      # serves ws://localhost:8787/ws
```

No Cloudflare account is needed for local dev.

**2. Start the app**

```bash
cd app
npm install
npm start
```

**3. Start a second instance to test against**

The app deliberately does *not* take a single-instance lock. `--profile <name>`
gives an instance its own `userData` directory, so it gets its own UUID and
config:

```bash
npm start -- --profile b
```

Both instances default to `ws://localhost:8787/ws`. Point them at a deployed
server by editing **Server URL** in ⚙ Settings (changing it reconnects immediately).

## Using it

The home screen shows **your ID** (with a Copy button) and a box to enter a
**remote ID**. The status dot is green when registered with the server, yellow if
the server connection dropped while a session is still running, red when offline.

**Access modes** (⚙ Settings — these govern *incoming* connections):

- **Ask me for each connection** (`approve`, the default) — every attempt raises a
  popup showing the requester's UUID with three choices: Deny / Allow view only /
  Allow view + control. It auto-denies after 30 s.
- **Password** — set a password and a permission level. A viewer that submits the
  correct password is admitted automatically at exactly that level, with no popup.
  If password mode is selected but no password is set, the app falls back to
  `approve` (fail-safe).

While a session is running, the host's session panel can switch the permission
between `view` and `control` at any time — it takes effect immediately, without
reconnecting — and can end the session.

## Packaging

Releases are built by the **Release** workflow (`.github/workflows/release.yml`). To cut one,
set `version` under `[workspace.package]` in `native/Cargo.toml` (the only copy of the
version), commit, and push the tag `v<version>`. The workflow checks the tag against
`cargo metadata`, runs the licence gate, builds Windows, macOS and Linux, and publishes a
GitHub Release with these assets:

| Asset | For |
|---|---|
| `ShareCtrlScreen_<version>_x64-setup.exe` | Windows installer (not code-signed: SmartScreen warns, More info → Run anyway) |
| `sharectrl-service.exe` | Windows service for UAC prompts and the lock screen |
| `ShareCtrlScreen_<version>_universal.dmg` | macOS 13+, Apple Silicon and Intel |
| `ShareCtrlScreen_<version>_amd64.deb` / `ShareCtrlScreen-<version>-1.x86_64.rpm` / `ShareCtrlScreen_<version>_amd64.AppImage` | Linux |
| `THIRD-PARTY-LICENSES-<os>.html` | Licences of the open-source code in that OS's build (also inside each installer) |
| `SHA256SUMS.txt` | Check downloads with `sha256sum -c SHA256SUMS.txt` |

**Run workflow** in Actions with `dry_run` ticked builds every asset as run artifacts and
publishes nothing. To build this machine's assets locally the same way, run
`native/scripts/release-dry-run.sh` (it lists what it needs); the results land in
`native/dist/`. The legacy Electron app can still be packaged with `cd app && npm run dist`.

## Known limitations (v1)

- **Platforms.** Windows is shipped. macOS 13+ and Linux (Ubuntu 22.04+) are in the
  port; see [What works where](#what-works-where) for each feature.
- **Login window (macOS and Linux).** When nobody is logged in, the sign-in screen cannot
  be shared or controlled on macOS or Linux. Windows can, through the service. The lock
  screen with a user logged in is in scope on all three systems.
- **Wayland focus.** On Wayland the app cannot bring a remote window to the front without a
  click, because no focus request is possible for other apps. A file dropped on a remote
  window stays on the clipboard, and a notice shows.
- **Wayland desktops without remote input (Linux).** On Weston and similar desktops,
  sessions are view only unless the optional uinput setting is on. Sway and Hyprland may
  work without it.
- **Protected video (DRM).** Protected content shows as black on every system.
- **macOS signing.** The app is not notarized by Apple. The first launch needs
  **Open Anyway** (see [Install](#install)), and on macOS 15 and later Screen Recording
  asks about once a month. Development builds without the signing certificate lose their
  permissions on each update.
- **macOS Secure Input.** While a password field has Secure Input on, the system blocks
  event taps, so shortcut passthrough stops working.
- **Linux software video.** Ubuntu 22.04 ships libva 2.14, so its hardware video path is
  off and software video (OpenH264, downloaded from Cisco) is used.
- **Multi-monitor (Windows):** the host can pick which monitor to share in ⚙ Settings →
  "Monitor to share". Sharing the primary uses a simple absolute-coordinate path;
  sharing a non-primary monitor maps input through the Win32 virtual desktop
  (`VIRTUALDESK`, DPI-corrected). Capture and injection stay consistent for the
  chosen monitor. (One monitor at a time; no all-monitors view.)
- **Elevated windows are not controllable (Windows).** A non-elevated host cannot inject
  into UAC prompts or apps running as administrator — this is the Windows UIPI
  security boundary, not a bug. Run the host as administrator if you need to
  control elevated apps.
- **Keyboard shortcut passthrough (Windows, experimental, off by default):** enable
  ⚙ Settings → "Capture keyboard shortcuts" and, while viewing with control,
  **Alt+Tab / Alt+Esc / the Win keys are captured locally and sent to the remote**
  instead of acting on your machine (a focus-scoped low-level keyboard hook —
  losing window focus releases it). Still **cannot** be captured, by Windows
  design: **Ctrl+Alt+Del** and **Win+L** (kernel/secure-desktop level). F11 is
  deliberately left alone so local fullscreen still works.
- **Modifier translation** (default on, ⚙ Settings → Advanced → "Translate modifiers for
  the remote OS"): Mac → Windows/Linux sends Cmd as Ctrl and Cmd+Tab as Alt+Tab, Mac
  Control as Win/Super; Windows/Linux → Mac sends Ctrl as Cmd, Win/Super as Control,
  Alt+Tab as Cmd+Tab. Ctrl+Tab stays Ctrl+Tab. Off = literal keys. Same-OS and
  Windows ↔ Linux sessions are never translated.
- **One session at a time** per machine. A second incoming request gets `busy`.
- **Password uses challenge-response** — the viewer sends a proof derived from a
  host-issued nonce (`SHA256(SHA256(password) + ":" + nonce)`), never the plaintext,
  so the password never transits the relay. Compared host-side against a stored
  SHA-256; the host adds a 2-second delay before rejecting a wrong password. There is
  no stronger rate limiting beyond one-session-at-a-time.
- **System audio (Windows)** is shared by default (Windows loopback capture, played on the
  viewer). Toggle it off in ⚙ Settings → "Share this machine's system audio when
  hosting". Note: on the same-machine `--profile` test setup this creates an audio
  feedback loop (the host captures the viewer playing back the audio); it is only
  meaningful between two machines.
- **Testing both roles on one machine is reflexive.** If you host and view on the
  same desktop, injected input lands on whatever window has focus — if that is the
  viewer, it re-captures and re-sends it, producing a feedback loop (mouse/keyboard
  and audio both). This cannot happen between two machines; it is only an artifact
  of the single-machine `--profile` test setup.

## How it fits together

```
Viewer app  ── wss ──►  Signaling server  ◄── wss ──  Host app
     │        (JSON: connect req/resp, SDP, ICE)          │
     └────────────── WebRTC peer connection ──────────────┘
            • video track  : host screen → viewer
            • data channel : viewer input → host, host control → viewer
```

The server keeps a `uuid → socket` map and relays messages between the two peers;
that is its whole job. Once signaling completes, video and input flow directly
peer-to-peer (STUN for NAT traversal; add a TURN entry to `iceServers` in the
config for restrictive NATs). Apps send a `{"type":"ping"}` every 25 s and
reconnect with exponential backoff if two pongs are missed.
