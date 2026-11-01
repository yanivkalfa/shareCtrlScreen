<!-- Generated 2026-10-31 by a parallel code-reading sweep (20 haiku readers, one per subsystem). Line numbers refer to the tree at commit 5a0d195. Facts were spot-checked but not every line was re-verified; treat as a map, not a spec. -->

# pipeline-part3 (lines 1790-2509: fit_within, file_send_loop, deliver_received_file, clipboard_loop, host_media_loop, viewer_media_loop, create_render_device, tests)

## summary
pipeline.rs lines 1790-2509 hold the file-push sender, received-file delivery, clipboard poll loop, host capture+encode loop (with adaptive pacing and keyframe policy), viewer decode+render loop, the D3D11 render-device factory, and unit tests. This is a Windows-only integration layer: the module doc (pipeline.rs:1-11) says "Windows media + transport pipeline", and COM objects are not Send, so each media stage runs on its own thread with its own D3D11 device. Of the roughly 720 lines, only fit_within (1791-1803), the control flow of file_send_loop (1810-1908), clipboard_loop (1957-1996), the pacing and keyframe-policy logic in host_media_loop (2064-2253), and the viewer's catch-up, keyframe-watchdog and size-debounce logic (2303-2418) are OS-agnostic. host_media_loop, viewer_media_loop and create_render_device are entangled with capture::Duplicator, codec::Encoder/Decoder, render::Renderer, render::window, input::Injector and windows-crate types. deliver_received_file depends on the Windows clipboard CF_HDROP path (via crate::clipboard::set_files), input::focus_window_at, and Ctrl+V injection. crate::clipboard is not in this unit, so its internals were not read here.

## public_api
- pipeline.rs:1791 fn fit_within(want:(u32,u32), src:(u32,u32))->(u32,u32). Private. Scales src to fit inside want, never upscales, rounds to even, minimum 2; returns src unchanged when any input is zero.
- pipeline.rs:1810 fn file_send_loop(drops: Receiver<render::window::FileDrop>, out: Sender<Vec<u8>>, ui: tokio::sync::mpsc::UnboundedSender<crate::UiEvent>, stop: Arc<AtomicBool>). Private blocking thread body.
- pipeline.rs:1922 fn deliver_received_file(path:&Path, drop_at:Option<(f64,f64)>, injector:Option<&mut input::Injector>)->bool. Private.
- pipeline.rs:1957 fn clipboard_loop(ctl_tx:Sender<Vec<u8>>, inbound:Receiver<String>, enabled:Arc<AtomicBool>, stop:Arc<AtomicBool>). Private blocking thread body.
- pipeline.rs:2000 fn host_media_loop(frame_tx:Sender<(Vec<u8>,bool)>, cursor_tx:Sender<Vec<u8>>, bitrate:Arc<AtomicU32>, link:Arc<AtomicU32>, force_key:Arc<AtomicBool>, stop:Arc<AtomicBool>)->Result<(),Box<dyn Error>>. Private blocking thread body.
- pipeline.rs:2259 fn viewer_media_loop(video_rx:Receiver<(Vec<u8>,bool)>, ctl_tx:Sender<Vec<u8>>, stop:Arc<AtomicBool>)->Result<(),Box<dyn Error>>. Private blocking thread body.
- pipeline.rs:2423 fn create_render_device()->Result<ID3D11Device,Box<dyn Error>>. Private; D3D11CreateDevice HARDWARE with VIDEO_SUPPORT|BGRA_SUPPORT at feature level 11_1 only.

## platform_deps
- **Win32 clipboard file list via crate::clipboard::set_files (CF_HDROP, the format Explorer writes on Ctrl+C of a file)** @ pipeline.rs:1927
  - purpose: Make a received file pasteable exactly like a locally copied file
  - portable: macOS: write file URLs to NSPasteboard (public.file-url). Linux: write text/uri-list plus x-special/gnome-copied-files (or an equivalent wl-clipboard/xclip path) so file managers and apps accept a paste. Contract: returns None on failure, the file must already exist on disk, and the clipboard must persist after the call returns.
- **Clipboard change counter via crate::clipboard::sequence() (GetClipboardSequenceNumber)** @ pipeline.rs:1963, 1982
  - purpose: Cheap polling for clipboard changes without a window, used for echo suppression
  - portable: macOS: NSPasteboard.changeCount (monotonic, cheap, no prompt). Linux/X11: no global counter, so use XFixes selection-owner notifications or poll the CLIPBOARD owner and timestamp. Linux/Wayland: no unprivileged change counter; needs wlr-data-control or wl-paste --watch. Contract: must be monotonic and must advance on our own writes, because the code stores the value returned by set_text and skips that echo.
