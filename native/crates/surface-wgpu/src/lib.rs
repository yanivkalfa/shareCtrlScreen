//! Shared pieces of the wgpu-based video surfaces (macOS Metal, Linux
//! Vulkan/GL — plan decision #2; Windows keeps D3D11 in `render`).
//!
//! * [`renderer`] — the one WGSL shader (NV12 BT.709 → RGB, letterbox, cursor
//!   sprite) and the draw sequence, rendering into any texture view.
//! * [`geometry`] — letterbox placement and pointer normalization (the two
//!   halves of one mapping).
//! * [`reveal`] — the hover-reveal state machine for the web toolbar.
//! * [`input`] — sink, held-input tracking and synthesized releases.
//! * [`state`] — what a surface shares across its UI, media and caller threads.
//!
//! Everything except [`renderer`] is pure logic with no GPU or OS dependency,
//! so it is unit-tested on every CI runner; the renderer is tested headless
//! against a real adapter in `tests/headless.rs`.
//!
//! ## References
//! * Windows implementation of the same contract: `render/src/lib.rs`
//!   (HLSL, letterbox, cursor) and `render/src/window.rs` (reveal, input).
//! * Contract: `platform_api::{VideoSurface, Presenter}` trait docs.

pub mod geometry;
pub mod input;
pub mod renderer;
pub mod reveal;
pub mod state;

pub use renderer::{DrawParams, Renderer, Source, SHADER_WGSL};
pub use state::SurfaceState;
