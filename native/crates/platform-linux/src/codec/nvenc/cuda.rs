//! The handful of CUDA driver-API calls NVENC's CUDA device needs, loaded from
//! `libcuda.so.1` (part of the NVIDIA driver; no CUDA toolkit involved).
//!
//! NVENC on Linux takes a CUDA context as its "device" and CUDA device memory
//! as input. v1 copies each host frame into one pitched device allocation with
//! `cuMemcpy2D`; the seam for zero-copy (a DMA-BUF imported with
//! `cuImportExternalMemory`) is the same [`DeviceFrame`] slot.
//!
//! Signatures follow `dynlink_cuda.h` from nv-codec-headers n12.0.16.1 (the
//! `_v2` entry points are the ones with 64-bit `CUdeviceptr`/`size_t`).

use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::sync::Arc;

use libloading::Library;

pub(crate) type CuResult = c_int;
pub(crate) type CuDevice = c_int;
pub(crate) type CuContext = *mut c_void;
pub(crate) type CuDevicePtr = u64;

const CUDA_SUCCESS: CuResult = 0;
const CU_MEMORYTYPE_HOST: c_uint = 1;
const CU_MEMORYTYPE_DEVICE: c_uint = 2;
/// `CU_CTX_SCHED_BLOCKING_SYNC`: block the CPU thread instead of spinning while
/// waiting on the GPU — the encode thread waits on every frame.
const CU_CTX_SCHED_BLOCKING_SYNC: c_uint = 0x4;

#[repr(C)]
struct CudaMemcpy2d {
    src_x_in_bytes: usize,
    src_y: usize,
    src_memory_type: c_uint,
    src_host: *const c_void,
    src_device: CuDevicePtr,
    src_array: *mut c_void,
    src_pitch: usize,
    dst_x_in_bytes: usize,
    dst_y: usize,
    dst_memory_type: c_uint,
    dst_host: *mut c_void,
    dst_device: CuDevicePtr,
    dst_array: *mut c_void,
    dst_pitch: usize,
    width_in_bytes: usize,
    height: usize,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<CudaMemcpy2d>() == 128);
    assert!(std::mem::offset_of!(CudaMemcpy2d, src_host) == 24);
    assert!(std::mem::offset_of!(CudaMemcpy2d, dst_device) == 88);
    assert!(std::mem::offset_of!(CudaMemcpy2d, width_in_bytes) == 112);
};

/// The loaded entry points. Lives as long as any context made from it.
pub(crate) struct Cuda {
    _lib: Library,
    init: unsafe extern "C" fn(c_uint) -> CuResult,
    device_get: unsafe extern "C" fn(*mut CuDevice, c_int) -> CuResult,
    device_get_name: unsafe extern "C" fn(*mut c_char, c_int, CuDevice) -> CuResult,
    ctx_create: unsafe extern "C" fn(*mut CuContext, c_uint, CuDevice) -> CuResult,
    ctx_destroy: unsafe extern "C" fn(CuContext) -> CuResult,
    ctx_push: unsafe extern "C" fn(CuContext) -> CuResult,
    ctx_pop: unsafe extern "C" fn(*mut CuContext) -> CuResult,
    mem_alloc_pitch:
        unsafe extern "C" fn(*mut CuDevicePtr, *mut usize, usize, usize, c_uint) -> CuResult,
    mem_free: unsafe extern "C" fn(CuDevicePtr) -> CuResult,
    memcpy_2d: unsafe extern "C" fn(*const CudaMemcpy2d) -> CuResult,
    get_error_name: unsafe extern "C" fn(CuResult, *mut *const c_char) -> CuResult,
}

// SAFETY: the CUDA driver API is thread-safe; the table holds only immutable
// function pointers and the library handle.
unsafe impl Send for Cuda {}
unsafe impl Sync for Cuda {}

