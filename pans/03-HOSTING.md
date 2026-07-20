# Plan 03 — Free Hosting for the Signaling Server (+ STUN/TURN)

> Research verified July 20, 2026. This file explains WHERE the server from plan 01
> runs for free, exactly how to deploy it, and the free NAT-traversal (STUN/TURN)
> options the client from plan 02 can be configured with.

---

## 1. The decision: Cloudflare Workers + Durable Objects

The signaling server's clients hold a WebSocket open **24/7** (that's how a host is
reachable by UUID at any time). That single requirement kills most "free tier" hosts,
which either idle-kill the process or cold-start it. Cloudflare is the only 2026
option that is simultaneously:

- **Free forever** (not a trial), **no credit card required**;
- Immune to the idle problem: the **WebSocket Hibernation API** keeps client
  connections open at Cloudflare's edge while the compute sleeps, waking it in
  milliseconds when a message arrives — clients never notice;
- Generous far beyond our needs: free plan gives Durable Objects **100,000 requests/day**
  and 13,000 GB-s/day compute (incoming WS messages count 20-to-1 toward requests; a
  full connect handshake is a few dozen messages — effectively unlimited for this app);
- Free TLS: the deployed URL is `wss://…workers.dev/ws` out of the box.

This is why plan 01 targets the Workers runtime rather than a plain Node process.

### Deployment steps (one-time, ~10 minutes, done by the person driving)

1. Create a free Cloudflare account at https://dash.cloudflare.com/sign-up
   (email + password only; no card). During onboarding pick the **Workers Free** plan
   and choose your `*.workers.dev` subdomain (e.g. `yaniv`).
2. In the `server/` directory: `npm install` (installs wrangler), then
   `npx wrangler login` (opens a browser; approve).
3. `npx wrangler deploy` — first run asks to confirm the Durable Object migration;
   accept. Output prints the public URL, e.g.
   `https://sharectrl-signal.yaniv.workers.dev`.
4. Verify: open that URL in a browser → should show `sharectrl-signal ok`.
5. The value to put in **both apps' Settings → Server URL** is that URL with `wss://`
   and the `/ws` path: `wss://sharectrl-signal.yaniv.workers.dev/ws`.
6. Redeploys: just `npx wrangler deploy` again. Note a deploy resets live
   connections; clients auto-reconnect within seconds (plan 02 §8).

## 2. Runner-ups (if Cloudflare is ever unacceptable)

| Rank | Option | Free terms (July 2026) | Why not #1 |
|---|---|---|---|
| 2 | **Render.com** free web service | 750 h/mo (covers one 24/7 service), no card, wss included, GitHub auto-deploy. | Spins down after 15 min without inbound traffic; next connect eats a ~60 s cold start. (Client pings do keep it awake while ANY app is online.) Would also require rewriting the server as a plain Node `ws` process. |
| 3 | **Oracle Cloud "Always Free" VPS** | Real VM (2 OCPU/12 GB ARM — allowance halved June 2026), no cold starts. | Credit card required, chronic ARM capacity shortages, Oracle reclaims "idle" instances (a signaling server IS idle), you manage TLS/updates yourself. |

**Dead ends verified — do not waste time on:** Railway (trial only now), Fly.io (no
free tier for new users), Koyeb (free tier gone after Mistral acquisition), Glitch
(hosting shut down July 2025), Vercel/Netlify (no long-lived WebSockets), Deno Deploy
(isolates recycle → forced disconnects), Hugging Face Spaces (48 h force-sleep).

## 3. STUN / TURN (WebRTC NAT traversal — used by the CLIENT, not the server)

- **STUN (default, ship it):** Google's free servers, no signup:
  `stun:stun.l.google.com:19302` (already the default `iceServers` in the client
  config). STUN alone connects roughly 80–90% of real-world peer pairs.
- **TURN (optional fallback for the stubborn 10–20%** — symmetric NATs, corporate
  firewalls). Two good free options; both just produce extra entries for the
  `iceServers` array in the app's config file:
  1. **Cloudflare Realtime TURN** — best free deal: **1,000 GB/month** free. Same
     Cloudflare account as above → dashboard → Realtime → create a TURN key →
     generate credentials (they're short-lived; for personal v1 use, regenerate as
     needed or script it later).
  2. **Metered Open Relay** (https://www.metered.ca/tools/openrelay/) — free account →
     20 GB/month, TURN on ports 80/443 (pierces strict firewalls); credentials come
     from a simple GET to their credentials API and are pasted into `iceServers`.
- Caveat: if the **video** falls back to TURN at ~4 Mbps, 20 GB ≈ 11 hours of
  screen-sharing per month; Cloudflare's 1 TB ≈ 580 hours. Prefer Cloudflare if TURN
  becomes a regular need.
- v1 stance: ship STUN-only defaults; document TURN as the "my connection fails"
  remedy in the README.

## 4. Total running cost

| Item | Cost |
|---|---|
| Signaling (Cloudflare Workers + DO) | $0, no card |
| STUN (Google) | $0, no signup |
| TURN (only if needed — Cloudflare Realtime or Metered) | $0 within free quotas |
| **Total** | **$0/month** |
