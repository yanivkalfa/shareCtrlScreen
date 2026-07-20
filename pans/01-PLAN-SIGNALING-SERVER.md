# Plan 01 — Signaling Server (Agent A)

> **Prerequisite reading:** `00-OVERVIEW-AND-PROTOCOL.md` — the message schemas there are
> the contract. This plan tells you HOW to implement the server side of that contract.
> You work ONLY inside the `server/` directory.
> **Deploy target:** Cloudflare Workers + Durable Objects (free plan) — chosen in
> `03-HOSTING.md`. Local development and all your testing use `wrangler dev` (no
> Cloudflare account needed until deployment).

---

## 1. What this server is

A rendezvous/relay for exactly one kind of traffic: small JSON signaling messages
between apps identified by UUID. It holds a set of connected WebSockets, knows which
UUID each belongs to, and forwards messages addressed with `to` to the right socket
(rewriting `to` → `from`). It stores **nothing** durably: no database, no persisted
state, no logs of message contents. If it restarts, clients simply reconnect and
re-register (they already do this automatically).

Total expected size: **one file, roughly 150–250 lines.**

## 2. Why Cloudflare Durable Objects (context, not a decision for you to revisit)

Container hosts' free tiers idle-kill or cold-start; Cloudflare's **WebSocket
Hibernation API** keeps client sockets open at the edge while the compute sleeps, free
plan is permanent and needs no credit card. Consequence for you: you MUST use the
**hibernation** WebSocket API (`ctx.acceptWebSocket`, `webSocketMessage` handler
methods), NOT the legacy `ws.addEventListener` style — the legacy style pins the object
in memory and burns through the free duration quota.

The one architectural rule hibernation imposes: **the Durable Object's in-memory JS
state can vanish at any moment between messages** (the object is evicted and re-created
on the next message). Therefore:

- The uuid↔socket registry must NOT live in a `Map` you build at accept time.
- Instead, attach the UUID to each socket with `ws.serializeAttachment({uuid})`
  (survives hibernation), and look sockets up by iterating
  `this.ctx.getWebSockets()` and reading `ws.deserializeAttachment()`.
- Anything kept in instance fields (e.g. rate-limit counters) must be treated as a
  cache that can reset — acceptable for rate limiting, unacceptable for identity.

## 3. Files to create

```
server/
├─ package.json          (name "sharectrl-signal-server", private: true,
│                         devDependencies: { "wrangler": "^4" },
│                         scripts: { "dev": "wrangler dev --port 8787",
│                                    "deploy": "wrangler deploy" })
├─ wrangler.jsonc        (see §4)
└─ src/index.js          (Worker entry + Durable Object class, see §5–§8)
```

No runtime npm dependencies at all. Plain JavaScript (not TypeScript).

## 4. `wrangler.jsonc` — exact required fields

| Field | Value | Why |
|---|---|---|
| `name` | `"sharectrl-signal"` | Becomes the `*.workers.dev` subdomain. |
| `main` | `"src/index.js"` | |
| `compatibility_date` | `"2026-07-01"` | |
| `durable_objects.bindings` | one binding: `{ "name": "SIGNAL", "class_name": "SignalingRoom" }` | Exposes the DO to the Worker as `env.SIGNAL`. |
| `migrations` | `[ { "tag": "v1", "new_sqlite_classes": ["SignalingRoom"] } ]` | **Must be `new_sqlite_classes`** (not `new_classes`) — only SQLite-backed DOs are allowed on the free plan; `new_classes` will deploy-fail on free. |

## 5. Worker entry (default export `fetch`)

Behavior, in order:

1. Parse the request URL.
2. If the path is NOT `/ws` → return a plain 200 text response `"sharectrl-signal ok"`
   (this doubles as a health-check / uptime-probe endpoint).
3. If path is `/ws` but the `Upgrade` header (case-insensitive value check) is not
   `websocket` → return 426 with text `"expected websocket"`.
