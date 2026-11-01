<!-- Generated 2026-10-31 by a parallel code-reading sweep (20 haiku readers, one per subsystem). Line numbers refer to the tree at commit 5a0d195. Facts were spot-checked but not every line was re-verified; treat as a map, not a spec. -->

# plan-04-native-rewrite

## summary
Plan 04 (`C:/Yanivs/shareCtrlScreen/pans/04-NATIVE-REWRITE-PLAN.md`, 610 lines, read in full) specifies a Windows-only, closed-source native rewrite of the Electron remote-desktop app. The stack is Rust, a Cargo workspace under `native/` with crates protocol, capture, codec, transport, signaling, input, render, elevation and engine, plus an `app/` Tauri 2 shell (WebView2) and a `service/` SYSTEM Windows service (L128-151). The design is: DXGI Desktop Duplication capture, Media Foundation hardware H.264 encode with zero B-frames and LTR recovery (no intra-refresh on MF), str0m data channels with no jitter buffer, FEC via reed-solomon-simd, D3D11VA hardware decode, and a flip-model D3D11 present. Input goes through SendInput with a scan-code table and a WH_KEYBOARD_LL hook, and the viewer captures input via Raw Input on the native video HWND (L368-404). Elevation is handled by a SYSTEM service that follows the input desktop and reaches UAC/lock/secure desktop (L446-490). The plan explicitly contains no existing source code. It cites external files that are NOT in this document and were not read here: `00-OVERVIEW-AND-PROTOCOL.md`, `02-PLAN §7.4`, `input.js`, `keyhook.js`, `verifyProof`, and the Electron `renderer/` (L34-37, L416, L418, L436, L359, L496-499). Every Windows-specific item below is a planned API, not existing code. Cites are to plan line numbers.

