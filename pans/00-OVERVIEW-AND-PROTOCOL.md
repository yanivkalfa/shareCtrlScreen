# ShareCtrlScreen — Overview & Shared Protocol Contract

> **READ THIS FILE FIRST.** This document is the single source of truth shared by both
> implementation plans. Two agents will build the two halves **simultaneously**:
>
> - Agent A builds **`01-PLAN-SIGNALING-SERVER.md`** (the signaling server)
> - Agent B builds **`02-PLAN-CLIENT-APP.md`** (the Electron desktop app)
>
> The two halves only interoperate if both follow the message schemas in this file
> **exactly — byte for byte on field names and values**. If anything in a sub-plan
> contradicts this file, THIS FILE WINS.

---

## 1. What we are building

A simplified AnyDesk/remote-desktop app for **Windows only**, two computers, over the
internet:

- **Screen share**: the "viewer" sees the "host"'s screen live (WebRTC video).
- **Remote control** (optional, permission-gated): the viewer's mouse moves/clicks/wheel
  and keyboard presses are injected into the host machine (WebRTC data channel → Win32
  `SendInput`).
- **Identity**: each installation generates a **UUID v4 once** and persists it. The UUID
  is the machine's "address". To connect to a machine you type its UUID.
- **Access modes** (host-side local config — governs *incoming* connections):
  - **Mode `approve`** ("open session"): any connection attempt triggers a popup on the
    host showing the requester's UUID with three choices: **Deny**, **Allow view only**,
    **Allow view + control**.
  - **Mode `password`**: host sets a password AND a pre-configured permission level
    (`view` or `control`). A connecting viewer must submit the password; if correct they
    are admitted automatically with exactly that pre-configured permission. No popup.
- **Live permission switching**: during an active session the host can switch the
  granted permission between `view` and `control` at any time from the app UI, and can
  end the session.

### Terminology (used consistently in all three documents)

| Term | Meaning |
|---|---|
| **Host** | The machine being viewed/controlled. Shares its screen, receives input. |
| **Viewer** | The machine that connects out, watches the screen, sends input. |
| **Server** | The tiny signaling/rendezvous server. Relays JSON only. Never sees pixels or input. |
| **Permission** | Either the string `"view"` or the string `"control"`. Never anything else. |
| **UUID** | Lower-case UUID v4 string, e.g. `"1c9a7b3e-8f21-4d6a-9e0b-2f4c8a1d5e73"`. |

Any single app instance can act as host and viewer (same binary, same UI); a machine
supports **one active session at a time** (v1 simplification).

---

## 2. Architecture (why each piece exists)

```
Viewer app  ── wss ──►  Signaling server  ◄── wss ──  Host app
     │        (JSON: connect req/resp, SDP, ICE)          │
     │                                                    │
     └────────────── WebRTC peer connection ──────────────┘
            • video track  : host screen → viewer
            • data channel : viewer input → host,
                             host control msgs → viewer
```

- UUIDs are not routable addresses, so both apps hold a **persistent WebSocket** open to
  the server. The server keeps a map `uuid → socket` and **relays** messages between the
  two. That is its entire job — it is stateless beyond that map, stores nothing on disk,
  and never carries video or input data.
- Once signaling completes, all heavy traffic (video + input) flows **directly
  peer-to-peer** over WebRTC (with STUN for NAT hole-punching; TURN as relay fallback —
  see hosting plan `03-HOSTING.md`).

---

## 3. WebSocket signaling protocol (Server ⇄ App) — THE CONTRACT

Transport: WebSocket, **text frames**, each frame is exactly one JSON object (UTF-8).
No batching, no binary frames. Unknown message types MUST be ignored silently (forward
compatibility). Every message has a `type` field (string).

Routing convention: messages from an app that must reach the other peer carry a `to`
field (target UUID). The server replaces `to` with a `from` field (the sender's
registered UUID, **taken from the server's own registry — never trusted from the message
body**) and forwards the rest of the object unchanged to the target's socket.