- **Clipboard text read/write via crate::clipboard::get_text / set_text (CF_UNICODETEXT)** @ pipeline.rs:1975, 1985
  - purpose: Text sync in both directions
  - portable: macOS: NSPasteboardTypeString. Linux: CLIPBOARD and optionally PRIMARY. On X11 and Wayland the selection owner dies with its process unless a daemon keeps it alive. Contract: set_text must return the post-write sequence number. get_text returns None for non-text or on failure. MAX_TEXT_BYTES (1987) is defined in crate::clipboard and not read here.
- **Ctrl+V injection via input::Injector.key("ControlLeft"/"KeyV", down/up) with W3C-style key names** @ pipeline.rs:1943-1946
  - purpose: Trigger paste in the focused window
  - portable: macOS: Cmd+V via CGEvent with the command flag, which needs Accessibility (TCC) permission for the host binary. Linux/X11: XTEST fake key events. Wayland: needs a compositor-specific path or uinput (root or input group). The paste modifier must be platform-selected, not hardcoded to Ctrl.
- **Focus the window under a normalized point without clicking: input::focus_window_at(nx, ny)** @ pipeline.rs:1937
  - purpose: Bring the drop-target window to the foreground before typing
  - portable: macOS: find the window under the point (CGWindowListCopyWindowInfo), then activate its NSRunningApplication or raise via AXUIElement (Accessibility needed). Linux/X11: XQueryPointer plus _NET_ACTIVE_WINDOW. Wayland: no generic focus API. Contract: returns false when no window is under the point, and the file is then left on the clipboard. Windows restricts focus stealing through its foreground lock, which the memory notes say needs UIAccess; macOS has its own activation rules.
- **120 ms sleep after focus, before typing** @ pipeline.rs:1942
  - purpose: Let focus settle before key injection
  - portable: OS-agnostic timing heuristic. A port may need a real readiness check where activation is asynchronous (macOS, Wayland).
- **File metadata and reads via std::fs (portable), with Windows PathBuf paths from FileDrop** @ pipeline.rs:1828-1852
  - purpose: is_dir, len, open, read
  - portable: OS-agnostic code. The FileDrop producer (render::window::FileDrop) must supply native paths on each platform: percent-decoded file:// URIs for Linux DnD, NSURL file paths for macOS.
- **Viewer child native view handle: RENDER_HWND.load then HWND(hwnd_raw as *mut _); render::window::show(hwnd_raw), render::window::fit(hwnd_raw), render::window::client_size(hwnd_raw)** @ pipeline.rs:2266-2273, 2305-2307
  - purpose: Show the video surface over the web chrome, follow resizes and hover-reveal offsets, measure the client area for the size report
  - portable: Win32 WS_CHILD window under the WebView2 HWND. macOS: NSView or CAMetalLayer subview of the Tauri content view. Linux: X11 child XID, or a Wayland subsurface (no reparenting on Wayland, and WebKitGTK embedding differs). fit() runs every 100 ms loop pass, so it must be cheap and a no-op when nothing changed (comment at 2304). Ownership: the HWND is created on the Tauri thread (create_video_window, pipeline.rs:50) and only read here.
- **Viewer D3D11 device: D3D11CreateDevice(None, HARDWARE, HMODULE::default(), VIDEO_SUPPORT|BGRA_SUPPORT, [FL_11_1], D3D11_SDK_VERSION, &mut device, None, None) inside unsafe** @ pipeline.rs:2423-2447
  - purpose: One device shared by the Decoder and the Renderer on the viewer
  - portable: Replacement must give a hardware decode device with BGRA interop to the renderer. macOS: VideoToolbox session with a Metal device and CVMetalTextureCache. Linux: VA-API or V4L2 M2M decode with DMA-BUF import into GL or Vulkan. Feature level 11_1 is a hard requirement, and there is no software fallback here: failure propagates through ? at pipeline.rs:2277.