## public_api
- No pub Rust items exist; the plan defines only crate boundaries and responsibilities (L128-151): crates/protocol (shared input/control/config/signaling enums, L134-136), crates/capture (ID3D11Texture2D + dirty rects + cursor, L137), crates/codec (only crate touching MF/NVENC, encode and D3D11VA decode, L138), crates/transport (str0m, channels, FEC, BWE loop, packetization, L139), crates/signaling (WebSocket client to the Cloudflare Worker, L140), crates/input (SendInput + WH_KEYBOARD_LL, L141), crates/render (flip-model swapchain, NV12->RGB shader, cursor sprite, present loop, L142), crates/elevation (SYSTEM service, desktop-follow, CreateProcessAsUser, L143), crates/engine (orchestration, L144), app (Tauri binary with #[tauri::command]s, L145-148), service (SYSTEM binary, L149). Also planned: the `Channel`-based Tauri event bridge (L363) and the `video`/`ctl`/`cursor` data-channel contract (L324-329).

## platform_deps
- **Tauri 2 shell on WebView2 (wry/tao windowed controller); WebView2 is the only UI host** @ L26, L87, L358, L368-369
  - purpose: web UI for home/settings/approval/session bar; IPC via invoke/listen/Channel; capability JSON gates IPC
  - portable: macOS and Linux need a native webview host with the same IPC bridge contract (`window.__TAURI__` with withGlobalTauri, invoke, event, Channel). Airspace limit (opaque native rectangle under the webview) is a property of the windowed webview and applies on those OSes too.
- **Native child HWND parented under Tauri window (WebviewWindow::hwnd(), CreateWindowEx, SetParent) for video (Option A, v1)** @ L373-377
  - purpose: hosts the video surface beneath/beside the web chrome with a hard seam
  - portable: Needs an equivalent native child surface attached to the window's native handle on each OS. Option B (composition-hosted WebView2 with DirectComposition sibling, transparent webview, put_DefaultBackgroundColor alpha=0) is documented as a non-v1 fallback (L379-384).
- **DXGI factory/adapter/output enumeration: CreateDXGIFactory1, EnumAdapters1, EnumOutputs, IDXGIOutput -> IDXGIOutput5::DuplicateOutput1 (BGRA always; R16G16B16A16_FLOAT and R10G10B10A2_UNORM for HDR); fallback IDXGIOutput1::DuplicateOutput (BGRA only)** @ L164-167
  - purpose: capture source for the primary display
  - portable: Behavioural contract for any replacement: a per-display capture session delivering full-resolution native-pixel frames (no DPI virtualization, L216), a format list with an always-available 8-bit BGRA-equivalent, and an HDR format when supported. Must be creatable on a fresh session/stream; must be re-creatable after loss.
- **DDA per-frame loop: AcquireNextFrame(timeout) -> cast to ID3D11Texture2D -> DXGI_OUTDUPL_FRAME_INFO -> ReleaseFrame; max 4 duplications per session** @ L171-174
  - purpose: event-driven frame acquisition; hold at most one frame
  - portable: Replacement needs: an event-driven 'frame changed or timeout' acquire returning a GPU-resident texture handle, a metadata struct (AccumulatedFrames, LastPresentTime, rect buffers, pointer info), and an explicit release that must be prompt. Per-session duplication cap of 4 is a DXGI limit to be re-checked on other OSes.
- **Move rects and dirty rects: GetFrameMoveRects then GetFrameDirtyRects into a buffer sized by TotalMetadataBufferSize; moves processed before dirties** @ L176-181
  - purpose: encode only changed regions via CopySubresourceRegion into the encoder input texture
  - portable: Behavioural contract: per-frame moved and dirty regions must be delivered (or synthesised by diffing) and applied in move-then-dirty order. Other OS capture APIs may give dirty regions but not move rects, and the diffing fallback has to be specified.
- **Pointer position every frame (PointerPosition) and pointer shape only when PointerShapeBufferSize!=0 (GetFramePointerShape); three shape types MONOCHROME (height = Height/2, AND/XOR), COLOR (straight BGRA), MASKED_COLOR (alpha as mask)** @ L183-187
  - purpose: cursor sent out-of-band on reliable channel, drawn as a client-side sprite
  - portable: Replacement must surface cursor position at high rate independent of video frames, and shape changes with the same three encodings or a canonical conversion. Cursor may be hidden or composited by the OS depending on the capture API, so this needs to be stated per OS.
- **Access-lost / re-init paths: DXGI_ERROR_ACCESS_LOST (UAC, lock, mode change, DWM toggle, fullscreen, TDR) -> release, drop and re-DuplicateOutput with backoff; WM_DISPLAYCHANGE -> re-enumerate from a fresh factory; E_ACCESSDENIED (secure desktop, needs SYSTEM helper); DXGI_ERROR_UNSUPPORTED; DXGI_ERROR_SESSION_DISCONNECTED; DEVICE_REMOVED -> recreate device** @ L210-214, L483-484
  - purpose: capture survives desktop/mode/session changes
  - portable: Each OS needs an equivalent error taxonomy mapped to: recoverable-with-backoff, needs-privileged-helper, unsupported, session gone, device lost. Desktop switch (secure desktop) reinit is triggered by desktop_changed() on the capture side (L484).
- **D3D11 device created once and shared with encode: D3D11_CREATE_DEVICE_VIDEO_SUPPORT; ID3D11Multithread::SetMultithreadProtected(TRUE)** @ L168-169
  - purpose: one device for capture, colour conversion, encode and decode textures
  - portable: Replacement: one GPU device/context shared by capture, colour conversion, encoder and decoder, with thread-protected access. Zero-copy depends on all stages sharing it.
- **DPI: Per-Monitor-V2 awareness via application manifest** @ L216-218
  - purpose: overlay and remote-cursor coordinate math align
  - portable: Equivalent: a DPI/scale-aware process setting on each OS, with coordinates kept in native physical pixels.
- **Media Foundation hardware encoder: MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, HARDWARE|ASYNCMFT|SORTANDFILTER) for MFVideoFormat_H264/HEVC; ActivateObject; check MF_SA_D3D11_AWARE; MF_TRANSFORM_ASYNC_UNLOCK=TRUE** @ L233-236
  - purpose: vendor-agnostic HW encode (the correctness baseline on all GPUs)
  - portable: Per-OS equivalent: a hardware encoder API that accepts GPU-resident NV12 input and runs async. The plan's MF requirements are the behavioural contract: async input/output, ICodecAPI-like low-latency knobs, and per-vendor verification that they take effect (L288-290).
