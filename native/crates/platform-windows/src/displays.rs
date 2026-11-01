//! Display enumeration for the "share display" setting.
//!
//! Only outputs of the default adapter (index 0) are offered: the capture crate
//! creates its D3D11 device on the default adapter, and Desktop Duplication
//! requires the device and the output to share an adapter. Ids are
//! `"dxgi:<output>"`, stable across restarts while the cabling is unchanged.

use platform_api::DisplayInfo;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

const PREFIX: &str = "dxgi:";

/// Parse an id produced by [`enumerate`]. `None` for anything else, including ids
/// saved by older builds (which used Electron display ids).
pub fn output_index(id: &str) -> Option<u32> {
    id.strip_prefix(PREFIX)?.parse().ok()
}

fn monitor_scale(monitor: HMONITOR) -> f64 {
    let (mut x, mut y) = (0u32, 0u32);
    // SAFETY: `monitor` comes from a live DXGI output description.
    match unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) } {
        Ok(()) if x > 0 => x as f64 / 96.0,
        _ => 1.0,
    }
}

pub fn enumerate() -> Vec<DisplayInfo> {
    let mut out = Vec::new();
    // SAFETY: standard DXGI enumeration; every interface is released on drop.
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return out;
        };
        let Ok(adapter) = factory.EnumAdapters1(0) else {
            return out;
        };
        let mut i = 0u32;
        while let Ok(output) = adapter.EnumOutputs(i) {
            if let Ok(desc) = output.GetDesc() {
                if desc.AttachedToDesktop.as_bool() {
                    let r = desc.DesktopCoordinates;
                    let name_len = desc
                        .DeviceName
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(desc.DeviceName.len());
                    out.push(DisplayInfo {
                        id: format!("{PREFIX}{i}"),
                        name: String::from_utf16_lossy(&desc.DeviceName[..name_len])
                            .trim_start_matches(r"\\.\")
                            .to_string(),
                        x: r.left,
                        y: r.top,
                        width: (r.right - r.left).max(0) as u32,
                        height: (r.bottom - r.top).max(0) as u32,
                        scale: monitor_scale(desc.Monitor),
                        primary: r.left == 0 && r.top == 0,
                    });
                }
            }
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip_and_reject_foreign_values() {
        assert_eq!(output_index("dxgi:0"), Some(0));
        assert_eq!(output_index("dxgi:3"), Some(3));
        // Electron-era ids stored by older versions must not be misread.
        assert_eq!(output_index("2528732444"), None);
        assert_eq!(output_index("dxgi:x"), None);
    }

    #[test]
    fn enumeration_ids_parse() {
        // Runs on any Windows box; a headless CI runner may have no outputs.
        for d in enumerate() {
            assert!(output_index(&d.id).is_some(), "{}", d.id);
            assert!(d.width > 0 && d.height > 0);
        }
    }
}
