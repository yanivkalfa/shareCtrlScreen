//! Display enumeration for the "share display" setting, and the geometry the
//! capture needs for one display.
//!
//! macOS measures the desktop in **points** (CoreGraphics global space: origin
//! at the top-left of the main display, y down). `DisplayInfo` bounds are in
//! that space because it is what `CGEventPost` (input injection) and
//! `CGDisplayBounds` use; `scale` converts to the pixels the capture delivers
//! (plans/01-MACOS-FINDINGS.md §8 "Retina").

use objc2_core_foundation::CGRect;
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayIsBuiltin,
    CGDisplayMode, CGDisplayModelNumber, CGDisplaySerialNumber, CGDisplayUnitNumber,
    CGDisplayVendorNumber, CGError, CGGetActiveDisplayList, CGMainDisplayID,
};
use platform_api::DisplayInfo;

use crate::convert::RectF;
use crate::display_id::{assign_ids, DisplayIdentity};

/// More than any Mac can drive at once (Apple Silicon tops out at 8), so the
/// list is never truncated.
const MAX_DISPLAYS: usize = 32;

pub(crate) fn rect_f(r: CGRect) -> RectF {
    RectF {
        x: r.origin.x,
        y: r.origin.y,
        w: r.size.width,
        h: r.size.height,
    }
}

/// Global bounds of `display` in points.
pub(crate) fn bounds(display: CGDirectDisplayID) -> RectF {
    rect_f(CGDisplayBounds(display))
}

/// The display's current mode size in **pixels** (the backing size: 2880×1800
/// for a Retina panel that "looks like" 1440×900). `None` if the display went
/// away between enumeration and this call.
pub(crate) fn native_pixels(display: CGDirectDisplayID) -> Option<(u32, u32)> {
    let mode = CGDisplayCopyDisplayMode(display)?;
    let w = CGDisplayMode::pixel_width(Some(&mode));
    let h = CGDisplayMode::pixel_height(Some(&mode));
    (w > 0 && h > 0).then(|| {
        (
            u32::try_from(w).unwrap_or(u32::MAX),
            u32::try_from(h).unwrap_or(u32::MAX),
        )
    })
}

/// Active displays (drawable, not mirrored-away or asleep), in unit-number
/// order so the duplicate-identity suffixes of [`assign_ids`] are stable.
fn active_displays() -> Vec<CGDirectDisplayID> {
    let mut ids = [0 as CGDirectDisplayID; MAX_DISPLAYS];
    let mut count = 0u32;
    // SAFETY: both pointers are valid for the sizes passed.
    let err = unsafe { CGGetActiveDisplayList(MAX_DISPLAYS as u32, ids.as_mut_ptr(), &mut count) };
    if err != CGError::Success {
        tracing::warn!("CGGetActiveDisplayList failed ({})", err.0);
        return Vec::new();
    }
    let mut out = ids[..(count as usize).min(MAX_DISPLAYS)].to_vec();
    out.sort_by_key(|&d| CGDisplayUnitNumber(d));
    out
}

/// Every shareable display, with its stable id, paired with its current
/// `CGDirectDisplayID`.
pub(crate) fn enumerate_with_ids() -> Vec<(CGDirectDisplayID, DisplayInfo)> {
    let displays = active_displays();
    let identities: Vec<DisplayIdentity> = displays
        .iter()
        .map(|&d| DisplayIdentity {
            vendor: CGDisplayVendorNumber(d),
            model: CGDisplayModelNumber(d),
            serial: CGDisplaySerialNumber(d),
        })
        .collect();
    let main = CGMainDisplayID();
    let mut external = 0;
    displays
        .iter()
        .zip(assign_ids(&identities))
        .map(|(&d, id)| {
            let b = bounds(d);
            let scale = match native_pixels(d) {
                Some((pw, _)) if b.w > 0.0 => pw as f64 / b.w,
                _ => 1.0,
            };
            let name = if CGDisplayIsBuiltin(d) {
                "Built-in Display".to_string()
            } else {
                external += 1;
                format!("Display {external}")
            };
            let info = DisplayInfo {
                id,
                name,
                x: b.x.round() as i32,
                y: b.y.round() as i32,
                width: b.w.max(0.0).round() as u32,
                height: b.h.max(0.0).round() as u32,
                scale,
                primary: d == main,
            };
            (d, info)
        })
        .collect()
}

pub(crate) fn enumerate() -> Vec<DisplayInfo> {
    enumerate_with_ids().into_iter().map(|(_, i)| i).collect()
}

/// The `CGDirectDisplayID` currently behind a [`DisplayInfo::id`].
pub(crate) fn resolve(id: &str) -> Option<CGDirectDisplayID> {
    enumerate_with_ids()
        .into_iter()
        .find(|(_, info)| info.id == id)
        .map(|(d, _)| d)
}
