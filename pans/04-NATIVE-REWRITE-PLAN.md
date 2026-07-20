# Plan 04 — Native Rust Rewrite for AnyDesk-Class Performance

> **Goal:** rebuild the remote-desktop app as a native Rust application reaching
> AnyDesk/Parsec-class latency (~20–40 ms glass-to-glass), **proprietary and
> closed-source**, while keeping our existing UI look, identity/approval/password
> model, and Cloudflare signaling server. Electron is dropped.
>
> **This plan is executed in one pass** (not staged milestones). It is grounded in
> July-2026 research; every crate/API/license below was verified. Where a choice
> rests on a tradeoff, the decision and its rationale are stated so the implementing
> agent does not re-litigate it.
>
> **THE HARD LEGAL RULE (read first, violate nothing):** RustDesk is **AGPL-3.0**.
> We take **zero lines** of its code. We use it only as an *architecture reference*
> (what APIs, what sequence) and implement everything from Microsoft/vendor primary
> docs and permissively-licensed crates. See §11 for the clean-room + license-audit
> setup that keeps this defensible. Any crate that is AGPL/GPL, or has **no license**,
> is banned (§11 lists the specific banned crates).

---

## 1. What we keep, what we replace

| Kept (already built, ported not rewritten) | Replaced (the entire latency story) |
|---|---|
| UI: HTML/CSS/JS screens (home, settings, approval popup, session bar) | Shell: Electron → **Tauri 2.x** (native, WebView2) |
| Identity: persistent UUID, config schema | Media capture: Chromium `getDisplayMedia` → **DXGI Desktop Duplication** |
| Access model: approve / password (challenge-response), live permission switch | Encode: WebRTC encoder → **hardware H.264/HEVC (Media Foundation, zero-copy)** |
| Signaling: Cloudflare Worker + our JSON WebSocket protocol | Transport: WebRTC-in-Chromium → **str0m data channels (no jitter buffer) + FEC** |
| Input semantics: DOM-code → Win32 scan-code table (from `input.js`) | Decode: `<video>` element → **D3D11VA hardware decode** |
| CI/release idea (adapted to Rust/Tauri build) | Render: DOM repaint → **direct D3D11 flip-model present** |
| | Elevation: none → **SYSTEM service** to control UAC/lock/secure desktop |

The signaling protocol in `00-OVERVIEW-AND-PROTOCOL.md` is reused **verbatim at the
WebSocket layer** — the server needs no changes. Only the `data` payloads it relays
change (they now carry str0m's ICE/DTLS handshake instead of browser SDP; the server
treats `data` as opaque, so this is transparent to it).

---

## 2. Target architecture & latency budget

```
HOST (being controlled)                                 VIEWER (controlling)
┌──────────────────────────────────┐                   ┌──────────────────────────────────┐
│ SYSTEM service (session 0)        │                   │ Tauri app (WebView2 UI + engine)  │
│  └ launches ↓ into user session   │                   │  ├ WebView2: chrome (home/settings│
│ Engine process (user session,     │                   │  │   /approval/session bar)       │
│  UIAccess-signed, desktop-follow) │                   │  ├ D3D11 video surface (native)    │
│  ├ DXGI Desktop Duplication ──┐   │   WebRTC (str0m)  │  ├ D3D11VA HW decode ──┐          │
│  │   (dirty rects, cursor OOB)│   │   data channels   │  │                       │          │
│  ├ RGB→NV12 (GPU) ────────────┤   │  ◄══════════════► │  ├ NV12→RGB shader ────┤          │
│  ├ HW encode (MF/NVENC) ──────┤   │  video: unreliable│  ├ flip-model present ──┘          │
│  │   H.264, no B-frames, LTR  │   │  input: reliable  │  └ Raw Input capture over video    │
│  └ SendInput injection ◄──────┘   │  cursor: reliable │      → input msgs ─────────────►   │
└──────────────────────────────────┘                   └──────────────────────────────────┘
              ▲                                                          ▲
              └──────── Cloudflare Worker (JSON WS signaling, UNCHANGED) ┘
```

**Latency budget (LAN, 1080p60), from research:**

| Stage | Target | Source of the number |
|---|---|---|
| Capture (DDA acquire + GPU copy of dirty rects) | ~1–3 ms | event-driven, GPU-only |
| RGB→NV12 (GPU shader / Video Processor MFT) | <1 ms | GPU |
| HW encode (H.264, no B-frames, ULL, surfaces=1) | ~1–4 ms (MF adds ~2–3 ms over NVENC-direct) | NVENC ~1 ms; MFT ~2–12 ms depending on pacing |
| Transport (str0m datagram, **no jitter buffer**) | ~0–2 ms + RTT | jitter buffer is the trap: 46–189 ms if left on |
| HW decode (D3D11VA, `MF_LOW_LATENCY`, no reorder) | ~1–3 ms | NVDEC ~1–1.5 ms |
| Render (flip-model, waitable, 1-frame latency) | ~1 frame | — |
| **Glass-to-glass, wired LAN** | **~11–30 ms** | Parsec ~11–13 ms; Moonlight 20–30 ms internet |

**The five things that would silently destroy this budget** (each is prevented by a
specific decision below): B-frames (reorder delay) → §5b zero B-frames; a jitter
buffer → §6 str0m with none; periodic IDR keyframes → §5b LTR recovery; encoder
look-ahead/async surface depth → §5b `surfaces=1`; an HTML `<video>` tag → §7 native
D3D surface.

