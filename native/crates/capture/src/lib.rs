//! Screen capture via **DXGI Desktop Duplication** (Plan 04 §5a). The only crate
//! touching DDA/WGC. Produces a shared `ID3D11Texture2D` per frame plus dirty /
//! move rects and out-of-band cursor info, event-driven (idle ⇒ silent).
//!
//! Setup chain (§5a): `CreateDXGIFactory1` → `EnumAdapters1` → `EnumOutputs` →
//! `IDXGIOutput5::DuplicateOutput1` (BGRA always in the supported-formats list),
//! falling back to `IDXGIOutput1::DuplicateOutput` on older Windows. The D3D11
//! device is created **once** with `D3D11_CREATE_DEVICE_VIDEO_SUPPORT` and
//! `ID3D11Multithread::SetMultithreadProtected(TRUE)` and **shared with encode**
//! (§5c). Re-init on `DXGI_ERROR_ACCESS_LOST`.
//!
//! WGC fallback (RDP / per-window / headless) is a documented secondary backend
//! (§5a "Gotchas"); this module implements the DDA primary path.

#![cfg(windows)]

use std::time::Duration;

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Foundation::{E_ACCESSDENIED, POINT, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutput5,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_MOVE_RECT,
    DXGI_OUTDUPL_POINTER_POSITION, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("windows: {0}")]
    Win(#[from] windows::core::Error),
    #[error("no adapter/output at requested index")]
    NoOutput,
    #[error("duplication access lost — caller must re-init")]
    AccessLost,
    #[error("secure desktop — only the SYSTEM helper can duplicate it (§8)")]
    SecureDesktop,
}

/// A move rect (§5a): the OS says a region moved from `src` to `dst`. Process
/// **before** dirty rects (documented ordering).
#[derive(Debug, Clone, Copy)]
pub struct MoveRect {
    pub src: POINT,
    pub dst: RECT,
}

/// Cursor position + optional shape (§5a — sent out-of-band on the reliable
/// channel; the viewer draws a client-side sprite so it feels instant).
#[derive(Debug, Clone)]
pub struct CursorUpdate {
    pub visible: bool,
    pub position: POINT,
    /// Present only when the shape changed this frame (`PointerShapeBufferSize`).
    pub shape: Option<CursorShape>,
}

#[derive(Debug, Clone)]
pub struct CursorShape {
    pub info: DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    pub data: Vec<u8>,
}

/// One acquired frame. Borrows the duplication's current surface; call
/// [`Duplicator::release`] promptly after use (hold at most one frame, §5a).
pub struct Frame {
    pub texture: ID3D11Texture2D,
    pub dirty_rects: Vec<RECT>,
    pub move_rects: Vec<MoveRect>,
    /// >1 means the OS coalesced updates — encode once for the newest (§5a).
    pub accumulated_frames: u32,
    /// `LastPresentTime == 0` ⇒ pointer-only update, no new pixels (§5a).
    pub pointer_only: bool,
    pub cursor: Option<CursorUpdate>,
}

/// DXGI Desktop Duplication for one output.
pub struct Duplicator {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    dupl: IDXGIOutputDuplication,
    output_index: u32,
    adapter_index: u32,
    holding_frame: bool,
    meta_buf: Vec<u8>,
    last_cursor_pos: POINT,
    cursor_visible: bool,
}

impl Duplicator {
    /// Create the shared D3D11 device and duplicate `output_index` on
    /// `adapter_index` (both usually 0 for the primary GPU/monitor).
    pub fn new(adapter_index: u32, output_index: u32) -> Result<Self, Error> {
        let (device, context) = create_device()?;
        set_multithread_protected(&device)?;
        let dupl = duplicate_output(&device, adapter_index, output_index)?;
        Ok(Self {
            device,
            context,
            dupl,
            output_index,
            adapter_index,
            holding_frame: false,
            meta_buf: Vec::new(),
            last_cursor_pos: POINT::default(),
            cursor_visible: false,
        })
    }

