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

Run the installer from the Releases page.

- The build is not code-signed, so Windows SmartScreen warns on the first install.
  Click **More info**, then **Run anyway**. This is expected.
- The installer **sets up the Windows service for you**. It installs
  `sharectrl-service.exe` next to the app in `C:\Program Files\ShareCtrlScreen\`, then
  registers and starts the `ShareCtrlScreenHost` service (one administrator consent). The
  service launches the host at every sign-in. It does **not** yet reach the lock screen, UAC
  prompts or the sign-in screen: while one is up, the viewer shows a message saying so
  instead of a black screen, and the picture returns once someone at the computer unlocks
  it. Reaching them is planned in [plans/10](plans/10-REMOTE-UNLOCK.md). Uninstalling removes
  the service.
- The host the service starts lives in the notification area. **Closing its window does
  not disconnect the machine**: it ends any live session and hides the window, and the
  machine stays reachable. Click the tray icon to bring the window back, or right-click
  it → **Quit** to stop the host. After Quit the host stays stopped until you sign in
  again or start the app. If the host crashes or is ended from Task Manager, the service
  starts it again within seconds.
- Because the service already starts the host at sign-in, the installed app does **not**
  add its own "Start at sign-in" entry (one host, not two). The setting has no effect while
  the service is installed.
- One instance only: launching the app again brings the running window to the front instead
  of starting a second copy.
- The standalone `sharectrl-service.exe` download is only for a **manual** install — put it
  next to a portable `sharectrl.exe` and run `sharectrl-service.exe --install-service` from
  a terminal opened as administrator (`--uninstall-service` removes it). The regular
  installer does this for you. Details are in [`native/README.md`](native/README.md).

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
| Remote mouse and keyboard | Yes. The installer sets up the Windows service, which runs the host as the signed-in user. That host still **cannot** control UAC prompts or elevated apps (the UIPI boundary — see Known limitations; run the host as administrator for those), and the secure desktop (lock screen, UAC prompts, sign-in screen) is out of reach: the viewer is told when it is up. Alt+Tab / Windows-key passthrough is a separate feature (W5) | Yes, with Accessibility permission | Yes (XTest) | Yes on GNOME 46+ and KDE Plasma 6 (RemoteDesktop portal). Sway and Hyprland: through wlroots virtual input, no dialog (unverified). Others, such as Weston: view only unless the uinput setting is on |
| The remote computer's real mouse pointer (arrow, I-beam, hand, resize), drawn on the viewer | Yes (pointer images from DXGI) | Yes (pointer images from AppKit) | Yes (XFixes) | Yes, when the portal sends pointer images (cursor metadata) |
| Clipboard text, both ways | Yes | Yes | Yes. The clipboard lives only while the app runs | Yes, same as X11 |
| Clipboard files, pasted on this side | Yes | Yes | Yes | Yes |
| File push, controller to controlled | Yes | Yes | Yes | Yes |
| Drop a file on a remote window | Yes | Yes, with Accessibility permission | Yes | No. The file stays on the clipboard and a notice shows |
| Shortcut passthrough (Alt+Tab, Cmd+Tab, Super) | Experimental, off by default. Alt+Tab and the Windows key take effect on the host only when it runs under the Windows service (installed by the installer) — a host you start yourself captures them but the host's shell ignores them. Ctrl+Alt+Del and Win+L can never be captured | Every Cmd shortcut (Cmd+C/V, Cmd+Tab, Cmd+Space, Cmd+Shift+4, Cmd+Option+D…) and Ctrl+arrows (Spaces, Mission Control) while the remote picture has the keyboard; clicking anywhere else gives them back at once. **Cmd+Q always quits the viewer** (the escape hatch). Cannot be taken: Force Quit (Cmd+Option+Esc), Lock Screen (Cmd+Ctrl+Q), Touch ID / power and the media keys, and anything while Secure Input is on (password fields). With "Translate modifiers" on (Settings → Advanced) Cmd arrives on a Windows/Linux host as Ctrl; with it off, as the Win/Super key | Yes, the window grabs the keyboard | Depends on the compositor. If refused, the app says shortcuts are unsupported here |
| Modifier translation for the remote OS (default on, Settings → Advanced) | From a Mac viewer: Cmd is sent as Ctrl, Cmd+Tab as Alt+Tab, Mac Control as Win. To a Mac host: Ctrl is sent as Cmd, Win as Control, Alt+Tab as Cmd+Tab | Same | Same | Same |
| Lock screen (a user is logged in) | Not yet. The viewer shows "The remote computer is locked" instead of a black screen, and the picture returns by itself after a local unlock (unverified on a device). Reaching it is planned in [plans/10](plans/10-REMOTE-UNLOCK.md) | In scope. Unverified | In scope. Unverified | In scope. Unverified |
| Login window (nobody logged in) | Not yet: someone must sign in on the machine before you can connect. Planned in [plans/10](plans/10-REMOTE-UNLOCK.md) (U6) | No | No | No |
| Hardware video encode | Yes (Media Foundation, H.264 and HEVC) | Yes (VideoToolbox, H.264 and HEVC) | Intel and AMD via VA-API, when libva is 2.20 or newer. NVIDIA via NVENC (optional) | Same as X11 |
| Hardware video decode | Yes (Direct3D 11) | Yes (VideoToolbox). AV1 on M3 and later | Intel and AMD via VA-API, when libva is 2.20 or newer. NVIDIA via NVDEC (optional) | Same as X11 |
| Software video (OpenH264) | Not yet (planned: downloaded from Cisco on first use) | Yes, downloaded from Cisco on first use | Yes, downloaded from Cisco on first use | Yes, downloaded from Cisco on first use |
| Saved passwords (remembered by default, one per computer and account workspace) | Yes (Windows DPAPI) | Yes (Keychain) | Yes with a Secret Service keyring (GNOME Keyring or KWallet). Without one, nothing is saved | Same as X11 |
| Start at sign-in | Yes (per-user; the installed app leaves this to the service instead, which starts the host at every sign-in) | Yes (LaunchAgent) | Yes (XDG autostart) | Yes (XDG autostart) |

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

**Tabs: several computers at once** (native app). Each remote computer you connect
to opens as a tab in the strip at the top of the window; **+** opens the connect
page for another one. Only the tab on screen shows its picture, takes your keyboard,
mouse and clipboard, and holds shortcut capture; the computers in background tabs
pause their video (an older version on the other side keeps sending, and the tab
keeps decoding it without drawing it). Switch tabs by clicking or with
**Ctrl+Alt+PageDown / PageUp** (**Cmd+Option+PageDown / PageUp** on macOS); that chord
always stays on this computer. Double-click a tab to rename the computer, drag it to
reorder, **×** to close it. Each tab shows its own password prompt, connection
progress and why it disconnected. Passwords are remembered by default (one per
computer, per account workspace); a tab that signs in by itself says so and offers
**Forget saved password**. **Connect all** connects every disconnected tab, one
every half second; going past 8 connected tabs asks first. The open tabs, their
names and order come back after a restart, disconnected — nothing connects by
itself. Closing the app ends every session first. While another computer is viewing
this one, no tab can connect (and the other way round).

**Tabs in windows of their own** (native app). **⧉** on a tab (or **Detach** in the
session bar) moves that computer into a normal window you can move, resize, maximise
and put on another screen — the connection stays, nothing reconnects. Every such
window shows its computer live at the same time as the tab on screen in the main
window; the window you are in takes the keyboard, the clipboard and shortcut capture
(the clipboard stays with the last one you used while another app is in front). A
minimised window pauses its computer, as a background tab does (on Wayland a window
cannot tell it is minimised, so it keeps streaming). **Dock** puts it back at the end
of the strip as the tab on screen; **×** on the window disconnects and closes that tab.
Tab-switch chords do nothing inside such a window. Which tabs are in windows, and
where — as a share of the screen, so it fits another computer's screen too — comes
back at the next launch and follows your account (on Wayland only the size and state,
not the position). Device checks pending: `plans/checklists/DETACH-W3.md`.

**Accounts: your tabs, names and settings on every computer** (native app, ⚙ Settings
→ Account). Accounts are optional; connecting never needs one. Without one,
everything is kept on this computer. An account owns a whole **workspace**: the
open tabs and their order, the names you give computers, the recents (and
**Clear**), the synced preferences (modifier translation, shortcut capture,
clipboard sharing, pasting dropped files) and the saved host passwords (these stay
on each computer, never synced). Creating an account takes this computer's
workspace with it. Signing in to an existing one while this computer has its own
tabs or names asks once: **Add** them to the account, or **Keep separate**. Several
accounts can be known on one computer; switching, signing out or removing one
disconnects every tab, and a signed-out account's workspace is hidden until it is
signed in again (the computer's own workspace is always there). The app syncs at
sign-in and at startup, shortly after any change, when the window comes to the
front, and every 5 minutes; **Sync now** forces one. The newer change wins: a
rename, a reorder, a close. A tab closed on another computer while it is connected
here stays until it disconnects. Closed tabs are remembered for 90 days, so a
computer offline for longer than that may bring one back.

- The account lives on the signaling server in **Server URL** (`wss://host/ws` →
  `https://host/api/...`; see `server/src/accounts.js`). The server stores a
  PBKDF2 hash of the password, hashes of the sign-in tokens, and the ID list
  (IDs, names, and last-connected times).