- **Host capture: capture::Duplicator::new(0,0), dup.dimensions(), dup.device(), dup.acquire(16ms), dup.release(), dup.reinit(). This is DXGI Desktop Duplication.** @ pipeline.rs:2010, 2015, 2026, 2155, 2199, 2241, 2245
  - purpose: Screen capture with dirty rects, move rects and pointer metadata
  - portable: Windows-only (DXGI output duplication). macOS: ScreenCaptureKit (SCStream provides dirty rects through attachments and cursor data separately; needs Screen Recording TCC permission). Linux: PipeWire screencast through xdg-desktop-portal on Wayland; XShm/XDamage on X11. Contract to keep: acquire(16ms) returns Ok(None) on idle (WAIT_TIMEOUT). Ok(Some(frame)) carries texture, dirty_rects, move_rects, pointer_only and cursor, and must be followed by release() on every path, including the early continue at 2200. Err(AccessLost) (desktop switch, UAC, mode change) requires reinit() and then an immediate keyframe (2245-2246).
- **Encoder shares the capture device for zero-copy: codec::Encoder::new(dup.device(), cfg); encoder.encode(&frame.texture) does BGRA to NV12 on the GPU and returns units with .data and .keyframe; set_bitrate(u32); force_keyframe()** @ pipeline.rs:2026, 2104, 2203, 2208, 2077, 2246
  - purpose: H.264, H.265 or AV1 encode through Media Foundation MFT (codec crate)
  - portable: Comment at 2025 says zero-copy (section 5c). macOS: VTCompressionSession fed by a CVPixelBuffer from ScreenCaptureKit (IOSurface, zero-copy). Linux: VA-API with DMA-BUF from PipeWire, or a CPU-copy software fallback (x264, SVT-AV1). Force-IDR and runtime bitrate change must both be supported.
- **Encoder rebuild on size change: Encoder::new(dup.device(), cfg) inside host_media_loop, memoized by rejected_size** @ pipeline.rs:2094-2121
  - purpose: Encode at the viewer's displayed size so it presents 1:1
  - portable: Platform encoder sessions are normally fixed at creation dimensions. The port must support recreate (VTCompressionSession, VA-API context) and must survive a failed rebuild without ending the stream.
- **Viewer decoder: codec::Decoder::new(&renderer_dev, codec, 1920, 1080) and decoder.decode(&au, keyframe, ts) returning Ok(Some(frame)) with frame.texture and frame.array_index. ts is in 100 ns MF units, incremented by 166_667 per AU.** @ pipeline.rs:2278, 2372, 2374-2392
  - purpose: Decode the network access units into a texture array
  - portable: The 1920x1080 initial dimensions (2278-2279) are hardcoded for both decoder and renderer, and the decoder is assumed to reconfigure on the first SPS. Timestamp workaround at 2369-2370 is MF-specific (software AV1 MFT stalls on pts 1,2,3). VideoToolbox and VA-API may not need it, but they should accept monotonic pts spaced about 16.7 ms apart. Ok(None) means the decoder needs more input.
- **Renderer: render::Renderer::new(&renderer_dev, hwnd, 1920, 1080) and renderer.render_frame(&texture, array_index, Option<(f64,f64)> cursor)** @ pipeline.rs:2279, 2391-2398
  - purpose: Present the decoded texture into the child window, with the out-of-band cursor sprite drawn on top
  - portable: Current design is a DXGI swapchain bound to the HWND (swapchain on the child, not on the WebView2 window, per the code notes). macOS: CAMetalLayer-backed view sampling the CVPixelBuffer as an MTLTexture. Linux: EGL or Vulkan surface on an X11 subwindow or a Wayland subsurface. The cursor is composited in the draw call, not as a separate window.
- **Win32 show: render::window::show(hwnd_raw), called once at viewer start, before device creation** @ pipeline.rs:2273
  - purpose: Reveal the video surface over the web chrome
  - portable: Equivalent of ShowWindow: NSView isHidden=false; X11 XMapWindow. Showing before the device exists means a later failure leaves a visible empty surface.

## portable_parts
- fit_within (pipeline.rs:1791-1803): pure function with no OS dependency. Its tests (2454-2475) are portable.
- file_send_loop control flow (pipeline.rs:1810-1908): the poll loop, folder and size rejection, chunking, Abort and End sequencing, and the size-mismatch check are OS-agnostic. Only the FileDrop type comes from the render crate.
- clipboard_loop control flow (pipeline.rs:1957-1996): the echo-suppression algorithm is OS-agnostic given a sequence/get/set abstraction.
- host_media_loop pacing and keyframe policy (pipeline.rs:2044-2212, 2224-2252): token-bucket pacing, the MIN_KEYFRAME_GAP limiter, the rebuild memo and the size-to-encode policy are all pure logic. Only the calls into Duplicator, Encoder and the frame texture are platform-coupled.
- viewer_media_loop catch-up, keyframe watchdog, awaiting_keyframe gate and size-debounce logic (pipeline.rs:2285-2418, excluding the render and window calls): OS-agnostic state machine.
- STUN parser tests (pipeline.rs:2477-2508), though the parser itself is defined outside this range (line 374).

