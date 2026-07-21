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

The control plane (handshake, config, challenge-response, session state, FEC,
packetization, signaling watchdog) is unit-tested and portable. The Windows media paths
(DXGI duplication, MF encode/decode event pump, D3D11 present, str0m↔UDP driver, service
injection/desktop-follow) compile against the real `windows` 0.62 / `str0m` 0.21 APIs and
encode the exact sequences from §5–§8, but their fine timing and vendor-MFT quirks are
validated on target hardware via the smoke-test above — as the plan intends (§5b "verify
at runtime against target hardware"; §12 "this is the one place I'd de-risk").
```
