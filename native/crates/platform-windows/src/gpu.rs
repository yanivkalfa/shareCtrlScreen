//! The concrete types behind `platform_api`'s `&dyn Any` GPU handles on Windows.
//! Only adapters in this crate create or downcast them.

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

/// A D3D11 device shared by capture + encoder (host) or decoder + presenter
/// (viewer).
pub struct D3d11Device(pub ID3D11Device);

/// A texture slice: a desktop-duplication surface (BGRA) or a decoder output
/// (NV12 texture array + slice index).
pub struct D3d11Texture {
    pub texture: ID3D11Texture2D,
    pub array_index: u32,
}