## abstraction_points
- pipeline.rs:2000 host_media_loop: the capture side should be a trait with acquire(timeout)->Result<Option<Frame>,Error>, release(), reinit(), dimensions()->(u32,u32), and an opaque device handle the encoder can share. Frame needs an opaque texture, dirty_rects, move_rects, pointer_only, and cursor{position:{x,y}, visible}. Error must distinguish AccessLost from fatal.
- pipeline.rs:2000 host_media_loop: the encoder should be a trait with new(device_handle, EncoderConfig)->Result<Encoder>, encode(&texture)->Result<Vec<Unit{data:Vec<u8>, keyframe:bool}>>, force_keyframe(), and set_bitrate(u32)->Result. EncoderConfig fields are codec, bitrate_bps, width, height, fps_num, fps_den.
- pipeline.rs:2259 viewer_media_loop: the decoder should be a trait with new(device_handle, codec, w, h)->Result and decode(&[u8], keyframe:bool, ts:i64)->Result<Option<Frame{texture, array_index}>>. The renderer should be a trait with new(device_handle, native_view_handle, w, h) and render_frame(&texture, array_index, Option<(f64,f64)>).
- pipeline.rs:2266-2307 viewer_media_loop: the window handle is the single point of platform coupling. Abstract it as an opaque NativeView with show(), fit() and client_size()->(u32,u32).
- pipeline.rs:1922 deliver_received_file: clipboard, focus and paste injection should sit behind a trait with set_files(&[PathBuf])->Option<..>, focus_window_at(nx,ny)->bool, and a chord injector. The paste chord should be chosen per platform, not hardcoded at the call site.
- pipeline.rs:1957 clipboard_loop: the clipboard should be sequence()->u64, get_text()->Option<String>, set_text(&str)->Option<u64>. The echo-suppression contract depends on set_text returning the new sequence number.
- pipeline.rs:2423 create_render_device: a factory returning one opaque shared device handle, passed to both Decoder and Renderer (the shared-device contract at 2275-2279).