- **Zero-copy GPU input: MFCreateDXGIDeviceManager, ResetDevice(shared device), ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER); MFCreateDXGISurfaceBuffer + MFCreateSample + AddBuffer; ProcessInput/ProcessOutput; events METransformNeedInput / METransformHaveOutput** @ L238-244
  - purpose: encoder consumes GPU NV12 textures without CPU copies
  - portable: Contract: the encoder must accept a GPU texture handle with timestamps, with a need-input/have-output event loop, and must share the capture/convert device.
- **BGRA->NV12 colour conversion on GPU: Video Processor MFT (CLSID_VideoProcessorMFT) or compute shader; never CPU** @ L239-241, L66
  - purpose: every HW encoder wants NV12
  - portable: Contract: GPU-side BGRA-to-NV12 with no readback. A compute shader is fully portable across GPU APIs; the plan leaves the choice open between the MFT and a shader.
- **ICodecAPI properties: CODECAPI_AVLowLatencyMode (TRUE), AVEncCommonRateControlMode (CBR), AVEncCommonMeanBitRate (BWE-driven), AVEncCommonBufferSize (small), AVEncMPVDefaultBPictureCount=0, AVEncMPVGOPSize (effectively infinite), AVEncVideoForceKeyFrame (connect/explicit decoder loss only), slices per frame** @ L246-254
  - purpose: zero-B-frame, no-lookahead, CBR, no periodic IDR
  - portable: Replacement encoder must support: no B-frames, no lookahead or reorder, CBR with runtime bitrate changes, very long GOP with on-demand IDR, and slice count control. Each property needs a per-OS equivalent or a documented gap.
- **Long-term reference frames: CODECAPI_AVEncVideoLTRBufferControl, AVEncVideoMarkLTRFrame, AVEncVideoUseLTRFrame; viewer ACKs last clean frame; NVENC-only intra-refresh (enableIntraRefresh, intraRefreshPeriod/Cnt)** @ L256-262
  - purpose: loss recovery without IDR hitches
  - portable: Contract: encoder must support marking and referencing LTR frames by viewer ACK. The plan states MF has no intra-refresh, so intra-refresh is a vendor-specific optimization.
- **Region-of-interest QP: NVENC emphasis/QP-delta maps (NV_ENC_PIC_PARAMS); MF gives only frame-level QP** @ L264-285
  - purpose: low QP on text/UI regions, higher QP on video
  - portable: Treat as an optional encoder capability (NVENC-path only in the plan). Region classification (edge/colour-count on a downscaled copy of dirty rects, L268-270) is OS-agnostic.
- **Viewer decode: raw D3D11VA (ID3D11VideoDevice -> CreateVideoDecoder -> DecoderBeginFrame/SubmitDecoderBuffers/DecoderEndFrame into a BIND_DECODER|BIND_SHADER_RESOURCE texture array); alternative MF decoder MFT with IMFDXGIDeviceManager and MF_LOW_LATENCY (decoder needs CODECAPI_AVLowLatencyMode as VT_UI4 not VT_BOOL)** @ L292-299
  - purpose: ~1-3 ms HW decode into a shader-readable texture
  - portable: Contract: hardware decode into GPU-readable surfaces, low-latency mode, no reorder, and a texture-array-slice or copy path. Per-GPU-vendor zero-copy gating with copy fallback (L311-312).
- **Texture-array pitfall: decoded frames are slices of a texture array + index; cache one SRV per pool slice at init (per-frame SRV creation about 3x CPU); zero-copy only if driver honours shader-readable decode surfaces, else CopySubresourceRegion** @ L306-312
  - purpose: avoid per-frame view creation and extra copies
  - portable: Replacement: pooled decode surfaces with per-slot resource views created once. Zero-copy eligibility is driver-dependent on every OS.
