//! Typed failures of the Wayland video surface, each worded so the user (or the
//! support log) says what to do. At the `platform-api` boundary they become
//! `SurfaceError::Backend` with the same message.

use platform_api::SurfaceError;

#[derive(Debug, thiserror::Error)]
pub enum SpikeError {
    #[error(
        "the app window is a {0} window, not a Wayland one; \
         the Wayland video surface only runs in a Wayland session"
    )]
    NotWayland(&'static str),
    #[error(
        "no GTK window owns the Wayland surface given to create_surface; \
         create the video surface on the UI thread after the app window is shown"
    )]
    NoGtkWindow,
    #[error("the app window has no content widget for the video to cover")]
    NoContent,
    #[error(
        "the compositor does not offer {interface} v{version}+, which the in-window \
         video needs; the separate video window (decision #3) is used instead"
    )]
    MissingGlobal {
        interface: &'static str,
        version: u32,
    },
    #[error("Wayland connection: {0}")]
    Wayland(String),
    #[error("XDG_RUNTIME_DIR is not set, so this is not a complete Wayland session")]
    NoRuntimeDir,
    #[error("could not create the video surface's 1x1 buffer in XDG_RUNTIME_DIR: {0}")]
    Shm(#[source] std::io::Error),
    #[error("no GPU adapter can draw into the Wayland video surface: {0}")]
    NoAdapter(String),
    #[error("GPU device: {0}")]
    Device(String),
}

impl From<SpikeError> for SurfaceError {
    fn from(e: SpikeError) -> Self {
        SurfaceError::Backend(e.to_string())
    }
}