---

## 3. Tech stack (decided — do not substitute)

| Concern | Choice | Version / License (verified) |
|---|---|---|
| Language | **Rust** | matches reference material; permissive ecosystem |
| Windows APIs | **`windows` crate** (windows-rs, Microsoft) | 0.62.2 · **MIT OR Apache-2.0** |
| Shell | **Tauri 2.x** (WebView2 via `wry`/`tao`) | 2.11.x · **MIT OR Apache-2.0** |
| Capture | **DXGI Desktop Duplication** via `windows` (primary); optionally `windows-capture` 2.0 (MIT) as a ready wrapper; **WGC** as fallback backend | — |
| Encode | **Media Foundation HW MFT** via `windows` (primary, all GPUs); **NVENC direct** as opt-in optimization | NVENC crate `nvidia-video-codec-sdk` MIT + NVIDIA SLA (commercial-OK) |
| Decode | **D3D11VA** (`ID3D11VideoDevice`) via `windows`, or MF decoder MFT | — |
| Transport | **`str0m`** (sans-IO WebRTC, data-channels-only) | 0.21.0 · **MIT OR Apache-2.0** |
| UDP I/O | **`quinn-udp`** (GSO/GRO/ECN, Windows-safe) | 0.6.1 · **MIT OR Apache-2.0** |
| FEC | **`reed-solomon-simd`** | 3.1.0 · **MIT AND BSD-3-Clause** |
| Encryption | **DTLS via str0m** (automatic); `snow` (Noise) only if we drop to raw UDP | snow 0.10 · Apache-2.0 OR MIT |
| TURN fallback | self-host **coturn** (BSD-3) or Cloudflare Realtime TURN | — |
| License audit | **`cargo-deny`** + **`cargo-about`** in CI | — |

**Codec default: H.264 4:2:0** (universal HW encode+decode, lowest latency). **HEVC**
opt-in (better text per bit, negotiated when both ends support it). **AV1** opt-in,
**negotiated when both ends support it** (RTX40+/Arc/RDNA3+ only, 4:2:0). **4:4:4 crisp-text
mode** is NVENC-only (MF input types are 4:2:0/4:2:2 only) — treat as an NVIDIA-only premium
feature, not baseline.

**Zero-cost shipping posture (decided — see §11.4):** ship with **H.264 hardware as the
runtime default**, negotiate AV1 up when both peers support it. Rationale: H.264-via-
hardware-encoder is what actually hits the latency target on the widest range of machines,
and the H.264/HEVC patent-royalty question is very likely already covered by the OS/GPU
vendor for this common case (using the encoder they licensed) — confirm with counsel later,
once there's a product worth protecting, not before. AV1 (royalty-free) is the guaranteed-$0
escape hatch if that ever becomes a concern. Combined with skipping code signing (§8b/§11.4),
**this owes nothing up front: no certificate, no codec license, no cloud bill.**

