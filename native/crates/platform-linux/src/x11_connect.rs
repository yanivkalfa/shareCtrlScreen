//! Connecting to an X server, including servers reachable only through the
//! Linux abstract socket namespace.
//!
//! An X server on Linux listens on two local sockets: the file
//! `/tmp/.X11-unix/X<n>` and the abstract socket `@/tmp/.X11-unix/X<n>`
//! (libxtrans creates both; libxcb tries the abstract one first). x11rb 0.14
//! only tries the file, then TCP. When the file cannot exist — WSLg mounts
//! `/tmp/.X11-unix` read-only, some sandboxes hide it — the server is
//! reachable only through the abstract socket and x11rb reports "connection
//! refused" (from its TCP attempt). [`x11_connect`] retries a local display
//! through the abstract socket with the same authentication lookup x11rb
//! uses, so it reaches every server libxcb reaches. Abstract sockets are Unix
//! sockets: file descriptors pass over them, so MIT-SHM works.
//!
//! Used by the capture and by the clipboard's file-list selection owner.
//! Clipboard *text* goes through `arboard`, which connects with x11rb itself
//! and so still needs the socket file (or `DISPLAY` over TCP).
//!
//! ## References
//! * x11rb 0.14 `RustConnection::connect` (socket path, then TCP) and
//!   `connect_to_stream_with_auth_info`; `DefaultStream::from_unix_stream`:
//!   <https://docs.rs/x11rb/0.14.0/x11rb/rust_connection/>
//! * libxcb `_xcb_open_unix` (abstract socket first, then the path):
//!   <https://gitlab.freedesktop.org/xorg/lib/libxcb/-/blob/master/src/xcb_util.c>
//! * Linux `unix(7)` "Abstract sockets"; Rust
//!   `std::os::linux::net::SocketAddrExt::from_abstract_name`.

use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixStream};

use x11rb::errors::ConnectError;
use x11rb::reexports::x11rb_protocol::parse_display::{parse_display, ParsedDisplay};
use x11rb::reexports::x11rb_protocol::xauth::get_auth;
use x11rb::rust_connection::{DefaultStream, RustConnection};

/// The abstract socket name of a local display, `None` for a remote one
/// (a host name or an explicit non-Unix protocol: those go over TCP and
/// there is nothing to retry).
fn abstract_name(d: &ParsedDisplay) -> Option<String> {
    let local = d.host.is_empty() || d.host == "unix";
    let unix = d.protocol.as_deref().is_none_or(|p| p == "unix");
    (local && unix).then(|| format!("/tmp/.X11-unix/X{}", d.display))
}

/// Connect like `RustConnection::connect(Some(display))`, falling back to the
/// abstract socket for a local display. On failure the error of the normal
/// attempt is returned: it names the address users know (`:0`).
pub fn x11_connect(display: &str) -> Result<(RustConnection, usize), ConnectError> {
    let first = match RustConnection::connect(Some(display)) {
        Ok(c) => return Ok(c),
        Err(e) => e,
    };
    let Ok(parsed) = parse_display(Some(display)) else {
        return Err(first);
    };
    let Some(name) = abstract_name(&parsed) else {
        return Err(first);
    };
    let attempt = || -> Result<(RustConnection, usize), ConnectError> {
        let addr = SocketAddr::from_abstract_name(name.as_bytes())?;
        let (stream, (family, address)) =
            DefaultStream::from_unix_stream(UnixStream::connect_addr(&addr)?)?;
        // Same policy as x11rb's own connect: a missing or unreadable
        // Xauthority means "try without credentials", which the server may
        // still accept (Xvfb without `-auth`, host-based access).
        let (auth_name, auth_data) = get_auth(family, &address, parsed.display)
            .unwrap_or(None)
            .unwrap_or_else(|| (Vec::new(), Vec::new()));
        let screen = usize::from(parsed.screen);
        let conn =
            RustConnection::connect_to_stream_with_auth_info(stream, screen, auth_name, auth_data)?;
        Ok((conn, screen))
    };
    // tracing's macros bring their own `display` helper into scope, so the
    // display name is logged under another binding.
    let server = display;
    attempt().map_err(|e| {
        tracing::debug!("x11: {server} not reachable through the abstract socket either: {e}");
        first
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(display: &str) -> Option<String> {
        abstract_name(&parse_display(Some(display)).expect("parses"))
    }

    #[test]
    fn local_displays_map_to_the_abstract_socket() {
        assert_eq!(name(":0").as_deref(), Some("/tmp/.X11-unix/X0"));
        assert_eq!(name(":12.1").as_deref(), Some("/tmp/.X11-unix/X12"));
    }

    #[test]
    fn remote_displays_are_not_retried() {
        assert_eq!(name("127.0.0.1:0"), None);
        assert_eq!(name("example.org:1"), None);
        assert_eq!(name("tcp/localhost:2"), None);
    }
}