- The sign-in token is kept in the OS secret store (Windows DPAPI, the macOS
  Keychain, a Secret Service keyring on Linux), never in `config.json`; without
  **Keep me signed in** it is not kept at all. It can only read and replace the
  account's workspace. Connecting to a machine still needs that machine's approval
  or password. Wrong passwords lock sign-in to the account for 15 minutes after
  10 tries. With the `AUTH_LIMITER` binding, each IP address is also limited to 20
  sign-in or sign-up attempts per minute.
- Deploying this server version adds the `AccountStore` Durable Object (migration
  `v2`) and the `AUTH_LIMITER` rate limit (`server/wrangler.jsonc`). Older apps
  keep working unchanged. To check the API against `wrangler dev`, run
  `npm run test:accounts` in `server/`.

## Updating

ShareCtrlScreen updates itself. **⚙ Settings → Updates** shows the current version
and a **Check for updates** button, and the app also checks quietly at startup (it
never blocks launching or hosting). When a newer release exists it shows the version
and release notes with an **Update & restart** button; clicking it downloads the
update, verifies its signature, installs it and restarts the app so you land straight
back in, ready to reconnect. If a session is live the button becomes **End session &
update** and warns you first — updating closes the app and ends the session.

The feed is the GitHub release asset `latest.json` on the "latest" release; each update
payload is signed and verified against the public key built into the app, so a tampered
download is refused. Updatable formats: the **Windows installer**, the **macOS `.dmg`**
app and the **Linux AppImage**. The **`.deb`/`.rpm`** packages do not self-update — update
those through your package manager or download the new one from Releases.

