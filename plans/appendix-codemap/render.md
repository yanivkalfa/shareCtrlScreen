<!-- Generated 2026-10-31 by a parallel code-reading sweep (20 haiku readers, one per subsystem). Line numbers refer to the tree at commit 5a0d195. Facts were spot-checked but not every line was re-verified; treat as a map, not a spec. -->

# render

## summary
Unit "render" is the Windows-only D3D11 presentation layer of ShareCtrlScreen. It has two files: lib.rs (577 lines), a Renderer that owns a DXGI flip-model swapchain on a child HWND and draws NV12 frames with an HLSL pixel shader plus a client-side cursor sprite, and window.rs (636 lines), the native child video window (creation, show/hide/fit, a reveal-bar clip, and a wndproc that captures mouse, keyboard and file-drop input and forwards it over mpsc channels). The crate is gated by #![cfg(windows)] at lib.rs:12, and window.rs has no cfg of its own, so on macOS and Linux the crate compiles to an empty library. Its only unconditional dependencies are thiserror and tracing (Cargo.toml:12-14). Nothing in the crate spawns a thread. The render thread and the UI message-pump thread belong to the engine crate, which was not read. Data flow: the decoder supplies an NV12 ID3D11Texture2D plus an array slice on a shared ID3D11Device to Renderer::render_frame (lib.rs:219). The renderer publishes the video size to a global (window.rs:157, called from lib.rs:237), and the UI-thread wndproc reads it to letterbox-normalize input. Input leaves the wndproc as VideoInput through a static Sender (window.rs:57, 319-325). The client size is reported to the host through window::client_size (window.rs:447). Renderer and VideoWindow share no Rust state; they are linked only by the HWND value (lib.rs:125, window.rs:364-371) and the global video-size atomic.