### 3.1 Registration

| Direction | Message | Notes |
|---|---|---|
| App → Server | `{"type":"register","uuid":"<my-uuid>","v":1}` | First message after connecting. `v` is protocol version, always `1`. |
| Server → App | `{"type":"registered","uuid":"<uuid>"}` | Success ack. |
| Server → App | `{"type":"register-error","reason":"duplicate"}` | UUID already online elsewhere. Reasons: `"duplicate"`, `"invalid-uuid"`, `"bad-version"`. Server then closes the socket. |

- Server MUST validate `uuid` against UUID-v4 regex
  `^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$`.
- Any non-`register` message from an unregistered socket → server closes the socket.
- If a socket disconnects, the server removes it from the registry immediately.

### 3.2 Connection handshake (viewer wants to see host)

All of the following are **relayed** messages (app sends with `to`, receiver gets `from`).

**Step 1 — Viewer → Host:** connection request

```json
{"type":"connect-request","to":"<host-uuid>","password":null}
```

`password` is `null` on the first attempt, or the **proof** string (not the
plaintext) when responding to `password-required` — see the challenge-response
note under Step 2.

**Step 2 — Host decides.** Host behavior on receiving `connect-request` (with `from`):

- If host already has an active session → reply `connect-response` with
  `"accepted":false,"reason":"busy"`.
- If host mode is `approve` → show popup with `from` UUID; the human picks
  Deny / View only / View + control.
- If host mode is `password` (challenge-response — see note below):
  - request's `password` is `null` → reply
    `{"type":"password-required","to":"<viewer-uuid>","nonce":"<random-hex>"}`.
    `nonce` is 16 random bytes as lower-case hex, fresh per attempt.
  - request's `password` present (it is a **proof**, not the plaintext) & correct →
    auto-accept with the config's pre-set permission
  - proof present & wrong → `connect-response` with `"accepted":false,"reason":"bad-password"`

> **Password challenge-response (v1.1).** The viewer never transmits the password.
> On `password-required` it computes
> `proof = SHA256( SHA256(plaintext) + ":" + nonce )` and puts that in the
> `connect-request` `password` field. The host stores only `SHA256(plaintext)`
> (`passwordHash`), so it recomputes the same proof and compares. The plaintext
> never leaves the viewer and never transits the relay. The server relays the
> `nonce`/proof fields opaquely, so this needed no server change.

**Step 3 — Host → Viewer:** decision

```json
{"type":"connect-response","to":"<viewer-uuid>","accepted":true,"permission":"view"}
{"type":"connect-response","to":"<viewer-uuid>","accepted":false,"reason":"denied"}
```

`permission` present only when `accepted` is `true`; one of `"view" | "control"`.
`reason` present only when `accepted` is `false`; one of
`"denied" | "busy" | "bad-password" | "timeout"`.

**Step 4 — WebRTC negotiation.** The **host is the offerer** (it owns the media). Host
captures the screen, then exchanges SDP/ICE via:

```json
{"type":"signal","to":"<peer-uuid>","data":{"kind":"offer","sdp":"..."}}
{"type":"signal","to":"<peer-uuid>","data":{"kind":"answer","sdp":"..."}}
{"type":"signal","to":"<peer-uuid>","data":{"kind":"ice","candidate":{...}}}
```

`data.candidate` is the serialized `RTCIceCandidate` (`candidate`, `sdpMid`,
`sdpMLineIndex` fields). The server treats `data` as an opaque blob.

**Step 5 — Session end (either side, via server, best-effort):**

```json
{"type":"end-session","to":"<peer-uuid>"}
```

Apps must ALSO treat data-channel close / ICE `disconnected`→`failed` as session end,
because the peer may vanish without sending this.

### 3.3 Server-generated errors & liveness

| Message | When |
|---|---|
| `{"type":"relay-error","reason":"peer-offline","to":"<uuid-you-tried>"}` | Sent back to a sender whose `to` target is not registered. |
| `{"type":"pong"}` | Reply to an app-sent `{"type":"ping"}` (see below). |