impl Cuda {
    /// `dlopen("libcuda.so.1")` + `cuInit`. Fails with a readable reason when the
    /// NVIDIA driver (or a usable GPU) is absent.
    pub(crate) fn load() -> Result<Arc<Self>, String> {
        // SAFETY: loading the driver's CUDA library runs only its initialisers.
        let lib = unsafe { Library::new("libcuda.so.1") }
            .map_err(|e| format!("libcuda.so.1 not loadable ({e}) — no NVIDIA driver"))?;
        macro_rules! sym {
            ($name:literal) => {
                // SAFETY: the symbol has the signature of the field it is stored
                // in (dynlink_cuda.h); the library outlives the copy (`_lib`).
                *unsafe { lib.get($name) }.map_err(|e| {
                    format!(
                        "libcuda.so.1 lacks {} ({e})",
                        String::from_utf8_lossy(&$name[..$name.len() - 1])
                    )
                })?
            };
        }
        let cuda = Self {
            init: sym!(b"cuInit\0"),
            device_get: sym!(b"cuDeviceGet\0"),
            device_get_name: sym!(b"cuDeviceGetName\0"),
            ctx_create: sym!(b"cuCtxCreate_v2\0"),
            ctx_destroy: sym!(b"cuCtxDestroy_v2\0"),
            ctx_push: sym!(b"cuCtxPushCurrent_v2\0"),
            ctx_pop: sym!(b"cuCtxPopCurrent_v2\0"),
            mem_alloc_pitch: sym!(b"cuMemAllocPitch_v2\0"),
            mem_free: sym!(b"cuMemFree_v2\0"),
            memcpy_2d: sym!(b"cuMemcpy2D_v2\0"),
            get_error_name: sym!(b"cuGetErrorName\0"),
            _lib: lib,
        };
        // SAFETY: plain FFI call; flags must be 0.
        cuda.check(unsafe { (cuda.init)(0) }, "cuInit")?;
        Ok(Arc::new(cuda))
    }

    fn check(&self, r: CuResult, what: &str) -> Result<(), String> {
        if r == CUDA_SUCCESS {
            return Ok(());
        }
        let mut name: *const c_char = std::ptr::null();
        // SAFETY: cuGetErrorName writes a pointer to a static string (or fails
        // and leaves `name` null).
        let named = unsafe { (self.get_error_name)(r, &mut name) } == CUDA_SUCCESS;
        let name = if named && !name.is_null() {
            // SAFETY: non-null, NUL-terminated, static.
            unsafe { CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned()
        } else {
            format!("CUresult {r}")
        };
        Err(format!("{what} failed: {name}"))
    }
}

/// A CUDA context on device `ordinal`, destroyed on drop.
pub(crate) struct Context {
    cuda: Arc<Cuda>,
    ctx: CuContext,
    name: String,
}

impl Context {
    pub(crate) fn new(cuda: &Arc<Cuda>, ordinal: c_int) -> Result<Self, String> {
        let mut dev: CuDevice = 0;
        // SAFETY: out-pointer to a local.
        cuda.check(
            unsafe { (cuda.device_get)(&mut dev, ordinal) },
            "cuDeviceGet",
        )?;
        let mut buf = [0 as c_char; 128];
        // SAFETY: buffer and its length; the driver NUL-terminates.
        let name = if unsafe { (cuda.device_get_name)(buf.as_mut_ptr(), buf.len() as c_int, dev) }
            == CUDA_SUCCESS
        {
            // SAFETY: NUL-terminated within `buf` (zero-initialised).
            unsafe { CStr::from_ptr(buf.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        } else {
            format!("CUDA device {ordinal}")
        };
        let mut ctx: CuContext = std::ptr::null_mut();
        // SAFETY: out-pointer to a local. cuCtxCreate also makes the context
        // current on this thread; it is popped right away so every later use
        // goes through `Current` and the calling thread does not matter.
        cuda.check(
            unsafe { (cuda.ctx_create)(&mut ctx, CU_CTX_SCHED_BLOCKING_SYNC, dev) },
            "cuCtxCreate",
        )?;
        let mut popped: CuContext = std::ptr::null_mut();
        // SAFETY: the context created above is current on this thread.
        unsafe { (cuda.ctx_pop)(&mut popped) };
        Ok(Self {
            cuda: cuda.clone(),
            ctx,
            name,
        })
    }

    /// Raw handle for `NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS::device`.
    pub(crate) fn raw(&self) -> CuContext {
        self.ctx
    }

    pub(crate) fn device_name(&self) -> &str {
        &self.name
    }

    /// Make the context current for the guard's lifetime. NVENC calls on a
    /// CUDA session need it current too, so the encoder holds one per call.
    pub(crate) fn current(&self) -> Result<Current<'_>, String> {
        // SAFETY: valid context; popped again by `Current::drop`.
        self.cuda.check(
            unsafe { (self.cuda.ctx_push)(self.ctx) },
            "cuCtxPushCurrent",
        )?;
        Ok(Current { ctx: self })
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: we own the context; nothing created from it outlives us
        // (DeviceFrame and the NVENC session borrow or are dropped first).
        unsafe { (self.cuda.ctx_destroy)(self.ctx) };
    }
}

/// The context is current on this thread while this lives.
pub(crate) struct Current<'a> {
    ctx: &'a Context,
}

impl Drop for Current<'_> {
    fn drop(&mut self) {
        let mut popped: CuContext = std::ptr::null_mut();
        // SAFETY: balanced with the push in `Context::current`.
        unsafe { (self.ctx.cuda.ctx_pop)(&mut popped) };
    }
}

