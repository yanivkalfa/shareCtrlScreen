//! Minimal `VARIANT` constructors for `ICodecAPI::SetValue` (Plan 04 §5b).
//! windows-rs exposes no `From`/`new` for `VARIANT`, so we build the union
//! by hand for the three types the low-latency recipe needs: `VT_UI4` (u32),
//! `VT_UI8` (u64) and `VT_BOOL`.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4, VT_UI8,
};

fn wrap(vt: VARENUM, fill: impl FnOnce(&mut VARIANT_0_0_0)) -> VARIANT {
    let mut inner = VARIANT_0_0 {
        vt,
        wReserved1: 0,
        wReserved2: 0,
        wReserved3: 0,
        Anonymous: VARIANT_0_0_0::default(),
    };
    fill(&mut inner.Anonymous);
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(inner),
        },
    }
}

/// `VT_UI4` unsigned 32-bit.
pub fn u32v(v: u32) -> VARIANT {
    wrap(VT_UI4, |u| u.ulVal = v)
}

/// `VT_UI8` unsigned 64-bit. Part of the complete helper set (used by callers
/// that pass 64-bit CODECAPI values, e.g. LTR buffer control).
#[allow(dead_code)]
pub fn u64v(v: u64) -> VARIANT {
    wrap(VT_UI8, |u| u.ullVal = v)
}

/// `VT_BOOL` (VARIANT_TRUE is all-ones, i.e. -1).
pub fn boolv(v: bool) -> VARIANT {
    wrap(VT_BOOL, |u| {
        u.boolVal = VARIANT_BOOL(if v { -1 } else { 0 });
    })
}
