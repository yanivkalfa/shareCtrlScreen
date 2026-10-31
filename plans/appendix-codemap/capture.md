<!-- Generated 2026-10-31 by a parallel code-reading sweep (20 haiku readers, one per subsystem). Line numbers refer to the tree at commit 5a0d195. Facts were spot-checked but not every line was re-verified; treat as a map, not a spec. -->

# capture

## summary
The capture crate (native/crates/capture) is a single-file Windows-only wrapper around DXGI Desktop Duplication (DDA) plus a shared D3D11 device. Its only source file is src/lib.rs (374 lines, read in full); Cargo.toml is 25 lines. Line 15 `#![cfg(windows)]` gates the entire crate, so on macOS/Linux it compiles to an empty library with no public API, and every consumer must be gated too. The crate has no statics, no channels, no spawned threads, no tests and no unsafe beyond the COM calls. Ownership is single-owner: `Duplicator` owns the device, context, duplication interface, a metadata scratch buffer and a `holding_frame` flag. Callers (engine/src/pipeline.rs:2010 and :2244, engine/examples/latency_smoketest.rs:17 and :41) call `Duplicator::new(0,0)`, match `Error::AccessLost` to re-init, and share the device with the encoder through `device()`/`context()`. The pipeline is pull-based: `acquire(timeout)` returns `Ok(None)` on timeout, `Frame` carries the D3D11 texture plus dirty/move rects and cursor data, and `release()` must be called before the next acquire.

