//! Shared data types for every crate in the workspace (Plan 04 §4, §9).
//!
//! Mirrors the `00-OVERVIEW-AND-PROTOCOL.md` contract **exactly** — the same
//! field names and values the Cloudflare signaling server relays and the same
//! data-channel message shapes. Byte-for-byte compatibility with the contract
//! is a hard requirement; the server needs no changes, only the opaque `data`
//! payloads differ (str0m ICE/DTLS instead of browser SDP).
//!
//! This crate is pure logic (no Windows / COM), so it builds and tests on any
//! host and is written first (§12 ordering constraint #1).

pub mod config;
pub mod control;
pub mod input;
pub mod signaling;

pub use config::{Config, Mode, Permission};
pub use control::ControlMsg;
pub use input::{Button, InputMsg};
pub use signaling::{IceCandidate, SignalData, SignalMsg};

/// Protocol version carried in the `register` message (`v`), always `1`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Lower-case UUID-v4 validation regex from contract §3.1, expressed as a
/// hand-rolled check to avoid a regex dependency in this hot-path-adjacent crate.
pub fn is_valid_uuid_v4(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    // 8-4-4-4-12 with dashes at 8,13,18,23; version nibble '4' at 14;
    // variant nibble in [89ab] at 19.
    for (i, &c) in b.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if c != b'-' {
                    return false;
                }
            }
            14 => {
                if c != b'4' {
                    return false;
                }
            }
            19 => {
                if !matches!(c, b'8' | b'9' | b'a' | b'b') {
                    return false;
                }
            }
            _ => {
                if !c.is_ascii_hexdigit() || c.is_ascii_uppercase() {
                    return false;
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_validation_matches_contract() {
        assert!(is_valid_uuid_v4("1c9a7b3e-8f21-4d6a-9e0b-2f4c8a1d5e73"));
        // Upper-case rejected (server accepts lower-case v4 only).
        assert!(!is_valid_uuid_v4("1C9A7B3E-8F21-4D6A-9E0B-2F4C8A1D5E73"));
        // Wrong version nibble.
        assert!(!is_valid_uuid_v4("1c9a7b3e-8f21-1d6a-9e0b-2f4c8a1d5e73"));
        // Wrong variant nibble.
        assert!(!is_valid_uuid_v4("1c9a7b3e-8f21-4d6a-1e0b-2f4c8a1d5e73"));
        assert!(!is_valid_uuid_v4("not-a-uuid"));
    }
}
