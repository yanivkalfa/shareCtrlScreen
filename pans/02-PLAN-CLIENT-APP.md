# Plan 02 — Electron Client App (Agent B)

> **Prerequisite reading:** `00-OVERVIEW-AND-PROTOCOL.md` — every wire message you send
> or receive is defined there; do not invent fields. You work ONLY inside the `app/`
> directory. The signaling server is being built in parallel by another agent — do NOT
> touch `server/`; until it exists, test against the protocol using milestones M1–M2
> below (they don't need a live server) and use `npx wrangler dev` in `server/` once it
> lands.

---

## 1. Stack (decided — do not substitute)

| Concern | Choice | Notes |
|---|---|---|
| Shell | **Electron `^43.1.1`** (devDependency) | Chromium 150, Node 24. |
| Renderer | Plain HTML/CSS/JS, **no framework, no bundler, no TypeScript** | One window, a few screens — keep it boring. |
| Input injection | **`koffi` `^3.1.1`** (runtime dependency) calling `user32.dll SendInput` | Only maintained option with layout-independent scan-code injection. Full spec in §7. |
| Screen capture | `session.setDisplayMediaRequestHandler` + `getDisplayMedia` | Modern API; no picker (we auto-pick the primary screen). |
| Transport | Browser-native `WebSocket` + `RTCPeerConnection` in the renderer | No libraries needed. |
| Packaging (last milestone only) | `electron-builder` `^26` | Windows `nsis` + `portable` targets. |

Both roles (host and viewer) live in the same app/window.

## 2. File layout

```
app/
├─ package.json
├─ main/
│  ├─ main.js       app entry: windows, display-media handler, security, IPC wiring
│  ├─ config.js     config load/save, UUID generation, password hashing
│  └─ input.js      koffi + SendInput wrapper (the ONLY file that touches koffi)
├─ preload.js       contextBridge API surface
└─ renderer/
   ├─ index.html    all screens/modals as hidden <div> sections, CSP meta tag
   ├─ style.css     dark, minimal
   ├─ ui.js         screen switching, modals, toasts, settings form
   ├─ signaling.js  WebSocket client: connect, register, ping/pong, reconnect, routing
   ├─ host.js       host role: accept flow, capture, offer, permission enforcement
   └─ viewer.js     viewer role: request flow, answer, video, input capture + math
```

`package.json` essentials: `"main": "main/main.js"`, scripts
`"start": "electron ."`, `"dist": "electron-builder"`, dependency `koffi`,
devDependencies `electron`, `electron-builder`. Add the `build` section in §12.

## 3. Config module (`main/config.js`)

- Path: `path.join(app.getPath('userData'), 'config.json')`.
- Schema and defaults: exactly contract §5. On first run generate
  `uuid = crypto.randomUUID()` (Node's built-in — already lower-case v4) and persist
  immediately. Never regenerate: if the file exists but is corrupt JSON, back it up to
  `config.json.bad` and create a fresh config (new UUID is acceptable in that corner
  case).
- Default `serverUrl`: `ws://localhost:8787/ws` (local dev); the user changes it to the
  deployed `wss://…/ws` in Settings.
- Save = write to `config.json.tmp` then `fs.renameSync` over the original (atomic).
- Password: store only `passwordHash` = SHA-256 hex (Node `crypto.createHash`) of the
  plaintext. Setting an empty password sets `passwordHash: null`.
- Export functions: `load()`, `save(partial)` (shallow-merge + persist),
  `verifyPassword(plain)` (hash & constant-time compare via `crypto.timingSafeEqual`
  on hash buffers; `false` when `passwordHash` is null).

## 4. Main process (`main/main.js`)

### 4.1 Startup order

1. Parse a `--profile <name>` CLI arg (`process.argv`). If present, call
   `app.setPath('userData', <default userData path> + '-' + name)` **before**
   `app.whenReady()`. This is how two instances run on one machine for testing:
   `npm start` and `npm start -- --profile b`. Do NOT use `requestSingleInstanceLock`.
2. `app.whenReady()` → install the display-media handler (§4.2) → create the window
   (§4.3) → register IPC handlers (§4.4).

### 4.2 Display-media handler (host-side screen capture)

On `session.defaultSession.setDisplayMediaRequestHandler(handler, { useSystemPicker: false })`:

- Handler calls `desktopCapturer.getSources({ types: ['screen'] })`.
- Choose the source whose `display_id` equals `String(screen.getPrimaryDisplay().id)`;
  fall back to `sources[0]`.
- Call `callback({ video: chosenSource })` — **video only in v1** (no audio; system
  loopback audio is a v2 item).
- Wrap everything in try/catch and call `callback({})` on error — an unhandled
  rejection here hangs the renderer's `getDisplayMedia` promise forever (known
  Electron issue).

### 4.3 Window & security

- One `BrowserWindow`: 1050×720, `minWidth` 800, `minHeight` 560, dark
  `backgroundColor`, `webPreferences: { preload: <abs path>, contextIsolation: true,
  sandbox: true, nodeIntegration: false }` (the last two restate defaults — keep them
  explicit).
- Deny navigation: on `webContents.will-navigate` → `event.preventDefault()`; and
  `setWindowOpenHandler(() => ({ action: 'deny' }))`.
- `index.html` carries a CSP meta tag:
  `default-src 'self'; style-src 'self' 'unsafe-inline'; connect-src ws: wss:; media-src 'self' blob:; img-src 'self' data:`.
- Set `app.commandLine.appendSwitch('autoplay-policy', 'no-user-gesture-required')`
  before ready (belt-and-braces so the incoming video element always plays).

### 4.4 IPC surface (all `ipcMain.handle`, all validated)

Every handler first checks `event.senderFrame === win.webContents.mainFrame`
(drop otherwise). Channels:

| Channel | Args → Returns | Behavior |
|---|---|---|
| `config:get` | → `{uuid, serverUrl, mode, passwordPermission, hasPassword, iceServers}` | Never returns the hash itself. |
| `config:set` | `{serverUrl?, mode?, password?, passwordPermission?, iceServers?}` → updated same shape as `config:get` | `password` arrives as plaintext (or `""` to clear) and is hashed here. Validate: `mode` ∈ {`approve`,`password`}, `passwordPermission` ∈ {`view`,`control`}, `serverUrl` starts with `ws://` or `wss://`. Reject invalid with a thrown Error. |
| `password:verify` | `plain` → `boolean` | Calls `config.verifyPassword`. **On mismatch, `await` a 2000 ms delay before returning false** (contract §6 brute-force damper). |
| `screen:size` | → `{w, h}` | Primary display in **physical pixels**: `screen.getPrimaryDisplay().size` × its `scaleFactor`, rounded. (Used only for diagnostics/v2; injection uses normalized coords.) |
| `input:inject` | one input-event object (contract §4.1 shape) → `void` | Validate strictly BEFORE touching `input.js`: `t` ∈ {`mm`,`md`,`mu`,`wh`,`kd`,`ku`}; `x`/`y` present where required, finite numbers, clamped to [0,1]; `b` ∈ {0,1,2}; `dx`/`dy` finite, clamped to [-1200,1200]; `code` a string ≤ 32 chars. Then dispatch to the matching `input.js` function. Renderer already gates on permission, but main re-checks shape because it is the privileged side. |

## 5. Preload (`preload.js`)

`contextBridge.exposeInMainWorld('native', {...})` with one wrapper function per IPC
channel above (each just `ipcRenderer.invoke`). Expose nothing else — no raw
`ipcRenderer`, no Node globals.

## 6. Renderer — state machine and UI (`ui.js`, `index.html`)

One global state variable; every state maps to exactly one visible screen/modal:

| State | What's on screen |
|---|---|
| `OFFLINE` | Home screen, red status dot "server: disconnected(retrying)". Connect button disabled. |
| `READY` | Home: **my UUID** in large monospace + Copy button; green dot; input box "Remote ID" + Connect button; ⚙ Settings button. |
| `REQUESTING` | Home + blocking modal "Connecting to <uuid>… (cancel)" with 30 s countdown. |
| `PASSWORD_PROMPT` | Modal with password field + OK/Cancel (viewer side, after `password-required`). |
| `INCOMING` | Modal (host side): "**<from-uuid>** wants to connect" + three buttons: `Deny`, `Allow view only`, `Allow view + control`, plus "auto-deny in NN s" 30 s countdown. |
| `HOST_ACTIVE` | Home replaced by session panel: "Sharing screen with <uuid>", a `<select>` with `view`/`control` (live permission), red "End session" button. |
| `VIEW_ACTIVE` | Full-window black area with `<video>` centered (letterboxed, `object-fit: contain`); slim top overlay bar: peer uuid, badge `VIEW ONLY`/`CONTROL`, Disconnect button. |

Also: a toast helper for errors ("Peer offline", "Wrong password", "Denied", "Busy",
"Connection lost"), and the Settings modal:

- Server URL (text input)
- Mode (radio: "Ask me for each connection" = `approve` / "Password" = `password`)
- Password (password input; placeholder "(unchanged)" when `hasPassword`; a "clear" checkbox)
- "When password is used, allow" (select: `view` / `control`)
- Saved via `config:set`; mode/permission changes apply to the NEXT incoming request
  (host.js re-reads config on every `connect-request`); serverUrl change triggers
  signaling reconnect.

## 7. Input injection (`main/input.js`) — full specification

Uses koffi. The koffi docs' union example is literally SendInput
(https://koffi.dev/unions) — follow that shape exactly.

### 7.1 Declarations

- `koffi.load('user32.dll')`.
- Structs (names/fields exact; koffi computes x64 alignment for you — do NOT hand-pack):
  - `MOUSEINPUT`: `dx: 'long'`, `dy: 'long'`, `mouseData: 'int32'` *(Win32 declares
    `DWORD`, but declare **int32** so negative wheel deltas can be passed directly —
    identical byte layout)*, `dwFlags: 'uint32'`, `time: 'uint32'`,
    `dwExtraInfo: 'uintptr_t'`.
  - `KEYBDINPUT`: `wVk: 'uint16'`, `wScan: 'uint16'`, `dwFlags: 'uint32'`,
    `time: 'uint32'`, `dwExtraInfo: 'uintptr_t'`.
  - `HARDWAREINPUT`: `uMsg: 'uint32'`, `wParamL: 'uint16'`, `wParamH: 'uint16'`.
  - Union `INPUT_U`: `{ mi: MOUSEINPUT, ki: KEYBDINPUT, hi: HARDWAREINPUT }`.
  - `INPUT`: `{ type: 'uint32', u: INPUT_U }`.
- Function: `SendInput` with prototype `uint32 SendInput(uint32 cInputs, INPUT *pInputs, int cbSize)`;
  always pass `cbSize = koffi.sizeof(INPUT)` (must come out as **40** on x64 — assert
  this once at module load and throw if not).
- INPUT `type` values: mouse `0`, keyboard `1`.

### 7.2 Flag constants

| Constant | Value |
|---|---|
| `MOUSEEVENTF_MOVE` | `0x0001` |
| `MOUSEEVENTF_LEFTDOWN` / `LEFTUP` | `0x0002` / `0x0004` |
| `MOUSEEVENTF_RIGHTDOWN` / `RIGHTUP` | `0x0008` / `0x0010` |
| `MOUSEEVENTF_MIDDLEDOWN` / `MIDDLEUP` | `0x0020` / `0x0040` |
| `MOUSEEVENTF_WHEEL` / `MOUSEEVENTF_HWHEEL` | `0x0800` / `0x1000` |
| `MOUSEEVENTF_ABSOLUTE` | `0x8000` |
| `KEYEVENTF_EXTENDEDKEY` | `0x0001` |
| `KEYEVENTF_KEYUP` | `0x0002` |
| `KEYEVENTF_SCANCODE` | `0x0008` |

### 7.3 Functions (each builds one INPUT and calls SendInput(1, [input], cbSize))

- **`mouseMove(nx, ny)`** — `dx = Math.round(nx * 65535)`, `dy = Math.round(ny * 65535)`,
  flags `MOVE | ABSOLUTE`. (v1 captures the **primary display only**, and plain
  `ABSOLUTE` coordinates map to the primary display — so this is exactly right. No
  `VIRTUALDESK`, no DPI math needed: normalized in, normalized out.)
- **`mouseButton(b, isDown, nx, ny)`** — first call `mouseMove(nx, ny)`, then send the
  button flag from `{0: LEFT, 1: MIDDLE, 2: RIGHT} × {down, up}`.
- **`wheel(dx, dy)`** — if `dy ≠ 0`: one event, flags `WHEEL`, `mouseData = dy`.
  If `dx ≠ 0`: one event, flags `HWHEEL`, `mouseData = dx`. (Values arrive already in
  Windows wheel units from the viewer — see §9.4.)
- **`key(code, isDown)`** — look up `code` in the table below → `{sc, ext}`; unknown
  code → return silently. Build KEYBDINPUT: `wVk = 0`, `wScan = sc`,
  `dwFlags = SCANCODE | (ext ? EXTENDEDKEY : 0) | (isDown ? 0 : KEYUP)`.

### 7.4 Complete DOM `KeyboardEvent.code` → scan-code table (Set 1)

Non-extended (`ext: false`):

| code | sc | code | sc | code | sc | code | sc |
|---|---|---|---|---|---|---|---|
| Escape | 0x01 | Digit1 | 0x02 | Digit2 | 0x03 | Digit3 | 0x04 |
| Digit4 | 0x05 | Digit5 | 0x06 | Digit6 | 0x07 | Digit7 | 0x08 |
| Digit8 | 0x09 | Digit9 | 0x0A | Digit0 | 0x0B | Minus | 0x0C |
| Equal | 0x0D | Backspace | 0x0E | Tab | 0x0F | KeyQ | 0x10 |
| KeyW | 0x11 | KeyE | 0x12 | KeyR | 0x13 | KeyT | 0x14 |
| KeyY | 0x15 | KeyU | 0x16 | KeyI | 0x17 | KeyO | 0x18 |
| KeyP | 0x19 | BracketLeft | 0x1A | BracketRight | 0x1B | Enter | 0x1C |
| ControlLeft | 0x1D | KeyA | 0x1E | KeyS | 0x1F | KeyD | 0x20 |
| KeyF | 0x21 | KeyG | 0x22 | KeyH | 0x23 | KeyJ | 0x24 |
| KeyK | 0x25 | KeyL | 0x26 | Semicolon | 0x27 | Quote | 0x28 |
| Backquote | 0x29 | ShiftLeft | 0x2A | Backslash | 0x2B | KeyZ | 0x2C |
| KeyX | 0x2D | KeyC | 0x2E | KeyV | 0x2F | KeyB | 0x30 |
| KeyN | 0x31 | KeyM | 0x32 | Comma | 0x33 | Period | 0x34 |
| Slash | 0x35 | ShiftRight | 0x36 | NumpadMultiply | 0x37 | AltLeft | 0x38 |
| Space | 0x39 | CapsLock | 0x3A | F1 | 0x3B | F2 | 0x3C |
| F3 | 0x3D | F4 | 0x3E | F5 | 0x3F | F6 | 0x40 |
| F7 | 0x41 | F8 | 0x42 | F9 | 0x43 | F10 | 0x44 |
| NumLock | 0x45 | ScrollLock | 0x46 | Numpad7 | 0x47 | Numpad8 | 0x48 |
| Numpad9 | 0x49 | NumpadSubtract | 0x4A | Numpad4 | 0x4B | Numpad5 | 0x4C |
| Numpad6 | 0x4D | NumpadAdd | 0x4E | Numpad1 | 0x4F | Numpad2 | 0x50 |
| Numpad3 | 0x51 | Numpad0 | 0x52 | NumpadDecimal | 0x53 | IntlBackslash | 0x56 |
| F11 | 0x57 | F12 | 0x58 | | | | |

Extended (`ext: true`):

| code | sc | code | sc |
|---|---|---|---|
| NumpadEnter | 0x1C | ControlRight | 0x1D |
| NumpadDivide | 0x35 | AltRight | 0x38 |
| Home | 0x47 | ArrowUp | 0x48 |
| PageUp | 0x49 | ArrowLeft | 0x4B |
| ArrowRight | 0x4D | End | 0x4F |
| ArrowDown | 0x50 | PageDown | 0x51 |
| Insert | 0x52 | Delete | 0x53 |
| MetaLeft | 0x5B | MetaRight | 0x5C |
| ContextMenu | 0x5D | PrintScreen | 0x37 |

Deliberately unsupported (ignore silently): `Pause` (uses an 0xE1 prefix sequence),
media keys, `Fn`.

## 8. Signaling client (`signaling.js`)

- Connects to `config.serverUrl`; on `open` sends `register` (contract §3.1); on
  `registered` → state `READY`.
- **Ping loop:** every 25 s send the exact literal string `{"type":"ping"}` (send the
  hard-coded string, NOT `JSON.stringify` of an object — key order must be byte-exact,
  contract §3.3). Track pong arrival; 2 consecutive misses (10 s watchdog each) →
  `ws.close()` → reconnect path.
- **Reconnect:** on close/error → state `OFFLINE`, retry with backoff 1, 2, 4, 8, 16,
  30, 30… seconds, re-register each time. If a session is active when the socket
  drops, the session **continues** (WebRTC is independent) — just show a yellow dot.
- On `register-error` `duplicate`: show a persistent error banner "This ID is already
  online elsewhere" and retry every 30 s.
- **Router:** parse each message; dispatch by `type` to host.js
  (`connect-request`), viewer.js (`connect-response`, `password-required`), and
  whichever role is active (`signal`, `end-session`, `relay-error`). Unknown types
  ignored.

## 9. Roles

### 9.1 Host flow (`host.js`) — numbered, follow exactly

1. On `connect-request` (with `from`): if state is not `READY` → send
   `connect-response {accepted:false, reason:"busy"}` and stop.
2. Re-read config. Effective mode = `password` only if `mode==="password"` AND
   `hasPassword`; else `approve` (contract §5 fail-safe).
3. `approve` mode → state `INCOMING`, show modal with `from`. Buttons resolve to
   `deny` / `view` / `control`; 30 s → auto-`deny` with reason `"timeout"`.
4. `password` mode → if `msg.password == null` → send `password-required` to `from`,
   stay `READY`, stop. Else `await native.passwordVerify(msg.password)`:
   false → `connect-response {accepted:false, reason:"bad-password"}` (the 2 s damper
   already happened in main); true → permission = config `passwordPermission`, go to 5.
5. **Accept path:** send `connect-response {accepted:true, permission}` FIRST, then:
   a. `stream = await navigator.mediaDevices.getDisplayMedia({ video: { frameRate: { ideal: 30, max: 60 } }, audio: false })`
      — the main-process handler answers it (no picker). On failure send
      `end-session` and toast.
   b. `track = stream.getVideoTracks()[0]`; set `track.contentHint = 'detail'`.
   c. `pc = new RTCPeerConnection({ iceServers: config.iceServers })`.
   d. Create BOTH data channels before the offer: `ctl = pc.createDataChannel('ctl', {ordered:true})`,
      `mm = pc.createDataChannel('mm', {ordered:false, maxRetransmits:0})`.
   e. `sender = pc.addTrack(track, stream)`.
   f. Tune (wrap in try/catch; skip silently on any error): via
      `sender.getParameters()`/`setParameters()` set `degradationPreference:
      'maintain-resolution'` and `encodings[0].maxBitrate = 4_000_000`,
      `encodings[0].scalabilityMode = 'L1T1'`. Then codec preference: find the video
      transceiver, `setCodecPreferences` ordering `RTCRtpSender.getCapabilities('video').codecs`
      as AV1 first, then VP9, then H.264, then the rest.
   g. `pc.onicecandidate` → send `signal {data:{kind:"ice", candidate: <serialized>}}`;
      `createOffer` → `setLocalDescription` → send `signal {data:{kind:"offer", sdp: pc.localDescription.sdp}}`.
   h. Handle inbound `signal` messages: `answer` → `setRemoteDescription({type:'answer',sdp})`;
      `ice` → `addIceCandidate`.
6. `ctl.onopen` → send initial `{"t":"perm","value":<permission>}`; state `HOST_ACTIVE`.
7. **Input handling** (`ctl.onmessage` + `mm.onmessage`): parse JSON; if current
   permission is `"control"` and `t` ∈ {mm,md,mu,wh,kd,ku} → `native.inputInject(msg)`
   (fire-and-forget). If permission is `"view"` → drop.
8. Permission `<select>` change → update local variable + send `{"t":"perm","value":…}`.
9. End session (button, or received `bye`, or `ctl.onclose`, or
   `pc.onconnectionstatechange` ∈ {failed, closed} or `disconnected` persisting 8 s):
   send `{"t":"bye"}` on ctl if open, send `end-session` via signaling (best-effort),
   stop all tracks, `pc.close()`, state `READY`.

### 9.2 Viewer flow (`viewer.js`)

1. Connect button → validate UUID format locally → send
   `connect-request {to, password:null}` → state `REQUESTING`, start 30 s timer.
2. On `password-required` → state `PASSWORD_PROMPT`; OK → resend
   `connect-request {to, password:<entered>}` → `REQUESTING` (fresh 30 s).
3. On `connect-response`: `accepted:false` → toast the reason (map: `denied`→"Denied by
   remote user", `busy`→"Remote is in another session", `bad-password`→"Wrong password"
   (reopen prompt), `timeout`→"No answer") → `READY`. `accepted:true` → remember
   `permission`, wait for the offer.
4. On `signal offer`: `pc = new RTCPeerConnection({iceServers})`;
   `pc.ondatachannel` → stash channels by `event.channel.label` (`ctl`/`mm`);
   `pc.ontrack` → `video.srcObject = event.streams[0]`; then try
   `event.receiver.jitterBufferTarget = 0` (try/catch);
   `setRemoteDescription({type:'offer', sdp})` → `createAnswer` →
   `setLocalDescription` → send `signal {data:{kind:"answer", sdp}}`;
   `onicecandidate` → send ice; inbound `ice` → `addIceCandidate`. State `VIEW_ACTIVE`
   on first `ctl` open. `relay-error peer-offline` at any point → teardown + toast.
5. `ctl.onmessage`: `perm` → update badge + enable/disable input capture;
   `bye` → teardown.
6. Teardown (Disconnect button, `bye`, `ctl.onclose`, pc failed): send `{"t":"bye"}` if
   ctl open, `pc.close()`, clear `video.srcObject`, state `READY`.

### 9.3 Viewer input capture (only while badge = CONTROL)

Attach listeners to the video **container** element:

- **Coordinate mapping** (the video is letterboxed with `object-fit: contain`; you must
  map into the actual picture, not the black bars):
  - `rect = video.getBoundingClientRect()`; `va = video.videoWidth / video.videoHeight`
    (bail if `videoWidth` is 0); `ba = rect.width / rect.height`.
  - If `ba > va` (pillarbox): `contentH = rect.height`, `contentW = contentH * va`;
    else (letterbox): `contentW = rect.width`, `contentH = contentW / va`.
  - `offX = rect.left + (rect.width − contentW) / 2`; `offY = rect.top + (rect.height − contentH) / 2`.
  - `nx = (e.clientX − offX) / contentW`, `ny = (e.clientY − offY) / contentH`.
  - If `nx` or `ny` outside [0,1] → ignore the event (cursor over the bars).
- **mousemove** → coalesce: store latest `{nx,ny}`; a `requestAnimationFrame` loop
  sends at most one `{"t":"mm",...}` per frame on the **mm** channel (guard: channel
  `readyState === 'open'`, `bufferedAmount < 65536`).
- **mousedown / mouseup** → `{"t":"md"/"mu","b":e.button,"x":nx,"y":ny}` on **ctl**
  (ignore `e.button` > 2). Also `contextmenu` → `preventDefault()` (so right-click goes
  to the remote machine, not a local menu).
- **wheel** → convert DOM deltas to Windows units (**note the Y sign flip**):
  `dy = e.deltaY === 0 ? 0 : −Math.sign(e.deltaY) × 120`;
  `dx = e.deltaX === 0 ? 0 : Math.sign(e.deltaX) × 120`;
  send `{"t":"wh","dx":dx,"dy":dy}` on ctl. `preventDefault()`.
- **keydown / keyup** (listeners on `window`, active only in `VIEW_ACTIVE` with
  control): if `e.repeat` and it's a `kd`, still send (remote auto-repeat won't happen
  otherwise). Send `{"t":"kd"/"ku","code":e.code}` on ctl; `preventDefault()` +
  `stopPropagation()` for everything except `F11` (leave local fullscreen toggle) —
  note honestly in README: OS-reserved combos (Alt+Tab, Ctrl+Alt+Del, Win+L) can't be
  fully captured.
- **Stuck-key prevention:** keep a `Set` of codes currently down; on window `blur`, on
  permission dropping to `view`, and on teardown → send `ku` for every member, clear it.

## 10. Milestones (build in this order; each has a gate)

| # | Build | Gate (verify before moving on) |
|---|---|---|
| M1 | package.json, main.js window + security, config.js, preload, home screen + settings UI (no networking) | App opens; UUID shows and survives restart; settings persist; `--profile b` gives a second instance with a different UUID. |
| M2 | signaling.js | With server running (`npx wrangler dev` in `server/` — if not yet available, replicate plan 01 §8's wscat checks against your client using a scratch local echo of the contract): green dot, register visible, pings flow, kill server → red dot → restart server → auto-reconnect. |
| M3 | Handshake only (host.js steps 1–4, viewer.js steps 1–3; no WebRTC — accept path just toasts "accepted") | Two profiles on one machine: approve popup + deny/timeout/busy paths; password mode incl. wrong-password (≥2 s delay) and fail-safe when no password set. |
| M4 | WebRTC video (host step 5–6, viewer steps 4–6), view-only | Viewer instance shows the host's live screen; End from both sides works; killing either process returns the other to READY within ~10 s. |
| M5 | input.js + IPC + viewer capture | With permission=control: remote cursor tracks smoothly; all three buttons + double-click; wheel both directions (natural direction!); typing incl. Shift-symbols, arrows, Home/End, numpad; no stuck modifiers after Alt-Tabbing away from the viewer. |
| M6 | Live permission switch + polish (badges, toasts, countdowns) | Contract §7 integration script passes end-to-end, steps 1–7. |
| M7 | electron-builder packaging | `npm run dist` yields an NSIS installer + portable exe; the **packaged** app still injects input (proves asarUnpack worked). |

## 11. Known gotchas (read before coding)

1. **koffi + asar:** native `.node` files can't load from inside the asar archive. In
   the `build` config set `"asarUnpack": ["**/node_modules/koffi/**"]`.
2. **UIPI / UAC:** a non-elevated host can't inject into elevated windows or UAC
   prompts — input silently no-ops. Not a bug; document "run the host as
   administrator" as the workaround.
3. `getDisplayMedia` must be called from the renderer AFTER the handler is installed
   in main; if the promise never settles, the handler threw — see §4.2.
4. `setParameters` / `setCodecPreferences` / `jitterBufferTarget` are tuning: wrap each
   in its own try/catch; the app must work if all of them fail.
5. The `mm` channel may open after `ctl` (or drop messages by design) — never treat an
   `mm` message as required for correctness; clicks carry their own coordinates for
   exactly this reason.
6. Send the ping as the hard-coded 15-char string; a re-serialized object with
   different key order will not match the server's auto-responder and will wake the
   Durable Object (works, but wastes free-tier quota) — and any whitespace difference
   means no pong at all from the auto-responder.
7. Multi-monitor hosts: v1 shares the **primary** display only, and `ABSOLUTE`
   SendInput coordinates target the primary display only — consistent. Don't "fix" one
   side without the other.
8. Two dev instances on one machine both register different UUIDs — but if you copy a
   config file between profiles you'll get `duplicate` register errors; the banner in
   §8 covers this.

## 12. `build` config (electron-builder, in package.json)

- `appId`: `com.sharectrl.screen`; `productName`: `ShareCtrlScreen`.
- `win.target`: `["nsis", "portable"]`.
- `files`: `["main/**", "renderer/**", "preload.js", "package.json"]`.
- `asarUnpack`: `["**/node_modules/koffi/**"]`.
- No code signing in v1 (users will see SmartScreen warnings — expected).
