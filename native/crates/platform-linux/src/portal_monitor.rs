//! Which monitor the user picked in the ScreenCast dialog, for input.
//!
//! On Wayland the engine has no target rectangle for the shared screen (the
//! monitor is chosen in the portal dialog, not by display id), so the
//! injector must learn it from the capture: the granted stream's `position`
//! and `size` (compositor logical coordinates, the space xdg-output and EI
//! regions use) and, from ScreenCast version 5, its `mapping_id`, which EIS
//! implementations attach to the matching EI region. The capture publishes it
//! on every successful open and clears it when it lets go of the session; the
//! injector reads it when it has no target.
//!
//! ## References
//! * ScreenCast `Start` stream properties `position`, `size`, `mapping_id`:
//!   <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html>
//! * EI `ei_device.region_mapping_id`:
//!   <https://gitlab.freedesktop.org/libinput/libei/-/blob/main/proto/protocol.xml>

use std::sync::{Mutex, PoisonError};

use ashpd::desktop::screencast::Stream;
use platform_api::Rect;

/// The granted monitor of one ScreenCast stream.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Monitor {
    /// Logical position and size; `None` when the portal did not say.
    pub rect: Option<Rect>,
    pub mapping_id: Option<String>,
}

impl Monitor {
    pub(crate) fn of(stream: &Stream) -> Self {
        let rect = match (stream.position(), stream.size()) {
            (Some((x, y)), Some((w, h))) => match (u32::try_from(w), u32::try_from(h)) {
                (Ok(w), Ok(h)) if w > 0 && h > 0 => Some(Rect { x, y, w, h }),
                _ => None,
            },
            _ => None,
        };
        Self {
            rect,
            mapping_id: stream.mapping_id().map(str::to_owned),
        }
    }
}

static CURRENT: Mutex<Option<Monitor>> = Mutex::new(None);

/// Record the monitor of the stream the capture just opened.
pub(crate) fn publish(monitor: Monitor) {
    *CURRENT.lock().unwrap_or_else(PoisonError::into_inner) = Some(monitor);
}

/// Forget the monitor: the capture let go of its session, so a later
/// injector must not aim at a monitor from a session that is over.
pub(crate) fn clear() {
    *CURRENT.lock().unwrap_or_else(PoisonError::into_inner) = None;
}

/// The monitor of the capture's live session, if any.
pub(crate) fn current() -> Option<Monitor> {
    CURRENT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}