    /// The D3D11 device — shared with the encoder (§5c: one device for capture
    /// and encode so there is no cross-device copy).
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }
    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    /// Acquire the next frame. Returns `Ok(None)` on `WAIT_TIMEOUT` (no change —
    /// caller sends nothing, §5a adaptive frame rate). Returns [`Error::AccessLost`]
    /// on `DXGI_ERROR_ACCESS_LOST` so the caller can [`reinit`](Self::reinit).
    pub fn acquire(&mut self, timeout: Duration) -> Result<Option<Frame>, Error> {
        if self.holding_frame {
            // Enforce "hold at most one frame" — release the previous first.
            self.release();
        }
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;

        // SAFETY: valid duplication; out params initialized above.
        let hr = unsafe {
            self.dupl
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
        };
        if let Err(e) = hr {
            return match e.code() {
                c if c == DXGI_ERROR_WAIT_TIMEOUT => Ok(None),
                c if c == DXGI_ERROR_ACCESS_LOST => Err(Error::AccessLost),
                c if c == E_ACCESSDENIED => Err(Error::SecureDesktop),
                _ => Err(Error::Win(e)),
            };
        }
        self.holding_frame = true;
        let resource = resource.ok_or(Error::NoOutput)?;
        let texture: ID3D11Texture2D = resource.cast()?;

        let pointer_only = info.LastPresentTime == 0;
        let (move_rects, dirty_rects) = if pointer_only {
            (Vec::new(), Vec::new())
        } else {
            self.read_rects(&info)?
        };
        let cursor = self.read_cursor(&info)?;

        Ok(Some(Frame {
            texture,
            dirty_rects,
            move_rects,
            accumulated_frames: info.AccumulatedFrames,
            pointer_only,
            cursor,
        }))
    }

    /// Read move + dirty rects (§5a). Size one buffer by `TotalMetadataBufferSize`,
    /// read moves then dirties (documented ordering).
    fn read_rects(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
    ) -> Result<(Vec<MoveRect>, Vec<RECT>), Error> {
        if info.TotalMetadataBufferSize == 0 {
            return Ok((Vec::new(), Vec::new()));
        }
        self.meta_buf
            .resize(info.TotalMetadataBufferSize as usize, 0);

        // Move rects first.
        let mut used: u32 = 0;
        // SAFETY: buffer sized to TotalMetadataBufferSize.
        unsafe {
            self.dupl.GetFrameMoveRects(
                self.meta_buf.len() as u32,
                self.meta_buf.as_mut_ptr() as *mut DXGI_OUTDUPL_MOVE_RECT,
                &mut used,
            )?;
        }
        let move_count = used as usize / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>();
        let moves = unsafe {
            std::slice::from_raw_parts(
                self.meta_buf.as_ptr() as *const DXGI_OUTDUPL_MOVE_RECT,
                move_count,
            )
        }
        .iter()
        .map(|m| MoveRect {
            src: m.SourcePoint,
            dst: m.DestinationRect,
        })
        .collect();

        // Dirty rects into the remainder of the buffer.
        let mut dirty_used: u32 = 0;
        unsafe {
            self.dupl.GetFrameDirtyRects(
                self.meta_buf.len() as u32,
                self.meta_buf.as_mut_ptr() as *mut RECT,
                &mut dirty_used,
            )?;
        }
        let dirty_count = dirty_used as usize / std::mem::size_of::<RECT>();
        let dirties = unsafe {
            std::slice::from_raw_parts(self.meta_buf.as_ptr() as *const RECT, dirty_count)
        }
        .to_vec();

        Ok((moves, dirties))
    }

    /// Read cursor position (cheap, every move) and shape (only when
    /// `PointerShapeBufferSize != 0`) — §5a.
    fn read_cursor(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
    ) -> Result<Option<CursorUpdate>, Error> {
        let pos: DXGI_OUTDUPL_POINTER_POSITION = info.PointerPosition;
        // LastMouseUpdateTime == 0 means no pointer update this frame.
        if info.LastMouseUpdateTime == 0 {
            return Ok(None);
        }
        self.cursor_visible = pos.Visible.as_bool();
        self.last_cursor_pos = pos.Position;

        let shape = if info.PointerShapeBufferSize != 0 {
            let mut buf = vec![0u8; info.PointerShapeBufferSize as usize];
            let mut shape_info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            let mut required: u32 = 0;
            // SAFETY: buf sized to PointerShapeBufferSize.
            unsafe {
                self.dupl.GetFramePointerShape(
                    buf.len() as u32,
                    buf.as_mut_ptr() as *mut _,
                    &mut required,
                    &mut shape_info,
                )?;
            }
            Some(CursorShape {
                info: shape_info,
                data: buf,
            })
        } else {
            None
        };

        Ok(Some(CursorUpdate {
            visible: self.cursor_visible,
            position: self.last_cursor_pos,
            shape,
        }))
    }

    /// Release the currently-held frame promptly (§5a). Safe to call twice.
    pub fn release(&mut self) {
        if self.holding_frame {
            // SAFETY: matched with a successful AcquireNextFrame.
            let _ = unsafe { self.dupl.ReleaseFrame() };
            self.holding_frame = false;
        }
    }

    /// Re-establish duplication after `DXGI_ERROR_ACCESS_LOST` (UAC/lock/mode
    /// change/DWM toggle/TDR) — §5a. Reuses the existing device.
    pub fn reinit(&mut self) -> Result<(), Error> {
        self.release();
        self.dupl = duplicate_output(&self.device, self.adapter_index, self.output_index)?;
        Ok(())
    }
}

