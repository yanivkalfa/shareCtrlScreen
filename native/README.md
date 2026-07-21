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
| `crates/capture` | DXGI Desktop Duplication (§5a): shared `ID3D11Texture2D` + dirty/move rects + out-of-band cursor; `ACCESS_LOST` re-init. |
| `crates/codec` | Media Foundation HW H.264/HEVC encode (§5b, zero-copy, LTR, the low-latency `ICodecAPI` recipe) + D3D11-backed decode (§5c). |
| `crates/transport` | str0m data channels (§6): video (unreliable) / ctl (reliable) / cursor; FEC (`reed-solomon-simd`) + app fragmentation. Unit-tested. |
| `crates/signaling` | Async WebSocket client to the Cloudflare Worker: register, byte-exact ping/pong watchdog, backoff reconnect. |
| `crates/input` | `SendInput` injection + Set-1 scancode table (§8a) + `WH_KEYBOARD_LL` shortcut hook. Ported from `input.js`/`keyhook.js`. Unit-tested. |
| `crates/render` | D3D11 flip-model present (§7): waitable swapchain, `ALLOW_TEARING`, NV12→RGB BT.709 shader. |
| `crates/elevation` | SYSTEM service primitives (§8b): session detection, `WTSQueryUserToken`, `CreateProcessAsUser`, input-desktop follow, service install/uninstall. |
| `crates/engine` | Orchestration: handshake state machine (approve/password, live view↔control), and the Windows media pipeline wiring capture↔codec↔transport↔render↔input. |
| `app/` | Tauri 2 binary: `#[tauri::command]`s, WebView2 UI host, native D3D child-HWND video, ported vanilla UI in `app/ui/`. |
| `service/` | Thin SYSTEM service binary: SCM dispatcher + session-follow launch loop. |

## Build

```sh
cd native
cargo build --workspace --release
cargo test  --workspace          # 20 unit tests (protocol/input/codec/transport/engine)
```

Two shippable binaries: `sharectrl.exe` (the Tauri app/engine) and
`sharectrl-service.exe` (the SYSTEM service).

Requires the Rust `x86_64-pc-windows-msvc` toolchain and the WebView2 runtime
(present on Windows 11; the installer ships the bootstrapper otherwise).

## The go/no-go: latency smoke-test (§12)

The single validation that the premise holds. On target hardware:

```sh
cargo run -p engine --example latency_smoketest
```

It wires `capture → encode → decode` on one machine and prints per-stage timings
against the §2 budget. If capture+encode+decode isn't in the low-single-digit-ms
range, ~40 ms glass-to-glass is unreachable and the design must change **before**
polishing transport/UI/elevation.

## Install (§8b, §10)

```sh
sharectrl-service.exe --install-service     # admin, once — registers LocalSystem service
sharectrl-service.exe --uninstall-service   # admin
```

The service is mandatory to reach the secure desktop (UAC / lock / login). UIAccess is
deliberately **not** used, so the app runs **unsigned** — the only cost is a first-install
SmartScreen "Run anyway" prompt (§8b/§11.4). No code-signing certificate, no codec
license, no cloud bill: `$0` to build and ship (§11.4).

## Licensing (§11 — CI-enforced)

- `deny.toml` — `cargo deny check` allowlist + banned-crate list (AGPL capture crates,
  no-license `hwcodec`, GPL x264/x265). Runs in CI.
- `about.toml` / `about.hbs` — `cargo about generate` → `third-party-licenses.html`.
- Clean-room: **zero lines** of RustDesk (AGPL). Every module implemented from
  Microsoft/vendor primary docs.

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