**Liveness is client-driven** (this exact design lets the server hibernate on
Cloudflare's free tier — see server plan): every app sends the **exact literal text**
`{"type":"ping"}` every **25 seconds**. The server answers with the exact literal text
`{"type":"pong"}`. These two strings must be byte-exact (no extra whitespace, key order
as shown) because the server matches the ping as a raw string, not parsed JSON. If an
app misses 2 consecutive pongs (no pong within ~10 s of a ping, twice) it must assume
the connection is dead: close the socket and reconnect with exponential backoff
(1 s, 2 s, 4 s … capped at 30 s), re-`register` on every reconnect.

Timeouts owned by the **viewer**: if no `connect-response` (or `password-required`)
arrives within **30 s** of sending `connect-request`, show "no answer / timed out" and
abort. The host popup auto-dismisses (auto-deny, reason `"timeout"`) after **30 s**.

---

## 4. WebRTC data-channel protocol (Viewer ⇄ Host) — THE CONTRACT

**Two data channels**, both created **by the host** when building the offer (the viewer
receives them via `ondatachannel` and matches them by label):

| Label | Options | Carries |
|---|---|---|
| `"ctl"` | `{ ordered: true }` (fully reliable — the default) | Everything EXCEPT mouse moves: `md`, `mu`, `wh`, `kd`, `ku`, `perm`, `bye`. A lost/reordered key-up or mouse-up would cause stuck keys — these must be reliable. |
| `"mm"` | `{ ordered: false, maxRetransmits: 0 }` (unreliable) | ONLY `mm` mouse-move messages. A lost move is instantly superseded by the next one; unreliable delivery avoids head-of-line blocking and keeps cursor latency minimal. |

All messages are JSON text (single object per message). Compact `t` field = type.
Both sides must tolerate the `mm` channel delivering nothing (e.g. before it opens).

### 4.1 Viewer → Host: input events (host MUST ignore all of these unless current permission is `"control"`)

| Message | Channel | Meaning / fields |
|---|---|---|
| `{"t":"mm","x":0.5321,"y":0.201}` | `mm` | Mouse move. `x`,`y` are **normalized floats in [0,1]** relative to the full captured screen (top-left origin). Sender throttles to ≤ 60 msg/s (one per animation frame). |
| `{"t":"md","b":0,"x":0.5,"y":0.2}` | `ctl` | Mouse button down at position. `b`: `0`=left, `1`=middle, `2`=right (matches DOM `MouseEvent.button`). Includes coordinates so the click lands exactly where aimed even if a preceding `mm` was dropped. |
| `{"t":"mu","b":0,"x":0.5,"y":0.2}` | `ctl` | Mouse button up. |
| `{"t":"wh","dx":0,"dy":-120}` | `ctl` | Wheel. `dy`/`dx` in Windows wheel units (multiples of ±120; sender converts DOM deltas — see client plan). |
| `{"t":"kd","code":"KeyA"}` | `ctl` | Key down. `code` is the DOM `KeyboardEvent.code` string (physical key, layout-independent). |
| `{"t":"ku","code":"KeyA"}` | `ctl` | Key up. |

### 4.2 Host → Viewer: session control (always on the `ctl` channel)

| Message | Meaning |
|---|---|
| `{"t":"perm","value":"view"}` | Host changed the live permission. Sent once immediately after the `ctl` channel opens (initial value) and again on every change. Viewer updates its UI (e.g. shows "view only" badge, stops capturing input). |
| `{"t":"bye"}` | Host is ending the session. Viewer closes the peer connection and returns to the home screen. |

Viewer → Host may also send `{"t":"bye"}` on `ctl` when the viewer disconnects gracefully.

---

## 5. Client configuration schema (host-side JSON, persisted)

Stored by the app (details in client plan). Reproduced here because the handshake
semantics above depend on it:

```json
{
  "uuid": "<generated once, never changes>",
  "serverUrl": "wss://<your-deployed-server>/ws",
  "mode": "approve",
  "passwordHash": null,
  "passwordPermission": "view",
  "iceServers": [{"urls": "stun:stun.l.google.com:19302"}]
}
```

- `mode`: `"approve"` (default) or `"password"`.
- `passwordHash`: SHA-256 hex of the password, or `null` if unset. If `mode` is
  `"password"` but `passwordHash` is `null`, the host treats incoming requests as if
  mode were `approve` (fail-safe).
- `passwordPermission`: permission auto-granted on correct password.
- `iceServers`: passed verbatim to `RTCPeerConnection`. Default is Google's free STUN;
  users can append a TURN entry (see `03-HOSTING.md`) for restrictive NATs.

---

## 6. Security posture (v1 — explicit tradeoffs)

- All signaling over **wss://** (TLS terminated by the hosting platform).
- WebRTC media/data is always DTLS-SRTP encrypted (automatic).
- Password uses **challenge-response** (v1.1, see Step 2): the viewer sends a proof
  derived from a host-issued nonce, never the plaintext, so the password never
  transits the relay and the server never sees it. Compared host-side against the
  stored SHA-256. Documented limitation: no rate limiting on guesses beyond the
  host's 1-connection-at-a-time behavior — the HOST adds a 2-second artificial delay
  before answering `bad-password`. (This replaces the original v1 plaintext-over-wss
  scheme; the server was unaffected because it relays the fields opaquely.)
- The server can be abused as a generic relay only between registered UUIDs; payload
  size limit (16 KB/message, except `signal` at 256 KB for SDP) and per-socket rate
  limit (30 messages/s) blunt this. Details in server plan.
- Injecting input cannot reach UAC/secure-desktop prompts or elevated (admin) windows
  when the host app runs non-elevated. This is a Windows security boundary (UIPI) —
  accept it for v1; document "run as administrator" as the workaround for controlling
  elevated apps.

---

## 7. Split of responsibilities & integration checklist

| # | Deliverable | Owner |
|---|---|---|
| 1 | Signaling server (Cloudflare Worker + Durable Object), per `03-HOSTING.md` | Agent A — plan `01` |
| 2 | Electron app: UI, config, WebRTC, screen capture, input injection, popups | Agent B — plan `02` |
| 3 | Hosting deployment steps (done by whoever runs deployment; server must conform) | `03-HOSTING.md` |

**Integration test script (run after both halves are done):**

1. Start server locally: in `server/`, run `npx wrangler dev --port 8787`
   (serves `ws://localhost:8787/ws`; no Cloudflare account needed for local dev).
2. Launch two instances of the app on one machine (client plan explains the
   `--profile` second-instance mechanism) with `serverUrl` =
   `ws://localhost:8787/ws`.
3. Instance H (host) in `approve` mode; instance V connects with H's UUID → H shows
   popup → choose "View + control" → V sees H's screen; moving the mouse inside V's
   video moves H's real cursor.
4. From H's session bar, switch permission to `view` → V's input stops working and V
   shows a "view only" badge — without reconnecting.
5. H sets mode `password` (password `test123`, permission `view`), V reconnects → V is
   prompted for password → wrong password rejected with an error toast; correct password
   connects straight to view-only (no popup on H).
6. Either side clicks End session → both return to home screens; both can reconnect.
7. Kill V's process mid-session → within ~10 s H detects the dead session and returns
   to idle (able to accept new connections).

---

## 8. File layout of the final repository

```
shareCtrlScreen/
├─ pans/                     (these plan documents)
├─ server/                   ← Agent A works ONLY here
│  ├─ package.json
│  ├─ wrangler.jsonc
│  └─ src/index.js
├─ app/                      ← Agent B works ONLY here
│  ├─ package.json
│  └─ (see client plan for internal layout)
└─ README.md                 (whoever finishes second writes it: run + deploy steps)
```

Agents A and B must not modify files outside their directory (except README as noted).
