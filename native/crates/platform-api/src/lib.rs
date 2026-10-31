//! The OS boundary of ShareCtrlScreen (plan 04 §2).
//!
//! Everything the engine needs from an operating system is a trait in this
//! crate; `platform-windows`, `platform-macos` and `platform-linux` implement
//! them, and the `mock` feature provides in-memory implementations for tests.
//! The crate has **no OS dependencies**, so it builds and tests everywhere.
//!
//! ## Threading model (keep it)
//!
//! The media objects — [`ScreenCapture`], [`VideoEncoder`], [`VideoDecoder`],
//! [`Presenter`], [`InputInjector`], [`ShortcutHook`], [`Clipboard`] — are
//! **created on the thread that uses them** (the host/viewer media thread, the
//! transport thread, the hook thread, the clipboard thread) by calling the
//! matching [`Platform`] factory there. They are therefore not required to be
//! `Send`: COM interfaces, X11 connections and CFRunLoop-bound objects can live
//! inside them unchanged. [`Platform`], [`VideoSurface`], [`SecretStore`] and
//! [`Autostart`] are shared across threads and are `Send + Sync`.
//!
//! ## Zero-copy
//!
//! GPU objects cross trait boundaries as `&dyn Any` ([`FrameData::Native`],
//! [`ScreenCapture::device`], [`Presenter::device`]). A backend's encoder
//! downcasts to the type its own capture produces (for example a D3D11 texture,
//! an IOSurface-backed `CVPixelBuffer`, or a DMA-BUF descriptor). Mixing
//! backends is not supported; every backend must also accept
//! [`FrameData::Cpu`] so software codecs and tests work.

pub mod capture;
pub mod codec;
pub mod frame;
pub mod input;
pub mod keymap;
pub mod os;
pub mod platform;
pub mod surface;
pub mod unavailable;

#[cfg(feature = "mock")]
pub mod mock;

pub use capture::{CaptureError, CapturedFrame, CursorShape, CursorUpdate, ScreenCapture};
pub use codec::{
    Codec, CodecError, CodecFactory, EncodedUnit, EncoderConfig, VideoDecoder, VideoEncoder,
};
pub use frame::{CpuPlane, Frame, FrameData, MoveRect, PixelFormat, Point, Rect};
pub use input::{InputError, InputInjector, KeyForwarder, ShortcutHook};
pub use keymap::{KeyId, Set1};
pub use os::{Autostart, Clipboard, Paths, SecretStore, AUTOSTART_FLAG, MAX_TEXT_BYTES};
pub use platform::{
    DisplayInfo, OsPermission, PermissionReport, PermissionState, Platform, PlatformInfo,
};
pub use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
pub use surface::{FileDrop, Presenter, SurfaceError, VideoInput, VideoSurface};