- **Present: DXGI_SWAP_EFFECT_FLIP_DISCARD, BufferCount 2, FRAME_LATENCY_WAITABLE_OBJECT | ALLOW_TEARING, IDXGISwapChain2::SetMaximumFrameLatency(1), WaitForSingleObjectEx(waitable) then render then Present(0, DXGI_PRESENT_ALLOW_TEARING); IDXGIFactory5::CheckFeatureSupport for tearing; CreateSwapChainForHwnd (Option A) or CreateSwapChainForComposition (Option B)** @ L386-392
  - purpose: minimum-latency presentation, one frame in flight
  - portable: Contract: a swapchain with max 1 queued frame, a waitable present gate (wait then render), tearing/unthrottled present where supported, and a feature query. Other OSes need equivalent low-latency present (queue depth 1, vsync off where supported).
- **NV12->RGB pixel shader: two SRVs (R8_UNORM luma, R8G8_UNORM chroma; R16 / R16G16 for 10-bit); BT.709 matrix + range scale; cursor drawn as a separate sprite at PointerPosition** @ L392-395
  - purpose: display decode output
  - portable: Shader logic is OS-agnostic (matrix, range and 10-bit handling), but the resource formats and shader language are GPU-API specific.
- **Input over the video: RegisterRawInputDevices -> WM_INPUT on the native video HWND wndproc (raw deltas, RAWKEYBOARD.MakeCode scancodes, high-Hz batch, background capture); WM_MOUSEACTIVATE and AcceleratorKeyPressed for focus between video HWND and WebView2** @ L397-404
  - purpose: viewer-side input capture that is decisive for correctness
  - portable: Contract: raw relative mouse deltas (no acceleration), hardware scancodes with extended-key flag, background capture while the window is focused, and focus arbitration so keys do not get trapped in the webview. Webview JS invoke is for non-latency-critical chrome only.