## cross_module_contracts
- Threads: file_send_loop, clipboard_loop, host_media_loop and viewer_media_loop are each separate OS threads. Their spawn sites are outside this range (transport_driver and the start paths) and were not read. The comment at pipeline.rs:4-6 says every COM-bound stage has its own D3D11 device because COM interfaces are not Send. host_media_loop owns capture::Duplicator, codec::Encoder, and the frame_tx and cursor_tx senders. viewer_media_loop owns codec::Decoder, render::Renderer, and the render HWND, which it reads from the RENDER_HWND static.
- Channels into file_send_loop: std::sync::mpsc::Receiver<render::window::FileDrop> (drops, polled with a 200 ms recv_timeout at 1820). mpsc::Sender<Vec<u8>> (out) carries protocol::transfer::BulkFrame encodings: Begin(FileMeta{name,size,drop_at}), Chunk(Vec<u8>) sized by CHUNK_BYTES, Abort(String), End. tokio::sync::mpsc::UnboundedSender<crate::UiEvent> (ui) carries Toast messages. Limits: CHUNK_BYTES and MAX_FILE_BYTES from protocol::transfer.
- Channels into clipboard_loop: mpsc::Sender<Vec<u8>> ctl_tx, carrying serialize(ControlMsg::Clipboard{text}) (serialize is defined at pipeline.rs:1054). mpsc::Receiver<String> inbound carries peer text. Arc<AtomicBool> enabled gates both directions. Arc<AtomicBool> stop. The poll interval is 300 ms (1994). The size cap is crate::clipboard::MAX_TEXT_BYTES (1987).
- Channels into host_media_loop: mpsc::Sender<(Vec<u8>,bool)> frame_tx (AU bytes and keyframe flag). mpsc::Sender<Vec<u8>> cursor_tx, carrying serde_json of ControlMsg::Cursor{x,y,shape:None,visible} (2162-2168). Arc<AtomicU32> bitrate is the BWE target, written by the transport side. Arc<AtomicU32> link is the link capacity estimate in bps, floored at 100_000 (2142). Arc<AtomicBool> force_key is set by the transport on PLI, KeyframeRequest or connect, and cleared here (2204). Arc<AtomicBool> stop. Static read: VIEW_SIZE (2087).
- Channels into viewer_media_loop: mpsc::Receiver<(Vec<u8>,bool)> video_rx, polled with a 100 ms recv_timeout (2324). Disconnected breaks the loop (2416). mpsc::Sender<Vec<u8>> ctl_tx carries ViewSize (2318), KeyframeRequest (2347, 2360, 2407). Arc<AtomicBool> stop. Statics read: RENDER_HWND (2266) and CURSOR (2385).
- Statics read in this range: RENDER_HWND (AtomicIsize, pipeline.rs:25, read at 2266). VIEW_SIZE (AtomicU64, pipeline.rs:45, packed as width<<32|height, 0 meaning unknown, read at 2087). PASTE_DROPPED (AtomicBool, pipeline.rs:40, default true, read at 1931). CURSOR (Mutex<Option<(f64,f64,bool)>>, pipeline.rs:32, read at 2385). CLIPBOARD_SETTING (pipeline.rs:36) is not read in this range. Constants used here but defined elsewhere in the file: MIN_BITRATE=2_500_000 (450), MAX_FPS=30 (458), MAX_FPS_INTERVAL (459-460), MIN_FPS=4 (463), negotiated_codec() (551), serialize() (1054), parse_stun_mapped_address (374).
- Local constant in host_media_loop: MIN_KEYFRAME_GAP = 1000 ms (2057).
- Ownership: the render HWND is created by create_video_window (50-61) on the Tauri thread and is only read by viewer_media_loop. The comment at 56-57 says it persists until the parent is destroyed. The viewer creates its own D3D11 device (create_render_device), separate from the host's capture device.
- Invariant: one active session at a time (SESSION static at pipeline.rs:28, comment at 27). The VIEW_SIZE comment at 44 relies on this for its global.
- Invariant (1807-1809): one file in flight at a time. The bulk channel is ordered and transfers carry no id, so overlapping transfers would interleave.
- Invariant (2044-2051): source pacing uses a token bucket (debt_bytes). Credit is capped at about 1 s (max_debt = link_bps/8/MIN_FPS, 2149). Frame rate gives way before sharpness does.
- Invariant (2174-2192): infinite GOP, keyframes on demand only. MIN_KEYFRAME_GAP = 1 s rate limit. Requests are deferred, never dropped: force_key stays set. Triggers are listed at 2183: connect, RTCP PLI, ctl KeyframeRequest, capture AccessLost.
- Invariant (2294-2296): after a local backlog dump, deltas are useless until an IDR arrives (awaiting_keyframe gate).
- Invariant (2334-2342): the newest queued keyframe is always decodable, so older frames are skipped.
- Invariant (2365-2366): every frame in a batch is decoded, but only the newest decoded frame is presented.