On a **service install** (the Windows installer) the update runs the installer silently,
which stops the service, replaces the files and restarts the service — the service then
relaunches the host, so there is never a second instance. On a portable install the app
restarts itself.

**2.2 changes the settings file** (accounts and tabs). Updating from 2.1.x moves
everything over and keeps a copy of the old file (`config.v1.json`) next to it.
Going back to a 2.1.x version afterwards works, but it shows no recents or names and
is signed out; updating to 2.2 again brings everything back, with what 2.1.x recorded
meanwhile merged in.

## Packaging

Releases are built by the **Release** workflow (`.github/workflows/release.yml`). To cut one,
set `version` under `[workspace.package]` in `native/Cargo.toml` (the only copy of the
version), commit, and push the tag `v<version>`. The workflow checks the tag against
`cargo metadata`, runs the licence gate, builds Windows, macOS and Linux, and publishes a
GitHub Release with these assets:

| Asset | For |
|---|---|
| `ShareCtrlScreen_<version>_x64-setup.exe` | Windows installer (not code-signed: SmartScreen warns, More info → Run anyway) |
| `sharectrl.exe` | Windows portable app, the same binary the installer puts in Program Files; run it from any folder (needs the WebView2 runtime, present on Windows 10/11) |
| `sharectrl-service.exe` | Windows service that starts the host at every sign-in (it does not reach UAC prompts or the lock screen yet, see [plans/10](plans/10-REMOTE-UNLOCK.md)). The installer already bundles, registers and starts it; this standalone copy is only for a manual install beside a portable `sharectrl.exe` (`--install-service`, it launches the host from its own folder) |
| `ShareCtrlScreen_<version>_universal.dmg` | macOS 13+, Apple Silicon and Intel |
| `ShareCtrlScreen_<version>_amd64.deb` / `ShareCtrlScreen-<version>-1.x86_64.rpm` / `ShareCtrlScreen_<version>_amd64.AppImage` | Linux |
| `THIRD-PARTY-LICENSES-<os>.html` | Licences of the open-source code in that OS's build (also inside each installer) |
| `latest.json` + per-bundle `.sig` files | The in-app updater feed and signatures (see **Updating**). Built only when the repo secret `TAURI_SIGNING_PRIVATE_KEY` is set; forks without it ship installers that do not self-update |
| `SHA256SUMS.txt` | Check downloads with `sha256sum -c SHA256SUMS.txt` |

