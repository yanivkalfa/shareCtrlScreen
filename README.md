# ShareCtrlScreen

Screen sharing + remote control between two Windows machines, over the internet.
A viewer watches the host's screen (WebRTC video) and — when the host grants it —
drives the host's mouse and keyboard (data channel → Win32 `SendInput`).

Each installation generates a UUID v4 once and keeps it; that UUID is the
machine's address. To connect somewhere, you type its UUID.

- `server/` — signaling/rendezvous server (Cloudflare Worker + Durable Object). Relays JSON only; never sees pixels or input.
- `app/` — Electron desktop app. Both roles (host and viewer) live in the same binary.
- `pans/` — the design documents. `00-OVERVIEW-AND-PROTOCOL.md` is the wire contract.

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

```bash
cd app
npm run dist
```

Produces an NSIS installer and a portable `.exe` in `app/dist/`. There is no code
signing in v1, so Windows SmartScreen will warn on first run — that is expected.

## Known limitations (v1)

- **Windows only.** Input injection is Win32 `SendInput`.
- **Multi-monitor:** the host can pick which monitor to share in ⚙ Settings →
  "Monitor to share". Sharing the primary uses a simple absolute-coordinate path;
  sharing a non-primary monitor maps input through the Win32 virtual desktop
  (`VIRTUALDESK`, DPI-corrected). Capture and injection stay consistent for the
  chosen monitor. (One monitor at a time; no all-monitors view.)
- **Elevated windows are not controllable.** A non-elevated host cannot inject
  into UAC prompts or apps running as administrator — this is the Windows UIPI
  security boundary, not a bug. Run the host as administrator if you need to
  control elevated apps.
- **Keyboard shortcut passthrough** (experimental, off by default): enable
  ⚙ Settings → "Capture keyboard shortcuts" and, while viewing with control,
  **Alt+Tab / Alt+Esc / the Win keys are captured locally and sent to the remote**
  instead of acting on your machine (a focus-scoped low-level keyboard hook —
  losing window focus releases it). Still **cannot** be captured, by Windows
  design: **Ctrl+Alt+Del** and **Win+L** (kernel/secure-desktop level). F11 is
  deliberately left alone so local fullscreen still works.
- **One session at a time** per machine. A second incoming request gets `busy`.
- **Password uses challenge-response** — the viewer sends a proof derived from a
  host-issued nonce (`SHA256(SHA256(password) + ":" + nonce)`), never the plaintext,
  so the password never transits the relay. Compared host-side against a stored
  SHA-256; the host adds a 2-second delay before rejecting a wrong password. There is
  no stronger rate limiting beyond one-session-at-a-time.
- **System audio** is shared by default (Windows loopback capture, played on the
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