## public_api
- lib.rs:37-47 enum Error { Win(windows::core::Error) via #[from], NoOutput, AccessLost, SecureDesktop } (thiserror)
- lib.rs:51-55 struct MoveRect { pub src: POINT, pub dst: RECT } (Windows types leak into public API)
- lib.rs:59-65 struct CursorUpdate { pub visible: bool, pub position: POINT, pub shape: Option<CursorShape> }
- lib.rs:67-71 struct CursorShape { pub info: DXGI_OUTDUPL_POINTER_SHAPE_INFO, pub data: Vec<u8> }
- lib.rs:75-84 struct Frame { pub texture: ID3D11Texture2D, pub dirty_rects: Vec<RECT>, pub move_rects: Vec<MoveRect>, pub accumulated_frames: u32, pub pointer_only: bool, pub cursor: Option<CursorUpdate> }
- lib.rs:87-97 struct Duplicator (all fields private)
- lib.rs:102 Duplicator::new(adapter_index: u32, output_index: u32) -> Result<Self, Error>: creates device, sets multithread protection, duplicates output
- lib.rs:124 Duplicator::dimensions(&self) -> (u32, u32): real output pixel size via IDXGIOutputDuplication::GetDesc
- lib.rs:132 Duplicator::device(&self) -> &ID3D11Device (shared with encoder)
- lib.rs:135 Duplicator::context(&self) -> &ID3D11DeviceContext
- lib.rs:142 Duplicator::acquire(&mut self, timeout: Duration) -> Result<Option<Frame>, Error>
- lib.rs:283 Duplicator::release(&mut self) (idempotent)
- lib.rs:293 Duplicator::reinit(&mut self) -> Result<(), Error> (reuses the existing device)
- lib.rs:300-304 impl Drop for Duplicator (calls release)

## platform_deps
- **DXGI Desktop Duplication: CreateDXGIFactory1 (lib.rs:350), IDXGIFactory1::EnumAdapters1 (351), IDXGIAdapter1::EnumOutputs (356), IDXGIOutput5::DuplicateOutput1 with formats [B8G8R8A8_UNORM] (363-365), fallback IDXGIOutput1::DuplicateOutput (371-372)** @ lib.rs:343-374 (duplicate_output); types imported lib.rs:29-35
  - purpose: Obtain the per-output frame stream: desktop image surfaces, dirty and move rects, pointer position and shape
  - portable: macOS needs an equivalent capture-stream API (ScreenCaptureKit or CGDisplayStream class) returning a GPU-backed image buffer per frame, with changed-region metadata if available, and cursor position and shape from a separate source. Linux needs a PipeWire/xdg-desktop-portal screencast stream (Wayland) or XShm/XDamage (X11). None of these expose DDA's dirty/move/accumulated semantics one-to-one.
- **D3D11CreateDevice with D3D_DRIVER_TYPE_HARDWARE, flags VIDEO_SUPPORT|BGRA_SUPPORT, feature levels 11_1 then 11_0, adapter argument None (default adapter), D3D11_SDK_VERSION (lib.rs:307-329)** @ lib.rs:307-329
  - purpose: One shared device for capture and encode (no cross-device copy)
  - portable: Requires a GPU device/queue that the hardware encoder can read directly. On macOS the equivalent is a Metal device shared with VideoToolbox; on Linux a VAAPI/VA-API display or Vulkan device. The shared-device invariant (§5c) is the important part, not D3D11 itself.
- **ID3D11Multithread::SetMultithreadProtected(TRUE), result discarded (lib.rs:333-338)** @ lib.rs:333-338
  - purpose: Make the shared immediate context safe when capture and encode drive it from different threads
  - portable: Macos/Metal and Linux GPU APIs need an equivalent serialisation rule for the shared queue or context. The crate's comment states the rule (lib.rs:331-332), not the mechanism.
- **DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_ERROR_NOT_FOUND, E_ACCESSDENIED error codes (lib.rs:21, 32, 157-162, 353, 358)** @ lib.rs:156-163, 351-360
  - purpose: Map OS conditions to Error::AccessLost (re-init), Ok(None) (idle), Error::SecureDesktop (UAC/lock screen), and Error::NoOutput (index not found)
  - portable: Each platform must provide the same four classes: no-new-frame timeout, output/session lost (needs re-init), permission or secure-desktop denial, and index not found. macOS reports lost capture via SCStream stop/error callbacks; Linux portal sessions via stream-closed signals. Secure-desktop denial has no direct equivalent on macOS or Linux.
- **IDXGIResource::cast to ID3D11Texture2D (lib.rs:166) and the desktop surface lifetime tied to AcquireNextFrame/ReleaseFrame (lib.rs:152-155, 286)** @ lib.rs:152-166, 283-289
  - purpose: Zero-copy: the desktop texture is handed to the encoder without copying
  - portable: The frame object is valid only between acquire and release, so the port needs a zero-copy buffer handle (IOSurface on macOS, DMA-BUF or PipeWire buffer on Linux) with an explicit return-to-pool/release point.
- **Windows types in the public API: POINT, RECT, DXGI_OUTDUPL_MOVE_RECT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_POSITION, DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_DESC (lib.rs:20-35)** @ lib.rs:20-35, 49-84, 124-127
  - purpose: Carry geometry and cursor data to callers
  - portable: The public structs MoveRect, CursorUpdate, CursorShape and Frame use Windows-specific types. A port should define platform-neutral equivalents (integer rect/point and a cursor-shape enum or pixel buffer) so engine/pipeline.rs does not depend on windows crate types.
- **Windows-only Cargo target dependency: [target.'cfg(windows)'.dependencies.windows] with features Win32_Foundation, Win32_Graphics_Dxgi, Win32_Graphics_Dxgi_Common, Win32_Graphics_Direct3D, Win32_Graphics_Direct3D11, Win32_Graphics_Gdi (Cargo.toml:16-25)** @ Cargo.toml:16-25
  - purpose: Pull in the windows crate only on Windows
  - portable: Win32_Graphics_Gdi is listed but no GDI symbol is referenced in lib.rs (verified by reading the whole file), so it is an unused feature that can be dropped. The rest is Windows-only and would be replaced by per-OS target dependencies.

## portable_parts
- lib.rs:37-47 Error enum (variants are OS-neutral in intent; only the Win(#[from] windows::core::Error) variant is Windows-specific)
- lib.rs:99-117 Duplicator struct construction and its private fields (structure is portable; field types are not)
- lib.rs:142-184 acquire control flow: the holding_frame check, timeout mapping (lib.rs:149 clamps the timeout to u32::MAX ms), pointer_only derivation (lib.rs:168, LastPresentTime == 0), and the order of read_rects then read_cursor
- lib.rs:188-238 read_rects ordering rule (moves before dirty rects, lib.rs:198 and 222), reusing one metadata buffer sized from TotalMetadataBufferSize (lib.rs:195-196)
- lib.rs:242-280 read_cursor logic: LastMouseUpdateTime == 0 means no update (lib.rs:248), shape read only when PointerShapeBufferSize != 0 (lib.rs:254)
- lib.rs:283-289 release idempotence via holding_frame flag
- lib.rs:293-297 reinit as release-then-duplicate on the same device

## abstraction_points
- Trait shape a macOS/Linux backend must satisfy (derived from the Duplicator public methods): `fn new(adapter_index: u32, output_index: u32) -> Result<Self, Error>`; `fn dimensions(&self) -> (u32, u32)`; `fn acquire(&mut self, timeout: Duration) -> Result<Option<Frame>, Error>`; `fn release(&mut self)`; `fn reinit(&mut self) -> Result<(), Error>`; and a device/context accessor used by the encoder (lib.rs:132, 135). Frame and CursorUpdate are the portable data contract that the trait returns.
- Error contract to reproduce on every platform: `AccessLost` (triggers reinit, the only error the engine handles, pipeline.rs:2244 and latency_smoketest.rs:41), `NoOutput`, `SecureDesktop` (lib.rs:45 and :160), and a generic `Win` variant that the portable version would replace with a backend-error variant.
- The texture type inside Frame.texture (lib.rs:76) is the seam where the encoder meets capture. A portable Frame needs an opaque GPU buffer handle (IOSurface, DMA-BUF, or a D3D11 texture on Windows) plus the device handle the encoder is created on.
- Cursor path: the Frame.cursor field (lib.rs:83) and the CursorShape payload (lib.rs:67-71) are the platform-neutral cursor contract. Pointer-only frames (lib.rs:81) are a DDA concept that other backends may emulate with a cursor-only event.

## cross_module_contracts
- Consumers: engine/src/pipeline.rs:2010 `capture::Duplicator::new(0, 0)?` (startup) and pipeline.rs:2244 matches `Err(capture::Error::AccessLost)` to call reinit (the capture thread/loop is in the engine crate, not here). engine/examples/latency_smoketest.rs:17 and :41 do the same, so the example is a second smoke test of the same contract.
- Threading: capture is not thread-safe by itself. `Duplicator` holds COM interfaces with no Send/Sync impls. Nothing in this crate spawns or synchronises threads. Whether the engine moves Duplicator across threads is decided in engine/src/pipeline.rs (not read here). The crate relies on SetMultithreadProtected (lib.rs:333-338) for the shared device, and the code comment at lib.rs:331-332 says it is needed when capture and encode drive the same immediate context from different threads.
- GPU ownership: the D3D11 device and context are owned by Duplicator (lib.rs:88-89) and handed out by reference (lib.rs:132-137). The encoder must not outlive the Duplicator. Frame.texture (lib.rs:76) is a COM clone of the desktop surface, valid only until release() (lib.rs:283-289). Holding it past the next acquire is invalid, and the comment at lib.rs:73-74 says so.
- Re-init contract: reinit (lib.rs:293-297) keeps the existing device and only re-runs duplicate_output. The device is never re-created, so a removed or reset device (DXGI_ERROR_DEVICE_REMOVED) is not handled. It surfaces as Error::Win and the engine would not recover from it through AccessLost.
- No globals, statics, channels or spawned threads exist in lib.rs or Cargo.toml (verified by full read). Shared state is limited to the device handed to the encoder.

## gotchas
- lib.rs:15 `#![cfg(windows)]`: the whole crate is empty off Windows. Any non-Windows port must add a new backend module and keep the public types identical, or consumers break at compile time.
- lib.rs:119-123 (comment): the encoder MUST be sized from Duplicator::dimensions(), not an assumed 1920x1080. A mismatch makes VideoProcessorBlt rescale every frame, which softens the picture, and on non-16:9 monitors also distorts it. This is a hard invariant for any backend.
- lib.rs:143-146: acquire releases any still-held frame before acquiring. Callers must not rely on a Frame surviving the next acquire.
- lib.rs:139-141: Ok(None) on WAIT_TIMEOUT means no change. Callers send nothing (adaptive frame rate). A port must keep 'no update' distinct from 'error', so an idle screen costs no encode work.
- lib.rs:168 and 248: LastPresentTime == 0 means pointer-only (no new pixels), and LastMouseUpdateTime == 0 means no pointer update. These are DDA-specific sentinels. A port needs its own rule for pointer-only or cursor-only events.
- lib.rs:149: the timeout is clamped to u32::MAX milliseconds, so very large Durations silently saturate instead of erroring.
- lib.rs:160: E_ACCESSDENIED from AcquireNextFrame becomes Error::SecureDesktop (UAC or Winlogon desktop). The comment at lib.rs:45 says only the SYSTEM helper (§8) can duplicate that desktop, so this is an OS-policy condition, not a retryable error. macOS and Linux have no direct secure-desktop concept (macOS has screen-recording permission and secure input). The port needs an explicit permission-denied error class instead.
- lib.rs:166 and 165: a missing resource from AcquireNextFrame is reported as Error::NoOutput, and the same variant is used for a missing device (lib.rs:326-327) and a missing output (lib.rs:358). The variant conflates 'no output' with 'null COM result', so a port should not copy this mapping.
- lib.rs:333-338: the SetMultithreadProtected result is discarded with `let _`. A failure is silently ignored, and only the cast error propagates.
- lib.rs:307-329: create_device passes adapter None (default adapter), while duplicate_output uses EnumAdapters1(adapter_index) (lib.rs:351). Both are used with index 0 by the engine, so they match today. A non-zero adapter_index would make DuplicateOutput fail on a device created on a different adapter (DDA requires the device and output on the same adapter). The code does not check this.
- lib.rs:102-117 and 343-374: the DXGI factory and adapter/output enumeration are rebuilt on every reinit (no caching). A port can rebuild its capture session the same way, but this is a per-reinit cost.
- lib.rs:362-369: Output5::DuplicateOutput1 failure is logged with tracing::warn! and then falls back to Output1::DuplicateOutput. A failed cast to IDXGIOutput5 falls back silently (lib.rs:363). Both paths return BGRA-only frames, so the fallback path does not change the pixel format.
- lib.rs:222-230 vs comment at lib.rs:222: the comment says dirty rects go 'into the remainder of the buffer', but the code passes meta_buf.as_mut_ptr() at offset 0 with the full buffer length. This overwrites the move-rect bytes in the buffer, which is harmless only because the moves are already copied into a Vec by collect() at lib.rs:220 before the dirty call. A port must not rely on the buffer layout described in the comment.
- lib.rs:254-266: the cursor shape buffer is freshly allocated (vec!) on every frame where PointerShapeBufferSize != 0, and the `required` out-value (lib.rs:257) is written but never checked. Buffer size is trusted from info.PointerShapeBufferSize.
- lib.rs:246-252: read_cursor caches cursor_visible and last_cursor_pos only when LastMouseUpdateTime != 0. Between updates the stale cached values are not reported (the function returns None at lib.rs:249). The engine therefore sees cursor position only on change events.
- lib.rs:283-289: release() clears holding_frame even if ReleaseFrame fails (the result is discarded with `let _`), so a failed release cannot be retried.
- lib.rs:12-13 and 1-13 (module doc): the WGC fallback (RDP, per-window, headless) is described as a documented secondary backend (§5a Gotchas) but is NOT implemented in this crate. A port cannot assume WGC exists as a reference.
- COM initialisation (CoInitializeEx) is not performed anywhere in this crate. Any caller on a new thread must have initialised COM. The crate does not enforce it.
- Plan references (§5a, §5c, §8, 'Plan 04') point to a design document that is not in the crate, so the invariants above are only as strong as the comments that cite them.

## tests
- No tests exist in the capture crate: no #[cfg(test)] module in src/lib.rs (374 lines read in full), no tests/ directory, and src/ contains only lib.rs.
- Exercised indirectly by two non-test binaries/examples: engine/examples/latency_smoketest.rs (uses Duplicator::new(0,0) and matches AccessLost at :41) and engine/src/pipeline.rs:2010 and :2244. These are Windows-only and need a live desktop. They are the closest thing to a capture test and are not portable as-is.
- Portability of the test surface: nothing in this crate is runnable off Windows (the whole file is cfg(windows)), so a macOS/Linux port needs its own smoke test (acquire N frames, dimensions match the encoder, reinit after a forced access-lost, cursor events) rather than reusing these.