- **SendInput injection: INPUT { r#type, Anonymous.mi/ki }; SendInput(slice, cbsize = size_of::<INPUT>()); MOUSEEVENTF_ABSOLUTE|VIRTUALDESK|MOVE normalised 0..65535 across virtual desktop (multiply in i64); KEYEVENTF_SCANCODE and KEYEVENTF_EXTENDEDKEY; wheel mouseData cast (delta as i32) as u32; return 0 = blocked by UIPI** @ L411-430
  - purpose: synthetic mouse and keyboard in the user session and on elevated windows via the SYSTEM helper
  - portable: Behavioural contract: absolute coordinates normalised over the full multi-monitor desktop, scan-code keyboard injection with extended-key flag, signed wheel delta, a blocked-injection signal that escalates to the privileged path, and a per-event self-tag so the local hook ignores injected input. Scan-code table is documented as Set-1 bytes identical across languages (L416); other OSes will need their own key-code mapping from the same DOM-code source.
- **Self-tag in dwExtraInfo on every injected event (sentinel), checked by the viewer-side hook to ignore synthetic input (replaces LLKHF_INJECTED check in old keyhook.js)** @ L432-434
  - purpose: prevents feedback loops between injection and the keyboard hook
  - portable: Equivalent: a per-event marker field in injected events that the local capture/hook can recognise. The plan treats this as mandatory.
- **WH_KEYBOARD_LL via SetWindowsHookExW: inspect KBDLLHOOKSTRUCT, return LRESULT(1) to swallow, message pump on the hook thread; cannot suppress secure Ctrl+Alt+Del SAS** @ L436-439
  - purpose: capture Alt+Tab and Win shortcuts from the viewer side
  - portable: Behavioural contract: a system-level keyboard tap that can swallow events, runs on its own pumped thread, and cannot intercept the OS secure attention sequence. Other OSes need the equivalent (and typically extra permissions).
- **UIAccess is explicitly NOT used (L461-469); the plan contradicts itself at L510 by describing the engine/app as UIAccess-manifested** @ L461-469, L510
  - purpose: elevated-window reach comes from the SYSTEM service instead
  - portable: Resolve before implementation: the code-signing and Program Files requirement only applies if UIAccess is used, and the plan's zero-signing posture depends on dropping it (L464-466, L585).
- **SYSTEM service: install with --install-service (admin); LocalSystem; WTSGetActiveConsoleSessionId + WTSEnumerateSessions (console vs RDP); WTSQueryUserToken (needs SE_TCB_NAME); CreateProcessAsUser with STARTUPINFO.lpDesktop = winsta0\\default and CreateEnvironmentBlock; relaunch the engine on session change (login, fast-user switch, RDP); the service hosts no UI** @ L471-479
  - purpose: runs the engine in the user session and on the secure desktop
  - portable: Per-OS equivalents: a privileged background daemon/launch-agent/systemd unit that tracks the active user session and starts the engine as that user with its environment. Each OS has different session models (Linux Wayland/X11, macOS login and fast-user switching), which the plan does not cover.
- **Input-desktop follower in the SYSTEM helper thread: before each injected event, OpenInputDesktop + GetUserObjectInformation(UOI_NAME); if changed, SetThreadDesktop (Default, Winlogon, secure)** @ L480-484
  - purpose: capture and click the UAC prompt and lock screen
  - portable: Behavioural contract: detect the active secure input context and re-attach capture and injection to it. macOS and Linux do not have the same secure desktop model; this has to be specified per OS.
- **Service <-> engine IPC over a named pipe** @ L485
  - purpose: control channel between the SYSTEM service and the user-session engine
  - portable: Contract: a local IPC channel with access control, ordered and reliable, usable by both privileged and unprivileged processes.
- **Installer NSIS/WiX (prompts for admin once); Authenticode signing optional (SmartScreen first-run prompt); Azure Trusted Signing or self-signed cert later** @ L517, L466-469, L511-516
  - purpose: packaging and first-install trust
  - portable: Each OS has a different packaging, signing and notarisation regime. The plan has no equivalent for macOS or Linux. Zero-signing is a Windows-specific cost decision (L585).
- **Build and CI: cargo build --release per crate; cargo test; cargo-deny check; cargo-about; Tauri bundles app; GitHub Actions on Windows implied by the old release pipeline (L509-523)** @ L509-523
  - purpose: build and licence gates
  - portable: The CI runner is implicitly Windows-only (NSIS/WiX, Windows SDK). No macOS or Linux runners or targets are named in the plan.

## portable_parts
- crates/protocol: shared data types, input/control/config/signalling enums (L128-136, L597). Planned as OS-agnostic.
- Signalling (crates/signaling): WebSocket client to the Cloudflare Worker with an unchanged JSON protocol, `data` treated as opaque (L34-37, L140, L345-346, L502). OS-agnostic by design.
- Transport (crates/transport): str0m sans-IO WebRTC, data channels only, no jitter buffer, no threads or timers, the app owns I/O (L316-352). The plan says str0m needs no OS-specific code. UDP I/O uses quinn-udp, which is cross-platform (L92). DTLS, ICE and SCTP framing are in str0m (L318-319, L350).
- FEC via reed-solomon-simd, runtime SIMD, no OS dependency (L93, L334-337). Loss strategy: heavy FEC on keyframe/LTR-ACK packets, drop delta frames, selective NACK (L337).
- BWE to bitrate loop: TWCC-based BWE, bytes/s fed to the encoder CBR target at tens-of-ms cadence (L340-342). Logic is OS-agnostic but depends on an encoder that accepts runtime bitrate changes.
- Packetisation rule: app-fragmented datagrams of at most ~1200 B protected by FEC (L327). OS-agnostic.
- Region classification heuristic on dirty rects (edge and colour count on a downscaled copy, no ML) (L268-270). OS-agnostic.
- Scan-code table from the DOM-code mapping in input.js / 02-PLAN §7.4, asserted to be byte-identical across languages (L416). Referenced files are not in this document. A non-Windows target needs its own keycode table from the same source (inference, not stated in the plan).
- Config schema, UUID identity, SHA-256 password hashing and challenge-response `SHA256(SHA256(pw)+":"+nonce)` (L496-499). Plaintext never crosses the wire. OS-agnostic, but this also depends on external files (`00-OVERVIEW §5`, `verifyProof`).
- Access modes (approve popup, password auto-grant, live view/control switch) and the WebView2 modal approval UX (L500-501). Logic is portable. The approval UI runs in the webview.
- Web UI (renderer HTML/CSS/JS) ported with withGlobalTauri (L26, L147-148, L359-363). OS-agnostic web code, with IPC mapping ipcRenderer->window.__TAURI__.core.invoke, event.listen, Channel (L360-363).
- Tauri capability JSON for IPC allow-listing (L364-365). Config-level and OS-agnostic in intent.
- Colour-conversion shader logic (BT.709 matrix, range scaling, 10-bit handling) (L392-393). Matrix maths is portable; the shader language and resource formats are not.
- Dirty-rect and LTR protocol state machines, the heartbeat floor at 1-2 fps, and the idle-silent rule (L195-200). Logic is OS-agnostic.
- Latency smoke-test harness (L598-602), defined as a capture-to-render harness, which the plan says is OS-agnostic in structure.

## abstraction_points
- `Capture` trait or interface (crates/capture): `acquire(timeout) -> Frame{texture_handle, dirty_rects, move_rects, pointer{position, shape?}, accumulated_frames, last_present_time}`; `release(frame)`; re-init on access-lost/desktop-change/display-change; explicit error taxonomy (recoverable-with-backoff, needs-privileged-helper, unsupported, session-gone, device-lost). Implementations: DDA (L171-214), WGC fallback (L222-225). Other OSes add their own implementation; the plan has no name for this trait, so one has to be defined. (inference: the plan names the crate but no trait)
- Cursor-source contract: position at high rate independent of video frames, and shape with three encodings or a canonical conversion (L183-187).
- `GpuDevice` shared handle (L168-169, L238-239): one device owned by capture, colour conversion, encoder and decoder. Thread-protected. Per-OS: device creation and sharing semantics differ.
- `Encoder` trait (crates/codec): takes GPU NV12 textures with timestamps, async need-input/have-output loop, runtime bitrate change, force-IDR, LTR mark/use, low-latency knobs. Impls: MF (baseline, L233-262), NVENC-direct (optional, adds intra-refresh and ROI-QP, L261, L275-279). Codec selection H.264 default, HEVC/AV1 negotiated (L98-101).
- `Decoder` trait (codec, viewer side): hardware decode into pooled GPU-readable surfaces with per-slot views; zero-copy vs copy path gated per vendor (L292-312).
- `Presenter` / render surface: flip-model waitable swapchain with wait-then-render; a native child surface (Option A) or composition surface (Option B) (L373-395). Abstraction is required because Option B changes the hosting model.
- `InputInjector` (crates/input): absolute/relative/wheel/key events over a normalised virtual-desktop coordinate space, self-tagging, return-code escalation to the privileged path (L411-430).
- `InputCapture` (viewer): raw relative pointer and hardware scancode feed over the video window; focus arbitration with the webview (L397-404).
- `KeyboardHook` (WH_KEYBOARD_LL equivalent, L436-439): system-wide shortcut capture that can swallow events, with a self-tag filter (L432-434).
- `DesktopFollower` / SYSTEM helper (L480-484) and `SessionLauncher` (L471-479): per-OS privileged helper and session-follow behaviour. Not designed for macOS or Linux.
- Named-pipe IPC between the service and the engine (L485). Needs a per-OS transport, and the plan does not name the abstraction.
- Transport wrapper around str0m: channels (video/ctl/cursor, L324-329), FEC, BWE. The plan treats str0m as OS-agnostic but depends on quinn-udp (L92).

## cross_module_contracts
- Build order (L597-604): `protocol` first (everything depends on it). Then capture, codec, transport, input, render, signaling and elevation in parallel behind protocol interfaces. Then engine and app. Then service, which depends on elevation and engine.
- Ownership and isolation (L154-156): `codec` is the only crate touching MF/NVENC; `capture` is the only crate touching DDA/WGC; `input` and `elevation` are the only crates doing SendInput and desktop switching. Other crates must not call these APIs directly.
- Shared GPU device (L168-169, L238-239): the encoder (MF) and the capture side share one D3D11 device. Decode and render also need to share that device or copy between devices. The plan does not state the device topology for the viewer side explicitly. (inference)
- Encoder bitrate feedback (L340-342): transport's BWE output must reach codec's CBR target (`AVEncCommonMeanBitRate`) at tens-of-ms cadence. A cross-crate control path from transport to codec.
- Keyframe and LTR control (L252, L256-261): the transport layer ACKs the last clean frame and requests forced IDR on connect or total desync. These messages flow from the viewer through transport to the encoder.
- Cursor path (L185-187, L201-202, L394-395): capture emits position and shape on the reliable `cursor` channel, render draws the sprite. Cursor motion must not wake the video encoder (L201-202).
- Input path (L397-404, L324-332): viewer raw input goes over the reliable `ctl` channel (keys, clicks) or an unreliable sub-stream (mouse move) to the host, which injects it via `input`. Injection escalates to the service helper on UIPI block (L425-426).
- Threading (L192, L242-244, L389, L438, L480): a waitable or QPC-driven tick as a ceiling on the capture loop. An async MFT event loop. A wait-then-render present loop. A WH_KEYBOARD_LL thread with a message pump. A SYSTEM helper thread that follows the input desktop. The plan does not name a thread for the transport I/O loop; str0m is sans-IO so the app must own it. (inference)
- Service-to-engine IPC (L485): named pipe. The service launches the engine via CreateProcessAsUser (L477) and relaunches on session change (L479). The engine is a child of the service and not a peer.
- Frame-level invariants (L173-174, L181, L195-203): hold at most one acquired frame; `AccumulatedFrames>1` means encode once for the newest; `LastPresentTime==0` means pointer-only update; send nothing on timeout beyond the heartbeat floor.
- No shared global or static state is specified in the plan. Cross-crate state passes through the protocol types and explicit channels (inference from L134-144).

## gotchas
- Latency invariants (L73-77): zero B-frames (L250); no jitter buffer anywhere (L338); no periodic IDR (L251, L260); `surfaces=1` / no lookahead (L67, L76, L247); no HTML `<video>` tag, decode into a native surface (L76-77). The stream AND the decoder both need low-latency mode; either missing adds a frame (L301-303).
- MF low-latency properties may silently no-op on some vendor MFTs. Verify at runtime on target hardware. This is why the NVENC path exists (L288-290).
- H.264 decoder quirk: set CODECAPI_AVLowLatencyMode as VT_UI4, not VT_BOOL (L298-299).
- `MFT_MESSAGE_COMMAND_DRAIN` only to evict a stuck frame, never per frame (L303-304).
- Texture-array decode output needs a cached SRV per slice, created at init; per-frame SRV is about 3x CPU (L310-311).
- Zero-copy decode is vendor-gated with a copy fallback (L311-312).
- DDA: hold at most one frame, release promptly (L173-174). Max 4 duplications per session (L174). Process moves before dirties (L177-178). Encode only the union of changed regions (L178-179).
- Cursor shape is cached; it is only fetched when `PointerShapeBufferSize != 0` (L185). Three shape types need separate handling (L186-187).
- DRM content (`ProtectedContentMaskedOut`) produces black frames. That is a hard platform limit with WGC the same (L220-221). Flip-model borderless windows duplicate more cleanly (L221-222). Headless machines need an indirect display driver (L222).
- Duplication surface is always native physical resolution, with no DPI virtualisation. Per-Monitor-V2 manifest is still required (L216-218).
- Secure desktop: E_ACCESSDENIED from DDA means only the SYSTEM helper can duplicate it (L212-213, L483-484).
- Injection: `SendInput` cbsize must be size_of::<INPUT>(). A wrong value silently returns 0 (L424). A return of 0 also means UIPI-blocked (L425-426). Never add repr(packed) to the INPUT structs (L427). Cast wheel delta through i32 to u32 (L428-429). Multiply absolute coordinates in i64 (L414).
- Injected events carry dwExtraInfo sentinel; the hook must ignore them, or there is a feedback loop (L432-434).
- Track injected-down keys and buttons so they can be released when the stream stops (stuck-input release, L417-418). Keys and clicks must be reliable, since a lost key-up means a stuck key (L331-332).
- Cannot suppress the secure Ctrl+Alt+Del SAS from WH_KEYBOARD_LL (L438-439).
- Raw Input focus: the topmost opaque video HWND receives mouse messages (L402). Focus is managed between the HWND and WebView2 via AcceleratorKeyPressed and WM_MOUSEACTIVATE to avoid keyboard trapping (L402-404).
- Option A (native child HWND under the webview) has a hard seam. Translucent HTML over video needs Option B, which is not v1 (L373-384).
- UIAccess is contradictory in the plan. L461-469 says UIAccess is NOT used. L510 says the engine/app is UIAccess-manifested. Resolve before implementation, since UIAccess would force signing and a secure install location (L463-465).
- The service must not host UI (L473-474). It should relaunch the engine on session change (L479).
- Idle behaviour: send nothing on WAIT_TIMEOUT or no dirty region. A heartbeat floor at 1-2 fps keeps decoder and LTR state warm. Cursor moves do not wake the encoder (L195-203).
- Honest limits (L487-490): cannot silently bypass UAC (the local user must click Yes); DRM stays black; portable (non-installed) mode can elevate per session but cannot reach the secure desktop without the service.
- Licence: AGPL RustDesk is an architecture reference only, no code, and a clean-room record is required per crate (L13-18, L544-551). Banned crates: dxgcap, dxgcap2, captrs, rustdesk-org/hwcodec, FFmpeg with --enable-gpl, x264, x265, rivet-codec (L536-542). cargo-deny runs from day one (L608-609).
- Codec patent royalties are separate from the software licence and need counsel review before commercial shipping (L553-561). The default H.264 hardware path is assumed to be covered by the OS or GPU vendor, which is not confirmed (L104-111, L558-561).
- The zero-cost posture (no signing, H.264 hardware, Cloudflare free tier) is a Windows-specific business decision. It is recorded at L573-588 and depends on UIAccess being dropped and on the SmartScreen cost being accepted.
- Three external documents are referenced and not included: 00-OVERVIEW-AND-PROTOCOL.md, 02-PLAN §7.4, input.js, keyhook.js, verifyProof. They must be read before the protocol and input port can be verified. The plan itself contains no code, so there are no source file:line cites for the current implementation.

## tests
- No per-crate unit or integration tests are specified in the plan (grep-level confirmation: only CI gates and one harness are named).
- CI gates (L522-523): `cargo build`, `cargo test`, `cargo-deny check` (licence and banned-crate enforcement), `cargo-about` to regenerate the third-party licence page. All run from day one (L608-609).
- Latency smoke test (L598-602, §12 item 2): a throwaway harness that wires capture -> codec(encode) -> transport -> codec(decode) -> render on two machines and measures glass-to-glass latency. The go/no-go target is about 40 ms (L599-601). Latency budget in §2 (L61-71) targets roughly 11-30 ms on wired LAN at 1080p60. The plan says this is a test, not a shipped stage.
- Vendor-specific runtime verification of MF low-latency properties on target hardware (L288-290). This is a required check, not a unit test.
- The plan names specific target machines in the sibling memory notes, not in this file. Not covered here.