## gotchas
- pipeline.rs:1790-1791 (doc comment): fit_within must keep the source aspect ratio, otherwise the viewer shows bars AND rescales. Output is rounded to even (& !1) for H.264 chroma subsampling and MFT odd-size rejection. Covered by the test at 2461-2468.
- pipeline.rs:1807-1809: the bulk channel has no transfer id, so only one file may be in flight. A port that multiplexes files must add ids.
- pipeline.rs:1837-1838: folders are rejected on purpose because they invite traversal bugs. Keep this on every platform.
- pipeline.rs:1859-1861: drop_at is sent in the BEGIN frame so the host can paste into the window under that point.
- pipeline.rs:1918-1919: focus is set WITHOUT clicking, because a synthetic click could press a button under the drop point. A port must not click to focus.
- pipeline.rs:1934-1936: if drop_at or the injector is missing, the file stays on the clipboard only, and the function returns false. The return value means 'paste attempted'.
- pipeline.rs:1941-1942: a fixed 120 ms sleep stands in for a focus readiness check.
- pipeline.rs:1956 and 1975-1977: echo suppression depends on set_text returning the new change counter. If the counter does not advance on writes, peer text can bounce forever.
- pipeline.rs:1968-1971: inbound text is discarded while enabled is false. The queue is drained, not held, so text received while disabled is lost.
- pipeline.rs:1987-1990: text over MAX_TEXT_BYTES is silently not sent, and last_text is left unchanged.
- pipeline.rs:2010: capture::Duplicator::new(0,0) selects output index 0. Monitor selection is not exposed here.
- pipeline.rs:2002-2007 and 2249-2251: any capture error other than AccessLost (the warn at 2249) breaks the loop, so the host media thread ends with Ok(()). The thread stops, but the failure is only visible in the log.
- pipeline.rs:2020-2021 and 2091: encode dimensions are forced even with a minimum of 2.
- pipeline.rs:2057-2058: the first keyframe clock starts a full gap in the past, so the first request is not deferred.
- pipeline.rs:2094: a rebuild needs both dimensions at least 2 and must not match rejected_size, which prevents a rebuild storm. On rebuild failure, cfg is restored (2116-2117) and the old encoder object is kept.
- pipeline.rs:2142: the link floor is 100_000 bps. The encoder target is floored separately at MIN_BITRATE (2_500_000, 450), so the pacer can reduce fps below the encoder target on a poor link.
- pipeline.rs:2155 and 2243: acquire(16 ms) with Ok(None) means idle and nothing is sent. The 16 ms wait also sets the loop's pace.
- pipeline.rs:2199-2200 and 2241: release() must run on the early continue path (no change and no keyframe) as well as after encode. Otherwise the duplicator stalls.
- pipeline.rs:2208-2237: encode returns a Vec of units, and each unit is sent as its own channel message.
- pipeline.rs:2227: debt_bytes is charged with the bytes actually produced, which can exceed the CBR target because of the QP floor. Frame rate pays for it.
- pipeline.rs:2245-2246: AccessLost means reinit, then force a keyframe.
- pipeline.rs:2266-2270: viewer_media_loop returns Err('no render target set') if RENDER_HWND is 0, which means create_video_window was never called or failed.
- pipeline.rs:2273: the window is shown BEFORE the device, decoder and renderer exist (2277-2279). A failure after show leaves a visible empty surface.
- pipeline.rs:2278-2279: decoder and renderer are both built at a hardcoded 1920x1080, whatever the host's real stream size is.
- pipeline.rs:2304-2323: the size is reported only when it has been stable for 600 ms, is at least 320x240, and differs from the last report. A moving resize sends nothing.
- pipeline.rs:2330-2332: the batch drain is unbounded. This is how catch-up works.
- pipeline.rs:2351-2363: more than 6 queued deltas (about 200 ms at 30 fps) triggers the dump-and-resync path.
- pipeline.rs:2346 and 2402-2403: KeyframeRequest is limited to one per second in both the awaiting branch and the 5-consecutive-undecoded branch.
- pipeline.rs:2369-2371: timestamps advance by 166_667 (100 ns units, about 60 fps) regardless of the true stream rate. The comment says this is for the software AV1 MFT stalling on small pts values.
- pipeline.rs:2385-2388: the cursor sprite is drawn only when visible is true.
- pipeline.rs:2389-2390: render errors are logged and rate-limited (first 10, then every 120th) so a black screen is not silent.
- pipeline.rs:2423-2447: device creation requires a HARDWARE driver and feature level 11_1. There is no WARP fallback in this function, so any machine without FL 11_1 fails hard.
- pipeline.rs:2449-2508: the STUN parser test imports parse_stun_mapped_address from line 374, which is outside this range. That test is pure byte logic and portable, even though it sits in a Windows-focused module.

## tests
- pipeline.rs:2454-2458 fit_within_never_upscales: pure, portable.
- pipeline.rs:2460-2468 fit_within_preserves_aspect_and_evenness: pure, portable. Asserts (1848,1040) for want (1920,1040), src (1920,1080).
- pipeline.rs:2470-2475 fit_within_handles_degenerate_input: pure, portable. Covers the zero-size fallback.
- pipeline.rs:2477-2500 parses_xor_mapped_address_ipv4: STUN XOR-MAPPED-ADDRESS parsing against parse_stun_mapped_address (374). Pure byte logic, portable.
- pipeline.rs:2502-2508 rejects_non_success_response: a STUN request (0x0001) must not yield an address. Pure, portable.
- No tests in this range cover file_send_loop, deliver_received_file, clipboard_loop, host_media_loop, viewer_media_loop or create_render_device. All need OS and hardware. The module doc at pipeline.rs:8-11 says they are validated on target hardware in the section 12 latency smoke test.