/// One pitched device allocation holding a whole picture: ARGB (`rows` = height)
/// or NV12 (`rows` = height × 3/2, chroma at `pitch × height`, which is where
/// NVENC looks for it in a CUDA device-pointer NV12 surface).
pub(crate) struct DeviceFrame {
    cuda: Arc<Cuda>,
    ctx: CuContext,
    pub(crate) ptr: CuDevicePtr,
    pub(crate) pitch: usize,
    rows: usize,
}

impl DeviceFrame {
    pub(crate) fn alloc(
        current: &Current<'_>,
        width_bytes: usize,
        rows: usize,
    ) -> Result<Self, String> {
        let cuda = &current.ctx.cuda;
        let mut ptr: CuDevicePtr = 0;
        let mut pitch = 0usize;
        // SAFETY: out-pointers to locals; the context is current (`current`).
        // Element size 16 = the widest access the driver should optimise for.
        cuda.check(
            unsafe { (cuda.mem_alloc_pitch)(&mut ptr, &mut pitch, width_bytes, rows, 16) },
            "cuMemAllocPitch",
        )?;
        Ok(Self {
            cuda: cuda.clone(),
            ctx: current.ctx.ctx,
            ptr,
            pitch,
            rows,
        })
    }

    /// Copy `rows` rows of `width_bytes` from host memory (`src_pitch` apart)
    /// to device rows starting at `dst_row`.
    pub(crate) fn upload(
        &self,
        _current: &Current<'_>,
        src: &[u8],
        src_pitch: usize,
        width_bytes: usize,
        rows: usize,
        dst_row: usize,
    ) -> Result<(), String> {
        if rows == 0 {
            return Ok(());
        }
        let needed = src_pitch * (rows - 1) + width_bytes;
        if src.len() < needed || width_bytes > self.pitch.min(src_pitch) {
            return Err(format!(
                "host plane too small: {} bytes for {rows} rows of {width_bytes} (pitch {src_pitch})",
                src.len()
            ));
        }
        if dst_row + rows > self.rows {
            return Err(format!(
                "upload of rows {dst_row}..{} overruns a {}-row device frame",
                dst_row + rows,
                self.rows
            ));
        }
        let copy = CudaMemcpy2d {
            src_x_in_bytes: 0,
            src_y: 0,
            src_memory_type: CU_MEMORYTYPE_HOST,
            src_host: src.as_ptr().cast(),
            src_device: 0,
            src_array: std::ptr::null_mut(),
            src_pitch,
            dst_x_in_bytes: 0,
            dst_y: dst_row,
            dst_memory_type: CU_MEMORYTYPE_DEVICE,
            dst_host: std::ptr::null_mut(),
            dst_device: self.ptr,
            dst_array: std::ptr::null_mut(),
            dst_pitch: self.pitch,
            width_in_bytes: width_bytes,
            height: rows,
        };
        // SAFETY: both spans were bounds-checked above (source slice, and our
        // own `rows × pitch` allocation); the context is current.
        self.cuda
            .check(unsafe { (self.cuda.memcpy_2d)(&copy) }, "cuMemcpy2D")
    }
}

impl Drop for DeviceFrame {
    fn drop(&mut self) {
        // SAFETY: we own the allocation; its context outlives it (the encoder
        // drops frames before the `Context`). Push/pop keeps the free correct
        // whatever is current on the dropping thread.
        unsafe {
            if (self.cuda.ctx_push)(self.ctx) == CUDA_SUCCESS {
                (self.cuda.mem_free)(self.ptr);
                let mut popped: CuContext = std::ptr::null_mut();
                (self.cuda.ctx_pop)(&mut popped);
            }
        }
    }
}