The in-app updater needs a one-time keypair, separate from code signing: run
`native/scripts/updater-make-key.sh`, set the repository secrets
`TAURI_SIGNING_PRIVATE_KEY` and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` from the files it
writes, and paste the public key it prints into `native/app/tauri.conf.json`
(`plugins.updater.pubkey`). A full end-to-end update needs **two** updater-enabled releases
— one to ship the mechanism, the next to update to.

**Run workflow** in Actions with `dry_run` ticked builds every asset as run artifacts and
publishes nothing. To build this machine's assets locally the same way, run
`native/scripts/release-dry-run.sh` (it lists what it needs); the results land in
`native/dist/`. The legacy Electron app can still be packaged with `cd app && npm run dist`.

## Known limitations (v1)

- **Platforms.** Windows is shipped. macOS 13+ and Linux (Ubuntu 22.04+) are in the
  port; see [What works where](#what-works-where) for each feature.
- **Login window.** When nobody is logged in, the sign-in screen cannot be shared or
  controlled on macOS or Linux, and not yet on Windows (planned in
  [plans/10](plans/10-REMOTE-UNLOCK.md), U6). The lock screen with a user logged in is in
  scope on all three systems.
- **Lock screen and UAC prompts (Windows).** Not reachable yet, with or without the service.
  While one is up the viewer shows a message instead of the picture and the remote mouse and
  keyboard do nothing; the picture comes back by itself once someone at that computer
  unlocks it or answers the prompt. Planned in [plans/10](plans/10-REMOTE-UNLOCK.md).
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
- **Switching monitors mid-session:** without reconnecting, the host can change the shared
  monitor from its session card, and a viewer **with control** can switch from the session bar
  (Windows) or the **Monitor** menu (macOS/Linux). A view-only viewer cannot. The switch lasts
  for the session; "Monitor to share" stays the default. Linux Wayland hosts offer no list (the
  portal picks the output).
- **Pop-out windows (other monitors side by side):** a viewer **with control** can open more of
  the host's monitors, each in its own window (session bar "Pop out…" on Windows, **Monitor →
  Open in New Window** on macOS/Linux; up to 3). Each window has its own video stream and mouse
  mapping; the bandwidth is shared between them. Closing a window stops its stream, and revoking
  control closes them all. The host's session card lists them. Both sides need this version (an
  older peer just doesn't offer it). File drop and shortcut capture work in the main window only.
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
  Whether the **host** acts on a forwarded Alt+Tab or Windows key depends on the
  host having the Windows **UIAccess** token flag: Windows silently drops those
  shell shortcuts from a process without it. The SYSTEM service (installed by the
  installer) sets that flag on the host it launches, so Alt+Tab switches windows
  and the Windows key opens Start on a service-installed host. A host you start
  yourself — from Program Files or the portable copy — does not get the flag; the
  shortcuts are still captured locally but the host's shell ignores them. The
  running host logs which case applies on startup (`UIAccess: granted` / `UIAccess:
  not granted — <reason>` in `app.log`) — that is the readout for the host instance
  the service started. `sharectrl.exe --print-uiaccess` is a separate self-check that
  **prints** that same line for the one invocation you run; a `sharectrl.exe` you run by
  hand is a fresh non-service process, so it always prints `not granted` — useful to
  confirm "not granted", a positive self-check only under the signed build (a later
  release); to confirm a service host is granted, read its `app.log`. Read the printed
  line, not an exit code from `cmd … & echo %errorlevel%`: `sharectrl.exe` is a GUI-
  subsystem binary, so cmd does not wait and reports `0` regardless. For the numeric code
  use PowerShell `(Start-Process sharectrl.exe -ArgumentList '--print-uiaccess' -Wait
  -PassThru).ExitCode` (0 granted / 1 not).
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
