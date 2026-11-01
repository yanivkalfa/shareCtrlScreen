//! All-zero construction for the C parameter structs of the runtime bindings.
//!
//! Both NVENC and VA-API define "zero" as the default for every field they do
//! not document otherwise (reserved arrays "must be set to 0 / NULL"), so the
//! bindings build every struct from zero and set only what they mean.

/// # Safety
/// Implement only for `#[repr(C)]` types made solely of integers, floats, raw
/// pointers, `Option` of function pointers, and arrays/unions of those — types
/// for which the all-zero bit pattern is a valid value.
pub(crate) unsafe trait Zeroed: Sized {
    fn zeroed() -> Self {
        // SAFETY: guaranteed by the trait's contract.
        unsafe { std::mem::zeroed() }
    }
}
