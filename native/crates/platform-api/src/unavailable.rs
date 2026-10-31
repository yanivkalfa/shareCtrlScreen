//! Fail-safe implementations for capabilities a platform (or a particular
//! machine) does not have. Each one refuses rather than pretends: no secret is
//! ever stored, no clipboard write claims success, no autostart toggle silently
//! does nothing.

use std::any::Any;
use std::path::PathBuf;

use crate::codec::{Codec, CodecError, CodecFactory, EncoderConfig, VideoDecoder, VideoEncoder};
use crate::os::{Autostart, Clipboard, SecretStore};

/// No secure storage: saving a password is refused (the engine then tells the
/// user and keeps prompting), and nothing stored elsewhere can be opened.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoSecretStore;

impl SecretStore for NoSecretStore {
    fn seal(&self, _plain: &str) -> Option<String> {
        None
    }
    fn open(&self, _sealed: &str) -> Option<String> {
        None
    }
}

/// No clipboard access: nothing is read, every write reports failure.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoClipboard;

impl Clipboard for NoClipboard {
    fn change_count(&self) -> u64 {
        0
    }
    fn get_text(&self) -> Option<String> {
        None
    }
    fn set_text(&self, _text: &str) -> Option<u64> {
        None
    }
    fn set_files(&self, _paths: &[PathBuf]) -> Option<u64> {
        None
    }
}

/// Start-at-login not supported: turning it on fails with `reason`.
#[derive(Debug, Clone, Copy)]
pub struct NoAutostart {
    pub reason: &'static str,
}

impl Autostart for NoAutostart {
    fn is_enabled(&self) -> bool {
        false
    }
    fn set_enabled(&self, on: bool) -> Result<(), String> {
        if on {
            Err(self.reason.to_string())
        } else {
            Ok(())
        }
    }
    fn reconcile(&self, want: bool) {
        if want {
            tracing_free_note(self.reason);
        }
    }
}

/// No codecs at all. Advertises H.264 because the protocol guarantees it (so
/// negotiation stays well-defined), but construction fails with `reason`, which
/// ends the session with that message.
#[derive(Debug, Clone, Copy)]
pub struct NoCodecs {
    pub reason: &'static str,
}

impl CodecFactory for NoCodecs {
    fn host_encodable(&self) -> Vec<Codec> {
        vec![Codec::H264]
    }
    fn viewer_decodable(&self) -> Vec<Codec> {
        vec![Codec::H264]
    }
    fn new_encoder(
        &self,
        _device: Option<&dyn Any>,
        _cfg: EncoderConfig,
    ) -> Result<Box<dyn VideoEncoder>, CodecError> {
        Err(CodecError::Backend(self.reason.to_string()))
    }
    fn new_decoder(
        &self,
        _device: Option<&dyn Any>,
        _codec: Codec,
        _width: u32,
        _height: u32,
    ) -> Result<Box<dyn VideoDecoder>, CodecError> {
        Err(CodecError::Backend(self.reason.to_string()))
    }
}

/// `platform-api` has no logging dependency; surface the note on stderr once.
fn tracing_free_note(reason: &str) {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| eprintln!("start at sign-in unavailable: {reason}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_refuses_instead_of_pretending() {
        assert_eq!(NoSecretStore.seal("pw"), None);
        assert_eq!(NoClipboard.set_text("x"), None);
        let a = NoAutostart { reason: "nope" };
        assert_eq!(a.set_enabled(true), Err("nope".to_string()));
        assert_eq!(a.set_enabled(false), Ok(()));
        let c = NoCodecs {
            reason: "no codecs",
        };
        assert!(c.host_encodable().contains(&Codec::H264));
        assert!(c.new_encoder(None, EncoderConfig::default()).is_err());
    }
}
