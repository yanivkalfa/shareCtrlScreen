import { DurableObject } from "cloudflare:workers";

const UUID_RE =
  /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

// Message types the server is willing to forward between two registered peers.
const RELAY_TYPES = new Set([
  "connect-request",
  "connect-response",
  "password-required",
  "signal",
  "end-session",
]);

const MAX_MESSAGE = 262144; // 256 KB — signal (SDP) ceiling
const MAX_RELAY = 16384; // 16 KB — every other relay type
const RATE_LIMIT = 30; // messages per 1s window per socket
const OPEN = 1; // WebSocket.READY_STATE_OPEN

function isValidUuid(s) {
  return typeof s === "string" && UUID_RE.test(s);
}

function getUuid(ws) {
  try {
    return ws.deserializeAttachment()?.uuid ?? null;
  } catch {
    return null;
  }
}

function findSocket(ctx, uuid) {
  for (const ws of ctx.getWebSockets()) {
    if (getUuid(ws) === uuid && ws.readyState === OPEN) return ws;
  }
  return null;
}

function send(ws, obj) {
  try {
    ws.send(JSON.stringify(obj));
  } catch {
    // Socket died between lookup and send — nothing to do.
  }
}

export class SignalingRoom extends DurableObject {
  constructor(ctx, env) {
    super(ctx, env);
    // Answered at the edge, so pings never wake the object from hibernation.
    // Both strings must stay byte-exact — the client matches them as raw text.
    this.ctx.setWebSocketAutoResponse(
      new WebSocketRequestResponsePair('{"type":"ping"}', '{"type":"pong"}')
    );
    // Cache only — resets on hibernation, which is fine for rate limiting.
    this.rates = new Map();
  }

  async fetch(request) {
    const [client, server] = Object.values(new WebSocketPair());
    // Identity is attached later, at register time, via serializeAttachment.
    this.ctx.acceptWebSocket(server);
    return new Response(null, { status: 101, webSocket: client });
  }

  webSocketMessage(ws, message) {
    if (typeof message !== "string") return; // binary frames are not part of the protocol

    const now = Date.now();
    let rate = this.rates.get(ws);
    if (!rate || now - rate.windowStart >= 1000) {
      rate = { count: 0, windowStart: now };
      this.rates.set(ws, rate);
    }
    rate.count++;
    if (rate.count > RATE_LIMIT) return; // drop silently; closing would worsen a storm

    if (message.length > MAX_MESSAGE) return;

    let msg;
    try {
      msg = JSON.parse(message);
    } catch {
      return;
    }
    if (typeof msg !== "object" || msg === null || Array.isArray(msg)) return;
    if (typeof msg.type !== "string") return;

    if (msg.type === "register") return this.handleRegister(ws, msg);
    if (RELAY_TYPES.has(msg.type)) return this.handleRelay(ws, msg, message.length);
    // Anything else is ignored for forward compatibility.
  }

  handleRegister(ws, msg) {
    if (getUuid(ws) !== null) return; // idempotent; no re-registering under a new UUID

    if (msg.v !== 1) {
      send(ws, { type: "register-error", reason: "bad-version" });
      ws.close(4000, "bad-version");
      return;
    }
    if (!isValidUuid(msg.uuid)) {
      send(ws, { type: "register-error", reason: "invalid-uuid" });
      ws.close(4000, "invalid-uuid");
      return;
    }
    // Take-over, not lock-out. The same UUID re-registering is almost always the
    // same user reconnecting — most importantly after a network change or a
    // sleep/wake that left the previous TCP connection half-open. Cloudflare keeps
    // that ghost socket marked OPEN for minutes, so rejecting the newcomer as
    // "duplicate" would strand the real client (retrying every 30 s) and make its
    // ID unreachable. Displace the stale socket and let the newcomer own the UUID.
    // Security rests on the per-session approval/password handshake, never on
    // registration exclusivity, so this cannot be used to hijack a live session.
    const existing = findSocket(this.ctx, msg.uuid);
    if (existing) {
      send(existing, { type: "register-error", reason: "displaced" });
      try {
        existing.close(4001, "displaced");
      } catch {
        // Already gone — nothing to close.
      }
    }

    ws.serializeAttachment({ uuid: msg.uuid });
    send(ws, { type: "registered", uuid: msg.uuid });
  }

  handleRelay(ws, msg, length) {
    const from = getUuid(ws);
    if (from === null) {
      ws.close(4002, "not-registered");
      return;
    }
    if (msg.type !== "signal" && length > MAX_RELAY) return;
    if (!isValidUuid(msg.to)) return;
    if (msg.to === from) return;

    const target = findSocket(this.ctx, msg.to);
    if (!target) {
      send(ws, { type: "relay-error", reason: "peer-offline", to: msg.to });
      return;
    }

    // Dumb pipe: forward the payload untouched, but `from` always comes from the
    // registry — never from anything the sender put in the body.
    const outgoing = { ...msg, from };
    delete outgoing.to;
    send(target, outgoing);
  }

  webSocketClose(ws) {
    this.rates.delete(ws);
  }

  webSocketError(ws) {
    this.rates.delete(ws);
  }
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);

    if (url.pathname !== "/ws") {
      return new Response("sharectrl-signal ok", {
        headers: { "content-type": "text/plain" },
      });
    }
    if ((request.headers.get("Upgrade") || "").toLowerCase() !== "websocket") {
      return new Response("expected websocket", { status: 426 });
    }

    const id = env.SIGNAL.idFromName("global");
    return env.SIGNAL.get(id).fetch(request);
  },
};