impl Drop for Duplicator {
    fn drop(&mut self) {
        self.release();
    }
}

/// Create the shared D3D11 device with VIDEO_SUPPORT + BGRA_SUPPORT (§5a).
fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext), Error> {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    // SAFETY: standard device creation; out params optional.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    Ok((
        device.ok_or(Error::NoOutput)?,
        context.ok_or(Error::NoOutput)?,
    ))
}

/// §5a: shared device must be multithread-protected when capture + encode drive
/// the same immediate context.
fn set_multithread_protected(device: &ID3D11Device) -> Result<(), Error> {
    let mt: ID3D11Multithread = device.cast()?;
    // SAFETY: valid interface.
    let _ = unsafe { mt.SetMultithreadProtected(true) };
    Ok(())
}

/// §5a duplicate chain: factory → adapter → output → Output5::DuplicateOutput1,
/// with a supported-formats list that always includes BGRA. Falls back to
/// `IDXGIOutput1::DuplicateOutput` (BGRA only) on older Windows.
fn duplicate_output(
    device: &ID3D11Device,
    adapter_index: u32,
    output_index: u32,
) -> Result<IDXGIOutputDuplication, Error> {
    // SAFETY: standard COM enumeration.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let adapter: IDXGIAdapter1 = match factory.EnumAdapters1(adapter_index) {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => return Err(Error::NoOutput),
            Err(e) => return Err(Error::Win(e)),
        };
        let output = match adapter.EnumOutputs(output_index) {
            Ok(o) => o,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => return Err(Error::NoOutput),
            Err(e) => return Err(Error::Win(e)),
        };

        // Prefer Output5::DuplicateOutput1 (HDR-capable, format list).
        if let Ok(output5) = output.cast::<IDXGIOutput5>() {
            let formats: [DXGI_FORMAT; 1] = [DXGI_FORMAT_B8G8R8A8_UNORM];
            match output5.DuplicateOutput1(device, 0, &formats) {
                Ok(d) => return Ok(d),
                Err(e) => tracing::warn!("DuplicateOutput1 failed ({e}); falling back"),
            }
        }
        // Fallback: Output1::DuplicateOutput (BGRA only).
        let output1: IDXGIOutput1 = output.cast()?;
        Ok(output1.DuplicateOutput(device)?)
    }
}