## public_api
- lib.rs:14-15 pub mod window; pub use window::VideoWindow
- lib.rs:41-47 pub enum Error { Win(windows::core::Error) via #[from], Shader(String) } (thiserror; leaks the windows crate error type)
- lib.rs:109 pub struct Renderer (all fields private)
- lib.rs:146 Renderer::new(device: &ID3D11Device, hwnd: HWND, _vw: u32, _vh: u32) -> Result<Self, Error> (_vw and _vh unused)
- lib.rs:219 Renderer::render_frame(&mut self, nv12: &ID3D11Texture2D, array_index: u32, cursor: Option<(f64,f64)>) -> Result<(), Error> (cursor normalized to [0,1]; None draws no sprite)
- lib.rs:345 Renderer::resize(&mut self, width: u32, height: u32) -> Result<(), Error>
- window.rs:33-52 pub enum VideoInput: Move{nx,ny}; Button{button:u8 (0 left, 1 middle, 2 right), down, nx, ny}; Wheel{dx:i32, dy:i32} (Windows units, multiples of 120); Key{scancode:u16 (Set-1), extended:bool, down:bool}
- window.rs:63 pub type FileDrop = (Vec<PathBuf>, f64, f64) (paths plus normalized drop point)
- window.rs:70 pub fn set_file_drop_sink(tx: Sender<FileDrop>) (also calls DragAcceptFiles(true) on DROP_TARGET)
- window.rs:84 pub fn clear_file_drop_sink()
- window.rs:157 pub fn set_video_size(w: u32, h: u32)
- window.rs:303 pub fn set_input_sink(tx: Sender<VideoInput>) (unwraps the lock, unlike the other sink functions)
- window.rs:308 pub fn clear_input_sink() (clears held key and button state WITHOUT emitting releases)
- window.rs:328-331 pub struct VideoWindow { hwnd: HWND, parent: HWND }
- window.rs:337 VideoWindow::create(parent: HWND) -> Result<Self, windows::core::Error> (creates a HIDDEN WS_CHILD window sized to the parent client area)
- window.rs:364 hwnd(&self) -> HWND; window.rs:369 hwnd_raw(&self) -> isize; window.rs:374 parent(&self) -> HWND
- window.rs:386 pub fn show(hwnd_raw: isize) (HWND_TOP plus SW_SHOW; clears reveal state)
- window.rs:415 pub fn fit(hwnd_raw: isize) (MoveWindow to parent client size if different; re-clips if reveal active; no call site in this crate)
- window.rs:447 pub fn client_size(hwnd_raw: isize) -> (u32,u32) (GetClientRect; (0,0) on failure; unaffected by the window region clip)
- window.rs:463 pub fn hide(hwnd_raw: isize) (resets reveal, clears VIDEO_DIMS, clips to empty region, then SW_HIDE)

## platform_deps
- **D3D11 device and immediate context shared with the decoder (device passed in; GetImmediateContext)** @ lib.rs:146-148, 193-196
  - purpose: All GPU work, and zero-copy access to decoder NV12 textures. The renderer clones the device and context but never creates its own device.
  - portable: The port must let the presenter read the decoder output surface without a CPU copy, on the same GPU/queue family, or by import (IOSurface/CVPixelBuffer on macOS, DMA-BUF surface on Linux). The immediate context is single-threaded; render_frame assumes one caller thread (comment lib.rs:253, 'single-threaded present').
- **DXGI adapter to factory chain: IDXGIDevice.GetAdapter, then IDXGIAdapter.GetParent::<IDXGIFactory2>** @ lib.rs:151-153
  - purpose: Gets the factory that created the device, for CreateSwapChainForHwnd on the same adapter.
  - portable: The swapchain must be created on the same GPU/driver as the decoder output. No cross-adapter path exists.
- **DXGI swapchain: CreateSwapChainForHwnd, DXGI_SWAP_EFFECT_FLIP_DISCARD, BufferCount 2, B8G8R8A8_UNORM, SCALING_STRETCH, ALPHA_MODE_IGNORE, RENDER_TARGET_OUTPUT usage, non-stereo, SampleDesc 1/0** @ lib.rs:163-182
  - purpose: Presents to the child window. Flip-discard leaves backbuffer contents undefined after Present, which is why every frame clears (lib.rs:256-261).
  - portable: Needs a double-buffered present target bound to a native child surface that sits above the web view. Contents are undefined after present, so the port must clear or fully overwrite each frame. SCALING_STRETCH is effectively inert because size tracks the client size.
- **IDXGISwapChain2.SetMaximumFrameLatency(1) and GetFrameLatencyWaitableObject (HANDLE)** @ lib.rs:185-186; wait at lib.rs:232
  - purpose: One-frame queue depth. The renderer blocks on the waitable handle before each render (wait-first-then-render), so at most one frame is queued ahead.
  - portable: Needs a present-pacing primitive that blocks the producer until a back buffer is free and caps queued frames at 1. The handle is captured once (lib.rs:186) and not refreshed after ResizeBuffers.
- **WaitForSingleObjectEx(handle, 1000, false)** @ lib.rs:232; import lib.rs:39
  - purpose: Frame pacing gate.
  - portable: Behavioral contract: block up to about 1 s. The return value is discarded (lib.rs:232), so a timeout still renders. Failure is silent.
- **Tearing: IDXGIFactory5.CheckFeatureSupport(DXGI_FEATURE_PRESENT_ALLOW_TEARING); flag ALLOW_TEARING at creation and resize; Present(0, DXGI_PRESENT_ALLOW_TEARING) when supported, else Present(0, 0)** @ lib.rs:155-161, 283-288, 353-356, 435-449
  - purpose: Low-latency uncapped flip when supported; otherwise a sync-interval-0 present.
  - portable: Equivalent contract: an uncapped, non-vsync present mode when available (display-sync off on the layer; IMMEDIATE or MAILBOX-style present on Linux), with a vsync fallback. Sync interval is 0 in both cases.
- **ResizeBuffers(2, w, h, B8G8R8A8_UNORM, flags) after releasing the RTV and calling ClearState** @ lib.rs:345-367 (ClearState at lib.rs:350)
  - purpose: Keep the backbuffer equal to the client size on resize, avoiding DXGI stretch.
  - portable: Contract: every reference to the back buffer must be dropped before resizing. Buffer count stays 2 and format is fixed.
- **Back-buffer RTV: IDXGISwapChain2.GetBuffer(0), then ID3D11Device.CreateRenderTargetView** @ lib.rs:450-462 (called at lib.rs:188 and lib.rs:365)
  - purpose: Render target for the frame draw.
  - portable: Equivalent: fetch the current drawable or back image each frame and make a render target or attachment from it.
- **Runtime HLSL compilation via D3DCompile (FXC), D3DCOMPILE_OPTIMIZATION_LEVEL3, targets vs_5_0 and ps_5_0; four compiles total (vs_main, ps_main, cur_vs, cur_ps) from one source string** @ lib.rs:464-498, 526-561; import lib.rs:19
  - purpose: Compiles the NV12-to-RGB pixel shader and the cursor shaders at startup. Compile errors come back as Error::Shader carrying the compiler log.
  - portable: Replacement needs a shader toolchain that accepts the same math, ported to the target shading language. Only the math is portable (see portable_parts).
- **Plane shader-resource views: Texture2DArray SRV, luma R8_UNORM and chroma R8G8_UNORM, MostDetailedMip 0, MipLevels 1, FirstArraySlice = array_index, ArraySize 1** @ lib.rs:372-397 (called at lib.rs:242-243)
  - purpose: Two views of one NV12 texture, one per plane, sampled in the pixel shader.
  - portable: Requires the decoder texture to be bindable as a shader resource with plane views. macOS equivalent: CVMetalTextureCache planes (R8 and RG8). Linux equivalent: separate plane imports of an NV12 DMA-BUF. The array slice has no equivalent on macOS or Linux decoders, which usually give one texture per frame, so the port would pass array_index 0 or drop it.
- **Sampler: MIN_MAG_MIP_LINEAR, ADDRESS_CLAMP on U, V and W, COMPARISON_NEVER, MaxLOD f32::MAX** @ lib.rs:563-577
  - purpose: Bilinear chroma upsampling (chroma is half size) and clamped edges.
  - portable: Needs a linear, clamp-to-edge sampler. Chroma is upsampled by hardware bilinear filtering at the luma uv; no chroma siting offset is applied.
- **Dynamic constant buffer, 32 bytes, USAGE_DYNAMIC, CONSTANT_BUFFER bind, CPU write; Map(WRITE_DISCARD) and Unmap per cursor draw** @ lib.rs:500-517, 302-330
  - purpose: Passes cursor center and halfsize to the cursor vertex shader.
  - portable: Per-frame uniform. Eight f32 are stored, of which four are used.
- **Viewport from letterbox math (f32); RSSetViewports; the cursor draw reuses the viewport with no new call** @ lib.rs:266-267, 421-433; cursor draw lib.rs:279-281
  - purpose: Centers the video in an aspect-preserving rect. Cursor NDC therefore maps into the letterbox rect, not the whole window.
  - portable: Pure math. Contract: the cursor pass and the video pass share one viewport.
- **Clear to [0,0,0,1] (ClearRenderTargetView), OMSetRenderTargets, PSSetShaderResources(0), PSSetSamplers(0), IASetPrimitiveTopology TRIANGLELIST, Draw(3,0) fullscreen triangle from SV_VertexID with no vertex buffer** @ lib.rs:254-276
  - purpose: Black letterbox bars plus the video quad in one clear and one draw.
  - portable: Equivalent: a vertex-less fullscreen-triangle draw. The clear color is the letterbox bar color.
- **Present(0, flags) with error path: warn log then hr.ok()? returns Err; presented counter increments only on success** @ lib.rs:283-296
  - purpose: Frame submission.
  - portable: A Present failure returns Error::Win to the caller. No DXGI_ERROR_DEVICE_REMOVED or RESET handling exists in this crate, so the caller must recreate.
- **Win32 window API: RegisterClassW (CS_HREDRAW|CS_VREDRAW, no hbrBackground, no hCursor, wndproc), CreateWindowExW (WS_CHILD|WS_CLIPSIBLINGS, not initially WS_VISIBLE), DefWindowProcW, GetModuleHandleW, GetClientRect, GetParent, MoveWindow, SetWindowPos(HWND_TOP, SWP_SHOWWINDOW), ShowWindow(SW_SHOW/SW_HIDE)** @ window.rs:15-26, 336-362, 386-409, 415-442, 463-483, 498-514, 635
  - purpose: Child window that sits above the WebView2 sibling and carries the input and drop surface.
  - portable: Contract: a native view that is a sibling of the web view, z-ordered above it on show, sized to the parent content area, hidden at creation, and able to receive pointer, key and drop events. window.rs:382-385 says the WebView2 child is created after this window and would otherwise sit above it, making the video invisible (airspace). The port must reproduce that z-order, and a web view created later must not cover it.
- **Region clipping: CreateRectRgn plus SetWindowRgn(hwnd, rgn, true) to clip the top strip (apply_reveal_clip) or to clip to empty (clip_to_nothing); DeleteObject only when SetWindowRgn fails, since the window owns the region on success** @ window.rs:233-253, 487-496 (imports window.rs:234, 488)
  - purpose: Hover-reveal of the web session bar: the top strip is removed from hit-test and paint so the web bar shows through. Also used to make hide() immediate.
  - portable: Contract: a visibility and hit-test mask that clips the top strip (bar height in physical px) with no move or resize, so the client size stays constant. Mouse events over the clipped strip must fall through to the web view below.
- **GetDpiForWindow (HiDpi), fallback 96** @ window.rs:209-214
  - purpose: Scales BAR_CSS_PX=40 CSS px to physical px.
  - portable: Contract: the backing scale factor of the window's display. The 96 fallback applies when the API returns 0.
- **Messages decoded in the wndproc: WM_MOUSEMOVE, WM_L/R/MBUTTONDOWN and UP, WM_MOUSEWHEEL, WM_MOUSEHWHEEL, WM_KEYDOWN/UP, WM_SYSKEYDOWN/UP, WM_KILLFOCUS, WM_DROPFILES. lParam low and high words are signed 16-bit client coords; key scancode is lParam bits 16-23 and extended is bit 24; wheel delta is the signed high word of wParam** @ window.rs:519-636
  - purpose: All input capture.
  - portable: VideoInput::Key is a Set-1 PC scancode with an extended flag (window.rs:45-50). macOS (NSEvent virtual keycodes) and Linux (evdev or XKB keycodes) need a translation table to Set-1 scancodes. Wheel contract: one notch = 120. Pointer coords are view-relative, so the port needs the view's coordinate system (y may need flipping).
- **SetFocus(hwnd) on button down. No SetCapture or ReleaseCapture anywhere in the crate. No WM_MOUSELEAVE tracking. No ShowCursor or SetCursor. No hCursor on the class.** @ window.rs:550-553 (SetFocus); 503-510 (class); absent elsewhere
  - purpose: Keyboard focus follows clicks. The local OS cursor is neither hidden nor drawn by this crate.
  - portable: Gap: without capture, a button held and dragged out of the video area never gets its up event, so the button stays in HELD_BUTTONS until WM_KILLFOCUS (window.rs:623-626). The port needs a mouse-capture contract or must accept the gap.
- **Drag and drop: DragAcceptFiles(hwnd, bool) (window.rs:78, 92); WM_DROPFILES handled with DragQueryPoint, DragQueryFileW (count via u32::MAX, then length, then buffer), DragFinish (window.rs:107-147)** @ window.rs:70-95, 105-147, 628-631
  - purpose: Push-to-host file transfer, delivered with a normalized drop point.
  - portable: Contract: receive local file paths plus a drop point in view coordinates, and accept or refuse drops dynamically. Paths are UTF-16 with lossy conversion (window.rs:132-133), so the port needs OS-path handling. DragFinish always runs (window.rs:137) before the empty-path return (window.rs:139-141). DragQueryPoint's return value is ignored (window.rs:117), so the drop point can be stale if it returns false.
- **HWND as process-global state: DROP_TARGET (AtomicU64) is written in VideoWindow::create (window.rs:360) and read by set_file_drop_sink and clear_file_drop_sink. The raw HWND crosses threads as isize in show, hide, fit and client_size (window.rs:386, 415, 447, 463)** @ window.rs:99-103, 360, 369-371, 386, 415, 447, 463
  - purpose: Lets the engine address the video window from other threads without the HWND type.
  - portable: The native view handle may cross threads only as an opaque value. show, hide and fit are documented as safe from the render thread because ShowWindow and MoveWindow marshal to the owning thread (window.rs:380-381). DragAcceptFiles, SetWindowRgn and SetWindowPos (window.rs:233-253, 382-409) run on the caller's thread with no explicit marshaling in this file.
- **tracing crate: info, warn and debug lines** @ lib.rs:290, 295; window.rs:280-284, 402
  - purpose: Diagnostics only.
  - portable: Portable as is.

## portable_parts
- lib.rs:421-433 letterbox(ww, wh, vw, vh) -> viewport rect: pure f32 math, centered, aspect-preserving. Port as plain code returning a rect.
- window.rs:170-189 normalize_client math (f64 letterbox, same formula as lib.rs:421-433). Only the GetClientRect call (window.rs:173) is OS-specific. The formula is duplicated, not shared, so the port should use one function.
- window.rs:154-164 VIDEO_DIMS packing (w<<32 | h) and unpacking: pure, over an atomic.
- window.rs:33-52 VideoInput and window.rs:63 FileDrop: pure data types. The Set-1 scancode contract is noted above.
- window.rs:267-300 release_all_held: pure bookkeeping that emits synthetic releases from HELD_KEYS, HELD_BUTTONS and LAST_POS. Portable once emit is supplied.
- window.rs:260-263 held-state bookkeeping (mutex-protected Vecs and a position tuple): pure data.
- lib.rs:51-105 SHADER_HLSL: the NV12-to-RGB BT.709 limited-range math (lib.rs:66-75: y=(y-16/255)*(255/219); r=y+1.5748*v; g=y-0.1873*u-0.4681*v; b=y+1.8556*u; saturate) and the cursor arrow shape (lib.rs:99-104) are portable as math. Syntax is HLSL-specific.
- lib.rs:401-417 client_size and texture_size helpers: thin wrappers with a minimum of 1 px (lib.rs:406-416).
- window.rs:209-214 bar_phys: pure scaling BAR_CSS_PX*dpi/96 once DPI is supplied.
- window.rs:202-206 reveal constants (BAR_CSS_PX=40, UNREVEAL_DEPTH_PX=56) and the reveal decision in WM_MOUSEMOVE (window.rs:533-542: reveal at y<=2, unreveal at y>56): portable logic.

## abstraction_points
- Renderer::new, render_frame and resize (lib.rs:146, 219, 345) as one trait, for example a Presenter with new(device-context handle, native view handle), present_nv12(frame handle, plane slice, cursor option) and resize(w, h). The D3D11 types appear in these public signatures, so the trait has to hide them.
- VideoWindow plus show, hide, fit, client_size and set_reveal (window.rs:328-483) as a trait over a native overlay surface: create(parent) -> handle, show, hide, fit, client_size -> (u32,u32), set_top_strip_clip(handle, px). The HWND is the only handle that crosses threads (window.rs:369-371).
- Input emit path (window.rs:57, 303-325): a sink trait or channel carrying VideoInput. Sender<VideoInput> and Sender<FileDrop> are the cross-thread contract.
- Key translation (window.rs:600-619, Win32 scancode and extended-bit decoding) should sit behind a keycode-to-Set-1 table, so each OS supplies its own decoder while VideoInput::Key stays fixed.
- Drop registration (window.rs:70-95, 360) behind a trait, so macOS and Linux drag-and-drop plug in without touching the engine.
- Shader source (lib.rs:51-105) behind a per-backend shader table (HLSL for D3D11; MSL, GLSL or SPIR-V elsewhere) that shares the matrix constants.
- Frame source: render_frame takes ID3D11Texture2D plus array index (lib.rs:219). A port needs an abstract frame handle exposing per-plane views (luma R8 and chroma RG8 equivalents) and an optional slice index.

## cross_module_contracts
- Renderer is created on the render thread with the decoder's ID3D11Device (lib.rs:146) and must be used only from that thread (single immediate context, lib.rs:253). render_frame and resize do not overlap: resize runs inline from sync_size (lib.rs:228, 337-342).
- Each render_frame publishes the decoded frame's pixel size via window::set_video_size (window.rs:157, called at lib.rs:237) before Present. The UI-thread wndproc reads it through normalize_client (window.rs:170-189) to map pointer coordinates. hide() resets it to 0 (window.rs:466), and normalization then falls back to the full window. The mapping can lag by at most one frame.
- The child HWND is owned by the UI thread (comments window.rs:7-9 and window.rs:333-334). Renderer keeps its own HWND copy (lib.rs:125, 207) and calls GetClientRect from the render thread (lib.rs:401-409). show, hide, fit and client_size (window.rs:386-483) take a raw isize and may be called from the render thread.
- The client size reported to the host is the child's client rect (window.rs:444-460; doc window.rs:444-446). The region clip does not change the client size (window.rs:195-200), so the host encode size stays stable during a bar reveal.
- Input events are sent from the UI-thread wndproc with Sender<VideoInput>.send (window.rs:319-325). The channel is unbounded std mpsc and send errors are ignored. Emission is gated on INPUT_SINK being Some (window.rs:319-325; comment window.rs:516-518).
- File drops are sent as Sender<FileDrop> (window.rs:144) with the drop point normalized by the same letterbox mapping (window.rs:116-119).
- Engine calls (not in this crate, not read): set_input_sink at session start with control and clear_input_sink at end (window.rs:303-317); set_file_drop_sink and clear_file_drop_sink (window.rs:70-95); show at session start and hide on return to home or settings (window.rs:386-409, 463-483); fit, which the doc says is called periodically from the render loop (window.rs:411-414), with no call site in this crate.
- Process-wide statics shared by every VideoWindow instance: INPUT_SINK, FILE_DROP_SINK, DROP_TARGET, VIDEO_DIMS, REVEAL_OFFSET, HELD_KEYS, HELD_BUTTONS, LAST_POS and REGISTER (see gotchas).
- Renderer.srv_cache holds strong references (an SRV keeps its texture alive) to decoder textures, up to 64 entries (lib.rs:134-137, 246-249). This pins decoder pool textures until the cache is cleared or the renderer is dropped.

## gotchas
- The crate compiles only on Windows (lib.rs:12). window.rs has no cfg attribute but is reachable only through the gated lib.rs. Non-Windows consumers that name render::Renderer or render::window items will fail to compile unless they also gate. Consumers were not inspected.
- LATENT BUG: resize() sets width and height (lib.rs:346-347) and clears rtv (lib.rs:349) BEFORE ResizeBuffers (lib.rs:357-363). If ResizeBuffers or create_rtv (lib.rs:365) fails, the error is returned with rtv None and the new size stored. sync_size (lib.rs:337-342) swallows the error and on the next frame sees w equal to self.width, so it never retries. Every later render_frame then runs with rtv None: the clear is skipped (lib.rs:258) and OMSetRenderTargets binds nothing, so the video is never drawn until the client size changes again.
- The WaitForSingleObjectEx return value is discarded (lib.rs:232), so after a 1000 ms timeout the renderer presents anyway. The waitable handle is fetched once in new() (lib.rs:186) and not refreshed after ResizeBuffers.
- Frame order: sync_size (and therefore resize) runs before the wait (lib.rs:228, then 232), and the video size is published after the wait (lib.rs:236-237). A resize can therefore happen while a frame is still queued.
- Flip-discard: backbuffer contents are undefined after Present, so each frame clears to black (lib.rs:256-261) and then draws the full letterbox. The cursor is drawn after the video in the same frame (lib.rs:279-281).
- Letterbox and input math must match. lib.rs:421-433 (f32) and window.rs:181-188 (f64) implement the same formula separately; the comment at window.rs:152-153 says they must match. The shader maps the full texture into the letterbox viewport (lib.rs:264-266), so letterbox placement is the only scaling step.
- Cursor sprite size is in window pixels scaled by the letterbox ratio, not a fixed 18x26 px: hx = 18/width and hy = 26/height (lib.rs:307-308), with NDC drawn through the letterbox viewport (set at lib.rs:266-267 and not reset for the cursor). The sprite is a client-side triangle, not the remote OS cursor shape. The local OS cursor is not hidden, since the crate has no ShowCursor or SetCursor, so both cursors are visible.
- Chroma is sampled bilinearly at the luma uv (lib.rs:66-75, 242-243, 563-577). No chroma siting offset is applied, which matters for fidelity when porting.
- Output is B8G8R8A8_UNORM with no sRGB conversion (lib.rs:166, 74). The shader writes gamma-encoded BT.709 values directly.
- Device loss (TDR or driver reset) is not detected or recovered. Present errors only propagate (lib.rs:288-292), and no recreate path exists in this crate.
- The SRV cache is keyed by raw texture pointer and array slice (lib.rs:240). Because the cache holds strong refs, the pointer cannot be reused while an entry lives. The cache is cleared when it exceeds 64 entries (lib.rs:246-248), which drops refs to decoder textures.
- release_all_held runs only on WM_KILLFOCUS (window.rs:623-626). clear_input_sink (window.rs:308-317) clears HELD_KEYS and HELD_BUTTONS without emitting releases, even though the comment at window.rs:265-266 says 'focus loss or session end'. A session that ends with keys held therefore sends no releases to the host, and the comment and code disagree.
- UNREVEAL_DEPTH_PX = 56 (window.rs:206) is documented as deeper than the bar (window.rs:231-232), but the bar is BAR_CSS_PX*dpi/96 physical px (window.rs:209-214). At 150% DPI the bar is 60 px, which is above the 56 px threshold, so the unreveal condition holds as soon as the pointer enters the video area. The behavior depends on DPI.
- Mouse and key messages return 0 without calling DefWindowProc (window.rs:548-619), including WM_SYSKEYDOWN and WM_SYSKEYUP. Alt+F4, F10 and Alt-menu are therefore swallowed while a session with control is active. Key auto-repeat is not filtered (repeat bit 30 is not read, window.rs:600-606); the held-set dedupes it.
- No SetCapture. A press, drag and release that leaves the window loses the up event, and the button stays in HELD_BUTTONS until focus is lost (window.rs:559-563, 579-582).
- hide() (window.rs:463-483) clips to nothing before ShowWindow(SW_HIDE). The comment at window.rs:470-476 explains that a deferred ShowWindow on the UI thread left a bar-height clip that sliced the Settings dialog. show() (window.rs:386-409) reapplies apply_reveal_clip(hwnd, 0) to clear any leftover clip.
- Client-area size is invariant under the reveal clip (window.rs:195-200, 444-460). A port must keep the client rect stable during reveal, or the host encode size will change.
- Hover reveal uses raw physical y coordinates (window.rs:535-541): reveal at y <= 2 px and unreveal at y > 56 px. These thresholds are not DPI-scaled.
- VideoWindow::create stores the HWND in the single global DROP_TARGET (window.rs:360). Creating a second VideoWindow silently redirects drop enablement to it. clear_file_drop_sink (window.rs:84-95) reads the same global.
- The window class has no background brush (window.rs:508) and no cursor (window.rs:503-510). The swapchain paints the whole client area, so flicker is not expected, but the cursor shape over the video is whatever Windows selects by default.
- WM_SIZE is not handled (window.rs:529-634). The comment at window.rs:7-9 says the parent loop services it. Child size is driven by fit() (window.rs:415-442), polled from the render loop, and by show() (window.rs:386-409), not by a size-change message.
- The _vw and _vh parameters of Renderer::new (lib.rs:146) are unused. The real video size comes from the texture (lib.rs:236).
- Tearing unsupported: the swapchain is still created with FRAME_LATENCY_WAITABLE_OBJECT (lib.rs:158-161), and the ALLOW_TEARING flag is simply omitted; Present falls back to flags 0 (lib.rs:283-287).
- compile() calls CString::new(...).unwrap() on static strings (lib.rs:527-528), which is safe. Compile errors fall back to the HRESULT string when the error blob is absent (lib.rs:547-558).
- The shader source is compiled four times (lib.rs:467-468, 485-486) into separate shader objects from the same text. Only the vertex and pixel pair actually used is bound per pass.
- set_input_sink uses .lock().unwrap() (window.rs:304), while emit and clear_input_sink use if-let-Ok (window.rs:320, 311). Poisoning behavior is inconsistent across the sink functions.
- draw_cursor does not clamp its normalized inputs (lib.rs:302-304). Only normalize_client clamps (window.rs:178-187). A NaN or out-of-range value reaches the NDC math unchecked.

## tests
- None. Neither lib.rs nor window.rs contains #[test] or #[cfg(test)] modules (both read in full). render/Cargo.toml (lines 1-32) declares no dev-dependencies and no test features.
- Pure functions that are good unit-test candidates once extracted: letterbox() (lib.rs:421-433), the math of normalize_client (window.rs:170-189), bar_phys (window.rs:209-214), VIDEO_DIMS pack and unpack (window.rs:154-164), and the release_all_held bookkeeping (window.rs:267-300).
- HLSL is checked only at runtime through D3DCompile at startup (lib.rs:532-546). The crate has no offline shader check.