**Encode path decision:** ship v1 on the **Media Foundation** path (one code path drives
all vendors' HW MFTs, fully permissive). It costs ~2–3 ms more than NVENC-direct and
**cannot do periodic intra-refresh or 4:4:4** (MF exposes no intra-refresh CODECAPI). Add
an **NVENC-direct path** for the large NVIDIA base to unlock intra-refresh (flat
bandwidth, no keyframe hitches) and 4:4:4. Both are built now since "one go," but MF is
the correctness baseline that must work on 100% of hardware; NVENC is the optimization
layer selected at runtime when an NVIDIA GPU is present.

---

## 4. Repository / workspace layout

A single Cargo workspace. Each crate is one clear responsibility so agents can work in
parallel without collision.

```
shareCtrlScreen/                 (existing repo; native app lives alongside old app/ during transition)
├─ native/                       ← the new product; Cargo workspace
│  ├─ Cargo.toml                 (workspace; [workspace.lints], shared deps, cargo-deny config ref)
│  ├─ deny.toml                  (§11 license allowlist — CI-enforced)
│  ├─ about.toml                 (cargo-about → third-party-licenses.html)
│  ├─ crates/
│  │  ├─ protocol/               data types shared by all: input msgs, control msgs, config schema,
│  │  │                          signaling message enums (mirrors 00-OVERVIEW contract)
│  │  ├─ capture/                DXGI Desktop Duplication + WGC fallback → ID3D11Texture2D + dirty rects + cursor
│  │  ├─ codec/                  encode (MF MFT + NVENC) and decode (D3D11VA) — the ONLY crate touching MF/NVENC
│  │  ├─ transport/              str0m wiring, data channels, FEC, BWE→bitrate loop, packetization
│  │  ├─ signaling/              WebSocket client to the Cloudflare Worker; reuses 00-OVERVIEW protocol
│  │  ├─ input/                  SendInput injection (port input.js scancode table) + WH_KEYBOARD_LL hook
│  │  ├─ render/                 D3D11 flip-model swapchain, NV12→RGB shader, cursor sprite, present loop
│  │  ├─ elevation/              SYSTEM service, session/desktop-follow, CreateProcessAsUser hand-off
│  │  └─ engine/                 orchestrates host & viewer sessions; ties all crates together
│  ├─ app/                       Tauri binary: WebView2 UI host + native D3D overlay + #[tauri::command]s
│  │  ├─ tauri.conf.json
│  │  ├─ src/                    Rust: commands, window/HWND, overlay compositing, event bridge
│  │  └─ ui/                     the existing renderer/ HTML/CSS/JS, ported (withGlobalTauri)
│  └─ service/                   the SYSTEM Windows service binary (thin; uses crates/elevation)
├─ server/                       UNCHANGED (Cloudflare signaling)
└─ app/                          the old Electron app (kept until native reaches parity, then removed)
```

`codec/` is the only crate allowed to touch Media Foundation/NVENC; `capture/` the only
one touching DDA/WGC; `input/`+`elevation/` the only ones doing SendInput/desktop
switching. This mirrors how the old app isolated koffi in `input.js`.

---

## 5. Media engine — stage by stage (crate `capture` + `codec`)

### 5a. Capture — DXGI Desktop Duplication (`crates/capture`)

**Setup chain:** `CreateDXGIFactory1` → `EnumAdapters1` → `EnumOutputs` → cast
`IDXGIOutput` → `IDXGIOutput5` → **`DuplicateOutput1`** (pass supported-formats list,
always include `DXGI_FORMAT_B8G8R8A8_UNORM`; add `R16G16B16A16_FLOAT` + `R10G10B10A2_UNORM`
for HDR). Fall back to `IDXGIOutput1::DuplicateOutput` (BGRA only) on older Windows. The
D3D11 device is created **once, shared with encode** (§5c), with
`D3D11_CREATE_DEVICE_VIDEO_SUPPORT` and `ID3D11Multithread::SetMultithreadProtected(TRUE)`.

**Per-frame loop:** `AcquireNextFrame(short_timeout, &info, &resource)` →
`resource.cast::<ID3D11Texture2D>()` → read `DXGI_OUTDUPL_FRAME_INFO` → process
metadata → consume → **`ReleaseFrame()` promptly** (hold at most one frame). Max 4
duplications per session.

**Dirty/move rects (bandwidth + encode win):** size one buffer by
`TotalMetadataBufferSize`, read `GetFrameMoveRects` then `GetFrameDirtyRects`. **Process
moves before dirties** (documented ordering). Encode only the union of changed regions
via `CopySubresourceRegion` per rect into the encoder-input texture. `AccumulatedFrames>1`
means the OS coalesced — encode once, don't catch up. `LastPresentTime==0` = pointer-only
update.

**Cursor out-of-band (client-side render — feels instant):** `PointerPosition`
(cheap, every move) and, only when `PointerShapeBufferSize!=0`, `GetFramePointerShape`
(cache it). Send position + shape over the **reliable** data channel separate from video;
the viewer draws it as a D3D sprite at `PointerPosition` (§7). Handle the three shape types
(MONOCHROME height=Height/2 AND/XOR; COLOR straight BGRA; MASKED_COLOR alpha-as-mask).

**Adaptive frame rate (send nothing when static, burst to 60fps on motion):** DDA is
event-driven — `AcquireNextFrame` only returns on change, else `DXGI_ERROR_WAIT_TIMEOUT`.
Exploit that instead of forcing a constant stream:
- Keep a "last good" full texture; drive a fixed 16.67 ms tick (waitable timer/QPC) as the
  *ceiling*, not a mandate to emit.
- On `S_OK` with a real dirty region → encode + send (burst up to 60fps during motion).
- On `WAIT_TIMEOUT` / no dirty region → **send nothing.** Do NOT emit repeat frames into an
  idle stream; a static screen should cost ~0 bytes and ~0 encode. This is the single biggest
  bandwidth saving for typical desktop use (typing, reading, menus).
- To bound the "no keyframe for a long idle period" risk, emit a **heartbeat** at a low floor
  (e.g. 1–2 fps) so a viewer that joined or lost a packet during a static period still
  refreshes, and so LTR/decoder state stays warm. The floor is a safety net, not the cadence.
- Cursor moves during an otherwise-static screen travel on the **cursor channel only**
  (§6/§7) — they never wake the video encoder.
- `AccumulatedFrames>1` means the OS coalesced multiple updates — encode once for the newest,
  don't try to replay each.

This is the "adaptive frame rate" pipeline trick: idle → silent; motion → full rate; and it
composes with dirty regions (small change → tiny encode) and the cursor channel (pointer
motion → no video at all).

**Re-init on `DXGI_ERROR_ACCESS_LOST`** (UAC/lock/mode-change/DWM toggle/fullscreen/TDR):
release, drop the duplication, re-`DuplicateOutput` with backoff. On `WM_DISPLAYCHANGE`
re-enumerate from a **fresh factory**. Handle `E_ACCESSDENIED` (secure desktop — only the
SYSTEM helper can duplicate it, §8), `DXGI_ERROR_UNSUPPORTED`,
`DXGI_ERROR_SESSION_DISCONNECTED`, `DEVICE_REMOVED` (recreate device).

**DPI:** the duplication surface is always native physical resolution (no DPI
virtualization). Still set **Per-Monitor-V2** awareness (manifest) so overlays and
remote-cursor coordinate math line up.

**Gotchas to encode into the design:** DRM content → black frames (`ProtectedContentMaskedOut`,
hard platform limit, WGC same); prefer flip-model borderless which duplicates cleanly;
headless machines need an Indirect Display Driver (virtual display). **WGC fallback**
(`windows-capture` 2.0, MIT, exposes `as_raw_texture()` + `dirty_regions()`) for RDP/
RemoteApp and per-window capture where DDA returns black; note WGC's yellow border needs a
package-identity + consent to remove — acceptable since DDA is primary.

**Crate choice:** build directly on `windows` for max control, OR adopt `windows-capture`
2.0 (MIT) which wraps both DDA+WGC and hands back the raw texture. **Banned:** `dxgcap`,
`dxgcap2`, `captrs` (all AGPL-3.0).

### 5b. Encode — Media Foundation HW MFT, zero-copy (`crates/codec`)

**Enumerate/instantiate:** `MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER,
MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER)` for
`MFVideoFormat_H264` (or `_HEVC`). `ActivateObject` → `IMFTransform`. Confirm
`MF_SA_D3D11_AWARE`. Unlock async: `MF_TRANSFORM_ASYNC_UNLOCK=TRUE`.

**Zero-copy GPU input:** `MFCreateDXGIDeviceManager` → `ResetDevice(shared_d3d11_device)`
→ `ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr)`. DDA gives BGRA; every HW encoder
wants **NV12** — convert on-GPU via the **Video Processor MFT** (`CLSID_VideoProcessorMFT`)
or a compute shader (never CPU). Wrap the NV12 texture with **`MFCreateDXGISurfaceBuffer`**
→ `MFCreateSample` + `AddBuffer` + times → `ProcessInput`. Async event loop:
`METransformNeedInput` → `ProcessInput`; `METransformHaveOutput` → `ProcessOutput` (yields
the compressed bitstream to packetize).

**Low-latency config via `ICodecAPI::SetValue` (the exact recipe — mirror Sunshine):**
- `CODECAPI_AVLowLatencyMode = TRUE` (single-picture slice, no multi-frame lookahead)
- `CODECAPI_AVEncCommonRateControlMode = eAVEncCommonRateControlMode_CBR`
- `CODECAPI_AVEncCommonMeanBitRate = <BWE target>` ; small `CODECAPI_AVEncCommonBufferSize`
- **`CODECAPI_AVEncMPVDefaultBPictureCount = 0`** (zero B-frames — mandatory)
- `CODECAPI_AVEncMPVGOPSize` = effectively infinite (no periodic IDR)
- `CODECAPI_AVEncVideoForceKeyFrame` = only on connect / explicit decoder-loss request
- one slice per frame (or N slices sized to the viewer's decode-thread count for pipelined
  decode, Sunshine-style)

**Keyframe strategy (avoid IDR hitches):** MF exposes **no intra-refresh**. Use
**Long-Term Reference frames**: `CODECAPI_AVEncVideoLTRBufferControl` +
`AVEncVideoMarkLTRFrame` + `AVEncVideoUseLTRFrame`. The viewer ACKs the last clean frame;
on loss the encoder references a known-good LTR instead of emitting a full IDR. Forced IDR
reserved for initial connect / total desync. **The NVENC-direct path** additionally enables
true periodic **intra-refresh** (`enableIntraRefresh`, `intraRefreshPeriod/Cnt`) for flat
bandwidth — the single strongest reason to add NVENC.

**Content-aware / region-of-interest QP (the "crisp text, lossy video" trick).** A general
codec spends equal bits everywhere; a desktop stream should spend them where the eye needs
sharpness. Two levers, applied per-frame using the dirty-region + classification info:
- **Region classification:** cheaply tag changed regions as *text/UI* (high spatial
  frequency, few colors, sharp edges) vs *photo/video* (smooth gradients, high motion). A
  fast heuristic on the dirty rects (edge/color-count on a downscaled copy) is enough; no ML
  needed.
- **Per-region QP via an ROI/emphasis map:** feed the encoder a QP delta map — **low QP
  (near-lossless) on text/UI regions, higher QP on video/photo regions.** This keeps small
  text and window chrome crisp while letting fast-moving video degrade gracefully, at a lower
  total bitrate than uniform quality.
- **Path support (why this is an NVENC-path feature):** **NVENC exposes ROI/emphasis QP maps
  and per-frame QP** directly (`NV_ENC_PIC_PARAMS` QP delta map / emphasis-level map) — this
  is where the trick lives. **Media Foundation barely exposes per-region QP** (`ICodecAPI`
  gives only frame-level QP band), so on the MF baseline path content-awareness reduces to:
  (a) 4:4:4 for text sharpness where available, and (b) frame-level QP tied to motion. Full
  region-based QP is an **NVENC-path enhancement**, consistent with §3's "MF baseline +
  NVENC optimization" split.
- **Combines with everything above:** dirty regions tell you *what* changed, classification
  tells you *what kind*, ROI-QP tells the encoder *how hard to protect it* — the three
  together are the bulk of what makes AnyDesk/Parsec "feel" sharp and responsive on real
  desktop content.

**Encode latency reality:** MF adds ~2–3 ms over NVENC-direct and worse under capped frame
delivery (~12 ms at a 16 ms cadence) because of the MFT abstraction; MF's `MF_LOW_LATENCY`/
`ICodecAPI` low-latency props may silently no-op on some vendor MFTs — **verify at runtime
against target hardware**, and this is exactly why the NVENC path exists.

### 5c. Decode — D3D11VA hardware (`crates/codec`, viewer side)

**Preferred: raw D3D11VA** (`ID3D11VideoDevice` → `CreateVideoDecoder` → per-frame
`DecoderBeginFrame`/`SubmitDecoderBuffers`/`DecoderEndFrame` into a
`D3D11_BIND_DECODER | D3D11_BIND_SHADER_RESOURCE` texture). This is the Moonlight approach,
~1–3 ms, and gives the tightest frame-pacing control. Alternative: MF decoder MFT with a
shared `IMFDXGIDeviceManager` + `MF_LOW_LATENCY=TRUE` (note the H.264 **decoder** quirk:
set `CODECAPI_AVLowLatencyMode` as **`VT_UI4`**, not `VT_BOOL`).

**Low-latency correctness:** the *stream* must carry no reordering (`num_reorder_frames=0`,
zero B-frames — guaranteed by §5b) AND the decoder must be in low-latency mode; both are
required or a frame of delay remains. `MFT_MESSAGE_COMMAND_DRAIN` only to evict a stuck
frame, never per-frame.

**Texture-array pitfall:** decoded frames arrive as a **slice of a texture array** +
array-index. Allocate the pool with `BIND_DECODER | BIND_SHADER_RESOURCE`; if the driver
honors shader-readable decode surfaces, bind the slice directly
(`Texture2DArray.FirstArraySlice=idx, ArraySize=1`) — true zero-copy; else one
`CopySubresourceRegion` into a single-slice shader texture. **Cache one SRV per pool slice
at init** — creating an SRV per frame ~3× the CPU (measured). Gate the zero-copy path per
GPU vendor with a copy fallback (Moonlight does exactly this).

---

## 6. Transport (`crates/transport`) — str0m data channels, no jitter buffer

**Decision: `str0m` (sans-IO WebRTC), data-channels-only, unreliable/unordered.** It gives
standards-compliant **ICE (NAT traversal) + DTLS (encryption) + SCTP framing for free**, but
imposes **no jitter buffer and no threads/timers** — we own all I/O and buffering, so we can
drive latency to the floor (PulseBeam: P99.99 70 ms → 10 ms on str0m). It reuses our
Cloudflare WebSocket signaling unchanged.

**Channels (mirror the old two-channel split, now three):**
| Channel | Config | Carries |
|---|---|---|
| `video` | `ordered=false, max_retransmits=0` (unreliable) | encoded frames (app-fragmented ≤ ~1200 B datagrams), protected by FEC |
| `ctl` | ordered, reliable | input events (md/mu/wh/kd/ku), permission changes, cursor shape, bye |
| `cursor` | reliable or unreliable-latest | cursor position updates (high-rate) |

Mouse-move-style high-rate input can ride an unreliable sub-stream as before; keys/clicks
stay reliable (a lost key-up = stuck key).

**Loss handling (no retransmit latency):** proactive **FEC** with `reed-solomon-simd`
(1.6–10 GiB/s, runtime SIMD) — recover within the frame interval from redundancy already in
flight. **Keyframes/LTR-ack packets get heavy FEC + selective NACK**; delta frames are just
dropped (Parsec: "no buffers on video; drop bitrate rather than retransmit"). No jitter
buffer anywhere.

**Adaptive bitrate:** str0m exposes **TWCC-based bandwidth estimation (BWE)** — read it and
feed bytes/sec as the CBR target into the encoder's `AVEncCommonMeanBitRate` (§5b), tight
loop (~tens of ms), drop bitrate under sustained loss.

**NAT traversal + signaling:** str0m's built-in ICE agent handles direct/STUN; our
Cloudflare Worker relays str0m's offer/answer + **trickle ICE** candidates as opaque JSON —
**zero server changes** (str0m even offers a direct API to skip SDP for a custom handshake).
TURN fallback for the ~10–15% of restrictive NATs: self-host **coturn** (BSD-3) or Cloudflare
Realtime TURN ($0.05/GB).

**Encryption:** DTLS via str0m, automatic — do not add Noise. (Only if we ever drop to raw
UDP via `quinn-udp` for the last few ms would we add `snow`/Noise_IK; that's the documented
fallback path #2, not v1.)

---

## 7. Shell & rendering (`app/` Tauri + `crates/render`)

**Shell: Tauri 2.x** (MIT/Apache, WebView2, no Chromium — ~2.5 MB vs Electron ~85 MB).
Port the existing `renderer/` HTML/CSS/JS with `app.withGlobalTauri: true` so vanilla
`<script>` works with no bundler. Mapping:
- `ipcRenderer.invoke('x')` → `window.__TAURI__.core.invoke('x')`
- `ipcRenderer.on(...)` → `window.__TAURI__.event.listen(...)` (use the **Channel** API for
  high-volume streams)
- each `ipcMain.handle` → a Rust `#[tauri::command]`, registered and granted in Tauri 2's
  capability JSON (undeclared IPC is blocked by default)
- main-process logic (config, password hash, session control) → Rust commands in `crates/*`

**Video compositing — the load-bearing decision.** Tauri's `wry` uses the **windowed**
WebView2 controller, so a native child HWND can only sit as an **opaque rectangle** over the
web content (the "airspace" limit — HTML cannot float translucently over native pixels).
Two options:

- **Option A (default, ship this): native D3D11 child HWND** parented under the Tauri
  window (`WebviewWindow::hwnd()` → `CreateWindowEx` + `SetParent`). The web UI frames the
  video (toolbar/badges **around** the video area, not overlapping). Lowest effort, keeps the
  whole web UI, accepts a hard seam between chrome and video. **This is the v1 choice** — our
  UI already puts chrome in a top bar, not over the pixels.
- **Option B (only if translucent HTML over live video is later required):** drop Tauri's
  windowed webview and host **WebView2 in Visual-hosting mode** yourself
  (`webview2-com` + `windows` crate, `CreateCoreWebView2CompositionController` →
  `put_RootVisualTarget`), placing the video swapchain (`CreateSwapChainForComposition`) as a
  DirectComposition sibling **under** a transparent webview
  (`put_DefaultBackgroundColor` alpha=0). True z-order, no seam, but you hand-write the win32
  host + input routing. Documented as a fallback; **not v1**.

**D3D11 present (lowest latency, `crates/render`, via `windows`):**
`DXGI_SWAP_EFFECT_FLIP_DISCARD`, BufferCount 2, creation flags
`FRAME_LATENCY_WAITABLE_OBJECT | ALLOW_TEARING`, `IDXGISwapChain2::SetMaximumFrameLatency(1)`,
per-frame **wait-first-then-render**: `WaitForSingleObjectEx(waitable)` → render →
`Present(0, DXGI_PRESENT_ALLOW_TEARING)`. Check tearing support via
`IDXGIFactory5::CheckFeatureSupport`. `CreateSwapChainForHwnd` (Option A) /
`CreateSwapChainForComposition` (Option B). NV12→RGB in a pixel shader (two SRVs: `R8_UNORM`
luma + `R8G8_UNORM` chroma; `R16`/`R16G16` for 10-bit), BT.709 matrix + range scale. Draw the
**cursor as a separate sprite** at the out-of-band `PointerPosition` so it never lags the
stream.

**Input capture over the video (decisive for correctness):** capture at the **native video
window's wndproc using Raw Input** (`RegisterRawInputDevices` → `WM_INPUT`), NOT webview
JS→IPC. Raw Input gives acceleration-free relative deltas, hardware scancodes
(`RAWKEYBOARD.MakeCode` — feeds our scan-code injector directly), high-Hz batched reads, and
background capture. The topmost opaque video HWND already receives the mouse messages. Use
webview JS `invoke` only for non-latency-critical chrome (buttons/menus). Manage focus
between the video HWND and WebView2 (`AcceleratorKeyPressed`, `WM_MOUSEACTIVATE`) so keyboard
focus doesn't get trapped in the webview.

---

## 8. Control plane — input injection & elevation (`crates/input`, `crates/elevation`, `service/`)

### 8a. Input injection (`crates/input`)
Port `input.js` directly: `SendInput` via the `windows` crate
(`windows::Win32::UI::Input::KeyboardAndMouse`), absolute move
(`MOUSEEVENTF_ABSOLUTE|MOUSEEVENTF_VIRTUALDESK|MOUSEEVENTF_MOVE`, normalized 0..65535 across
the virtual desktop — do the multiply in `i64`), wheel, and **scan-code** keyboard
(`KEYEVENTF_SCANCODE` + `KEYEVENTF_EXTENDEDKEY` for arrows/right-mods/numpad/nav) for layout
independence. The DOM-code→Set-1 scan-code table from `02-PLAN §7.4` transfers **verbatim**
(the bytes are identical regardless of language). Track injected-down keys/buttons for
stuck-input release (as the old code does).

**windows-rs porting gotchas (differ from the koffi/C layout — get these exact):**
- `INPUT { r#type: INPUT_TYPE, Anonymous: INPUT_0 }` — the discriminant field is `r#type`
  (`type` is a reserved word); union members are `Anonymous.mi` / `Anonymous.ki`.
- `SendInput(pinputs: &[INPUT], cbsize: i32)` — windows-rs folds the count into the slice
  length; pass `cbsize = core::mem::size_of::<INPUT>() as i32`. Wrong `cbsize` → silent
  return 0. Return value 0 also means **blocked by UIPI** (→ escalate to the elevated/SYSTEM
  path, §8b).
- Structs are already `#[repr(C)]` — **never** add `#[repr(packed)]`.
- Wheel delta is signed but `mouseData` is `u32` — cast `(delta as i32) as u32` (bit pattern
  preserved).
- Flag constants are newtype wrappers (`MOUSE_EVENT_FLAGS`, `KEYBD_EVENT_FLAGS`) — combine
  with `|`.
- Set a sentinel in **`dwExtraInfo`** on every injected event so the viewer-side
  `WH_KEYBOARD_LL` hook can recognize and ignore our own synthetic input (prevents feedback
  loops), exactly as the old `keyhook.js` checked `LLKHF_INJECTED`.

The `WH_KEYBOARD_LL` shortcut-capture hook (Alt+Tab/Win) from `keyhook.js` ports to
`SetWindowsHookExW(WH_KEYBOARD_LL, ...)` via `windows`: inspect `KBDLLHOOKSTRUCT`, return
`LRESULT(1)` to swallow, keep a message pump on the hook thread. (Cannot suppress the secure
Ctrl+Alt+Del SAS — by design.)

Alternative crate: **`enigo` 0.6.1 (MIT)** wraps SendInput with raw-scancode + absolute-coord
support; use it only if its abstraction covers our full input set — we call `windows` directly
because we need explicit `VIRTUALDESK` normalization over a specific monitor rectangle, precise
`0xE0`/`HWHEEL` control, and `dwExtraInfo` self-tagging.

### 8b. Elevation — control UAC / lock / secure desktop like AnyDesk (`service/` + `crates/elevation`)

**Why a service is mandatory (from research):** UAC prompts and the login/lock screen render
on the **secure desktop**, which "only Windows processes can access." **UIAccess is NOT
sufficient** for the SYSTEM secure desktop (Microsoft: "setting UIAccess has no effect" for
system-IL UI). AnyDesk/TeamViewer/RustDesk all require an installed SYSTEM service. So:

**Injection reach — the SYSTEM service covers everything (no code-signing needed):**
1. **Normal windows** → plain SendInput from the user-session engine (medium IL).
2. **Elevated app windows** (Task Manager, elevated apps) → routed through the **SYSTEM
   service's inject helper**, which is SYSTEM integrity (higher than high-IL elevated
   windows) and so can drive them.
3. **UAC prompt / lock / login (secure desktop)** → the **SYSTEM service** does it (only
   SYSTEM/Windows processes can reach the secure desktop).

**UIAccess is deliberately NOT used** — it was a lighter middle tier for elevated windows, but
the SYSTEM service already covers tiers 2 and 3, so UIAccess would be redundant. This matters
for cost: UIAccess is the *only* thing that would force Authenticode code-signing + a
Program-Files "secure location". By routing all privileged injection through the SYSTEM
service instead, **the app needs no code-signing certificate to be fully functional** (§11.4).
A user-mode Windows service installs and runs unsigned; it only needs admin rights **once** at
install to register. The one cost of skipping signing is a **SmartScreen "Run anyway" prompt**
on first install (and possible AV-heuristic flags common to all remote-access tools) — a
first-install click, not a capability loss.

**SYSTEM service design (clean-room from RustDesk's public architecture, our own code):**
- Install a Windows service (`--install-service`, requires admin) running as **LocalSystem**.
- The service **does not host UI itself** (Microsoft warns against it). It:
  - detects the active session: `WTSGetActiveConsoleSessionId` + `WTSEnumerateSessions`
    (distinguish console vs RDP);
  - gets the user's primary token: `WTSQueryUserToken` (needs LocalSystem + SE_TCB_NAME);
  - launches the **engine** into the user's session with **`CreateProcessAsUser`**,
    `STARTUPINFO.lpDesktop = "winsta0\\default"`, `CreateEnvironmentBlock`;
  - relaunches the engine whenever the active session changes (login, fast-user-switch, RDP).
- A **SYSTEM-context capture/inject helper thread** follows the input desktop: before each
  injected input event, call `OpenInputDesktop` + `GetUserObjectInformation(UOI_NAME)` and, if
  the input desktop changed, `SetThreadDesktop` to re-attach to it (Default ↔ Winlogon ↔
  secure). This is exactly how the secure desktop / UAC prompt gets captured and clicked.
  Capture side reacts to `desktop_changed()` by re-initializing DDA on the new desktop.
- Service ↔ engine IPC over a named pipe.

**Honest limitations to document:** cannot silently bypass UAC (the local user must still
click Yes on a real consent prompt if credentials are required — same as AnyDesk); DRM/
protected content stays black; a portable (non-installed) mode can still elevate per-session
but not reach the secure desktop without the service.

---

## 9. Identity, config & security (reused, in `crates/protocol` + `crates/signaling`)

- **UUID / config schema** unchanged from `00-OVERVIEW §5` (Rust struct + serde; persist to
  the same JSON in the user data dir). Password stored as SHA-256; **challenge-response**
  verification from the current app (`verifyProof`: `SHA256(SHA256(pw)+":"+nonce)`) ports
  directly — plaintext never crosses the wire.
- **Access modes** (approve popup / password auto-grant, live view↔control switch) reused;
  the approval popup is a WebView2 modal driven by a Rust command, same UX.
- **Signaling** speaks the exact `00-OVERVIEW §3` protocol to the Cloudflare Worker.
- **Media/input encryption** is DTLS (str0m), always on.

---

## 10. Build, signing, packaging, CI

- **Build:** `cargo build --release` per crate; Tauri bundles the app. Two shippable
  binaries: the **engine/app** (UIAccess-manifested) and the **SYSTEM service**.
- **Code signing is OPTIONAL** (see §8b / §11.4): because all privileged injection goes
  through the SYSTEM service (not UIAccess), the app is fully functional **unsigned**. Skipping
  it costs only a SmartScreen "Run anyway" on first install. Sign later ($0 for dev via a
  self-trusted cert; ~$200/yr commercial, or Azure Trusted Signing ~$10/mo) once there are
  real users, to remove the warning and reduce AV false-positives — a "when you have traction"
  expense, not a "to build it" one.
- **Installer:** NSIS/WiX that installs the engine/app + registers the SYSTEM service
  (prompts for admin once). Program Files install is good practice but no longer *required*
  (that was a UIAccess constraint, now dropped). The old `electron-builder` release pipeline
  is replaced; the version-gated GitHub Actions release idea from `02`/prior carries over, now
  building Rust + Tauri and running `cargo-deny` as a gate.
- **CI gates:** `cargo build`, `cargo test`, **`cargo-deny check`** (license + banned-crate
  enforcement, §11), `cargo-about` to regenerate the third-party-licenses page.

---

## 11. Licensing & clean-room compliance (the part that keeps it proprietary + legal)

**11.1 `cargo-deny` (`deny.toml`) — CI-enforced allowlist.**
- **Allow:** MIT, Apache-2.0, BSD-2/3-Clause, ISC, Zlib, Unicode-DFS, MPL-2.0 (file-level
  copyleft — OK for proprietary as long as we don't modify those files without publishing
  just the file), CC0.
- **Deny (build fails):** **GPL-\*, AGPL-\*, LGPL-\*** (LGPL's relink obligation is a
  headache — avoid), SSPL, and **any crate with no license / "non-standard" unverified
  license**.
- **Explicitly banned crates** (found during research):
  - `dxgcap`, `dxgcap2`, `captrs` — AGPL-3.0 (capture)
  - `rustdesk-org/hwcodec` — **no license file** (all-rights-reserved) + AGPL-org association
  - anything pulling **FFmpeg with `--enable-gpl`**, **x264**, **x265** — GPL
  - `rivet-codec` — "non-standard" unverified license (until its file is read)
- Prefer `reed-solomon-simd` over the maintenance-seeking `reed-solomon-erasure`; note
  `raptorq` (Apache-2.0) has a historical Qualcomm patent caution if chosen.

**11.2 AGPL clean-room do/don't (RustDesk):**
- ❌ Never paste or adapt RustDesk source into `native/` — not even "as a reference to edit."
- ✅ Learn the *approach* (which API, which sequence) from its public architecture and from
  the research reports here, then implement from **Microsoft/vendor primary docs**.
- ✅ Treat looking at RustDesk like reading a blog post: absorb the concept, close it, write
  our own. The DDA + MF + SendInput + service pattern is fully documented by Microsoft
  independently of RustDesk.
- Keep a short written record that each `crates/*` module was implemented from primary docs.

**11.3 Codec patent royalties (separate from software license — flag for counsel):**
- **Patent ≠ copyright.** A permissive (BSD/MIT) *software* license on an H.264/HEVC codec
  does **not** grant the standard's **patent** rights (Via LA AVC pool for H.264; multiple
  pools incl. Via LA / Access Advance for HEVC). These royalties are **independent of** the
  Rust/MF software license and clean-room design solves neither.
- Using Windows' built-in Media Foundation encoder / the GPU's hardware encoder generally
  means Microsoft / the GPU vendor licensed the codec block — but this does **not**
  automatically clear your *application's* obligation in all cases. **Get IP counsel to
  confirm** before commercial shipping.
- **Royalty-free codec options** (structured to avoid patent pools) if H.264/HEVC royalties
  are a concern: **AV1** (AOMedia, BSD-2 + AOM Patent License — the cleanest long-term
  default once decode install-base is wide), **VP9/VP8** (libvpx, BSD-3, royalty-free).
  **OpenH264** is BSD-2 *and* carries Cisco's patent grant **only for the prebuilt binary
  Cisco ships** — self-compiling forfeits that coverage, so it's a narrow option.
- Vendor SDK *software* licenses are all commercial-safe and royalty-free: NVENC (NVIDIA
  SLA, object-code redistribution), AMD AMF (MIT), Intel VPL (MIT).
- The clean-room legal footing is well established (*Google v. Oracle*, 2021: reimplementing
  an interface is fair use) — you may use the same Win32/vendor APIs RustDesk uses; you may
  not copy its expression.

**11.4 Zero-cost shipping — the deliberate posture.** The app is designed to owe **nothing up
front**:

| Potential cost | How it's avoided | What (if anything) you give up |
|---|---|---|
| Code-signing certificate | All elevation via the SYSTEM service, not UIAccess → runs unsigned (§8b) | SmartScreen "Run anyway" on first install; AV false-positives |
| H.264/HEVC patent royalty | Default to H.264 **hardware** encoder (OS/GPU-vendor-licensed, common-case covered); AV1 (royalty-free) negotiated up and available as the guaranteed-$0 fallback | Nothing for the default path; AV1 has narrower HW support if you ever *must* switch |
| x264/x265 commercial license | Never used — hardware + permissive codecs only | Nothing |
| All crates/SDKs/APIs | MIT/Apache/BSD + OS APIs + royalty-free vendor SDKs | Nothing |
| Signaling | Cloudflare Worker free tier (already deployed) | Nothing |
| TURN relay (~10–15% of sessions) | Cloudflare free tier or coturn on a free-tier VPS | Nothing meaningful |

**Recommended default:** H.264 hardware, unsigned, Cloudflare free tier — **$0 to build and
ship**. Revisit signing (~$200/yr) and the codec-royalty question with counsel *only once
there is real traction*, not before. This is a business decision recorded here so the build
doesn't accidentally take on a dependency (a paid codec, a mandatory cert) that isn't needed.

---

## 12. Execution notes for parallel agents

Because this is built in one pass, split the workspace crates across agents; the only hard
ordering constraints are:

1. **`crates/protocol`** (shared types) is written first — everything depends on it.
2. **A latency smoke-test is the single go/no-go**: a throwaway harness wiring
   `capture → codec(encode) → transport → codec(decode) → render` on two machines, measuring
   glass-to-glass. If this doesn't hit ~40 ms, the whole premise fails — validate it before
   polishing UI/elevation. (This is the one place I'd de-risk even in a "whole thing at once"
   build; it is a test, not a shipped stage.)
3. `capture`, `codec`, `transport`, `input`, `render`, `signaling`, `elevation` are
   otherwise independent and parallelizable behind the `protocol` interfaces.
4. `engine` and `app` integrate last; `service` depends only on `elevation` + `engine`.

Everything else (per-crate API specifics, exact structs, error handling) is in §5–§8 and the
research is captured there. Keep each crate's external dependency to the versions/licenses in
§3, and run `cargo-deny` from day one so a copyleft crate can never slip in.