4. Otherwise forward the request to the **single global room**:
   `env.SIGNAL.idFromName("global")` → `getStub` → return `stub.fetch(request)`.
   One DO instance handles everyone; fine for this scale (a DO comfortably handles
   thousands of mostly-idle sockets; hibernation supports up to 32k per object).

## 6. The `SignalingRoom` Durable Object class

Export the class from the same file. It extends `DurableObject` (imported from
`"cloudflare:workers"`).

### 6.1 Constructor

- Call `super(ctx, env)`.
- Set up the ping auto-responder so pings never wake the object:
  `this.ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair('{"type":"ping"}', '{"type":"pong"}'))`
  — the two strings must be **byte-exact** as written in the contract §3.3.
- Initialize the in-memory rate-limit map: `this.rates = new Map()` (socket → counter
  object). Remember: this map resets on hibernation — that's acceptable.

### 6.2 `fetch(request)` — accepting a socket

1. Create a pair: `const [client, server] = Object.values(new WebSocketPair())`.
2. Accept with the hibernation API: `this.ctx.acceptWebSocket(server)`.
   (No tags — identity is added later via attachment at register time, because tags
   can only be set at accept time and we don't know the UUID yet.)
3. Return `new Response(null, { status: 101, webSocket: client })`.

### 6.3 Helper functions (module-level, pure)

- `isValidUuid(s)` — string test against
  `^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$`.
- `getUuid(ws)` — `ws.deserializeAttachment()?.uuid ?? null` (wrap in try/catch;
  a never-registered socket has no attachment and `deserializeAttachment` may throw —
  return `null` in that case).
- `findSocket(ctx, uuid)` — iterate `ctx.getWebSockets()`, return the first whose
  `getUuid` equals `uuid` **and** whose `readyState === WebSocket.READY_STATE_OPEN`
  (constant value 1), else `null`.
- `send(ws, obj)` — `ws.send(JSON.stringify(obj))` wrapped in try/catch (a socket can
  die between lookup and send; swallow the error).

### 6.4 `webSocketMessage(ws, message)` — the whole protocol

Process strictly in this order:

1. **Type gate:** if `message` is not a string (i.e., it's an ArrayBuffer/binary
   frame) → ignore and return.
2. **Rate limit:** look up `this.rates.get(ws)`; keep `{count, windowStart}`. If more
   than **30 messages within the current 1-second window** → ignore and return
   (silently drop; do not close — a reconnect storm would make things worse).
   Use `Date.now()` for the window.
3. **Size gate (cheap, before parsing):** if `message.length > 262144` (256 KB) →
   ignore and return.
4. **Parse:** `JSON.parse` in try/catch → on failure ignore and return. Reject
   non-objects and missing/non-string `type` the same way.
5. **Dispatch on `msg.type`:**

**`"register"`**
- If the socket already has a UUID (`getUuid(ws) !== null`) → ignore (idempotent;
  don't allow re-registration with a different UUID).
- If `msg.v !== 1` → `send(ws, {type:"register-error", reason:"bad-version"})`, then
  `ws.close(4000, "bad-version")`, return.
- If `!isValidUuid(msg.uuid)` → `send(ws, {type:"register-error", reason:"invalid-uuid"})`,
  then `ws.close(4000, "invalid-uuid")`, return.
- If `findSocket(this.ctx, msg.uuid)` finds another live socket →
  `send(ws, {type:"register-error", reason:"duplicate"})`, then
  `ws.close(4001, "duplicate")`, return.
- Otherwise: `ws.serializeAttachment({ uuid: msg.uuid })`, then
  `send(ws, {type:"registered", uuid: msg.uuid})`.

**Relay types — exactly this whitelist:**
`"connect-request"`, `"connect-response"`, `"password-required"`, `"signal"`,
`"end-session"`

- Sender must be registered: `const from = getUuid(ws)`; if `null` →
  `ws.close(4002, "not-registered")`, return.
- Per-type size limit: `"signal"` messages may be up to 262144 chars (already gated);
  every OTHER relay type must be ≤ **16384** chars → ignore if bigger.
- `msg.to` must pass `isValidUuid` → ignore if not.
- Self-relay (`msg.to === from`) → ignore.
- `const target = findSocket(this.ctx, msg.to)`; if `null` →
  `send(ws, {type:"relay-error", reason:"peer-offline", to: msg.to})`, return.
- Forward: build the outgoing object as a copy of `msg` with the `to` property
  **removed** and `from` set to the sender's registered UUID (**never** trust a
  `from` field the sender may have included — overwrite/delete it), then
  `send(target, outgoing)`. Do not inspect or validate the inner payload (`data`,
  `password`, `accepted`, etc.) — the server is a dumb pipe for whitelisted types.

**Anything else** → ignore silently (forward compatibility; also covers `"ping"`,
which normally never reaches here because the auto-responder answers it at the edge).

### 6.5 `webSocketClose(ws, code, reason, wasClean)` and `webSocketError(ws)`

- `this.rates.delete(ws)`. Nothing else: the attachment dies with the socket, and
  `getWebSockets()` stops returning it, so the "registry" cleans itself. Do NOT try to
  notify the peer of a session — peers detect death via WebRTC (contract §3.2 step 5).

## 7. Explicit non-goals (do not build these)

- No persistence, no SQL usage (the SQLite backing is a billing requirement, not a
  feature to use), no alarms, no auth on the socket itself, no HTTP API beyond the
  health path, no metrics, no TypeScript, no test framework — testing is manual (§8).
- No TLS handling — `wrangler dev` gives `ws://` locally, production gives `wss://`
  automatically on `workers.dev`.

## 8. Acceptance tests (run every one before declaring done)

Run `npm run dev` (serves `ws://localhost:8787/ws`). Use two terminal WebSocket clients
(`npx wscat -c ws://localhost:8787/ws` — accept the wscat install prompt) as clients A
and B. Use these UUIDs:
A = `11111111-1111-4111-8111-111111111111`, B = `22222222-2222-4222-8222-222222222222`.

| # | Steps | Expected |
|---|---|---|
| 1 | `curl http://localhost:8787/` | 200 `sharectrl-signal ok` |
| 2 | A sends `{"type":"register","uuid":"<A>","v":1}` | A receives `{"type":"registered","uuid":"<A>"}` |
| 3 | A sends `{"type":"ping"}` (exact) | A receives `{"type":"pong"}` |
| 4 | New socket sends register with uuid `"hello"` | gets `register-error` `invalid-uuid`, then socket closes |
| 5 | New socket registers with A's UUID while A is connected | gets `register-error` `duplicate`, closes; **A stays connected** |
| 6 | B registers; A sends `{"type":"connect-request","to":"<B>","password":null}` | B receives `{"type":"connect-request","from":"<A>","password":null}` — note `to` removed, `from` added |
| 7 | A sends `{"type":"connect-request","to":"<A-with-last-digit-3>","password":null}` (nobody there) | A receives `{"type":"relay-error","reason":"peer-offline","to":"..."}` |
| 8 | A sends `{"type":"signal","to":"<B>","data":{"kind":"ice","candidate":{"candidate":"x"}}}` | B receives it with `from:"<A>"`, `data` untouched |
| 9 | A sends `{"type":"hack","to":"<B>"}` and also `not json at all` | B receives nothing; A's socket stays open |
| 10 | A sends a relay message with a spoofed `"from":"<B>"` field included | B receives `from` = A's UUID (spoof overwritten) |
| 11 | Before registering, a fresh socket sends `{"type":"signal","to":"<B>","data":{}}` | that socket is closed (code 4002) |
| 12 | Disconnect B (Ctrl-C wscat); A sends connect-request to B | A gets `relay-error` `peer-offline` |
| 13 | Reconnect B, register again | works (old registration fully gone) |

## 9. Done criteria & deployment

- All 13 tests pass locally.
- `npx wrangler deploy` is NOT run by you unless the person driving asks — deployment
  needs a Cloudflare login; the exact steps live in `03-HOSTING.md`. Your deliverable
  is the `server/` directory passing the local test suite via `wrangler dev`.
