//! At-rest protection for saved passwords (auto-login).
//!
//! Auto-login inherently requires keeping the password itself — a hash can't be
//! replayed to the host. Storing it as plaintext in `config.json` would mean
//! anyone who can read the file gets the credential, so it is sealed with
//! **DPAPI** (`CryptProtectData`) first: the ciphertext is bound to the current
//! Windows user account, so copying the config file to another machine or
//! account yields nothing usable.
//!
//! This is not protection against code running AS this user — nothing at this
//! layer can be, since the app must be able to decrypt unattended. It removes
//! the "password sitting in a readable file" class of exposure, which is the
//! realistic one.

use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

/// Seal `plain` for this Windows account; returns lowercase hex, or `None` if
/// DPAPI refuses (in which case the caller must NOT fall back to plaintext).
pub fn protect(plain: &str) -> Option<String> {
    let mut input = plain.as_bytes().to_vec();
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_mut_ptr(),
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: in_blob points at `input` which outlives the call; `out` is filled
    // with a LocalAlloc'd buffer that we copy and then free.
    let ok = unsafe {
        CryptProtectData(
            &in_blob,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    if ok.is_err() || out.pbData.is_null() {
        return None;
    }
    // SAFETY: DPAPI filled cbData bytes at pbData.
    let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() };
    // SAFETY: DPAPI allocates the out buffer with LocalAlloc.
    unsafe { LocalFree(Some(HLOCAL(out.pbData as *mut _))) };
    Some(hex_encode(&bytes))
}

/// Reverse of [`protect`]. Returns `None` for malformed hex or a blob this
/// account cannot decrypt (e.g. the config was copied from another machine).
pub fn unprotect(hexed: &str) -> Option<String> {
    let mut input = hex_decode(hexed)?;
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_mut_ptr(),
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as in `protect`.
    let ok = unsafe {
        CryptUnprotectData(
            &in_blob,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    if ok.is_err() || out.pbData.is_null() {
        return None;
    }
    // SAFETY: DPAPI filled cbData bytes at pbData.
    let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() };
    // SAFETY: DPAPI allocates the out buffer with LocalAlloc.
    unsafe { LocalFree(Some(HLOCAL(out.pbData as *mut _))) };
    String::from_utf8(bytes).ok()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_through_dpapi() {
        let secret = "correct horse battery staple";
        let sealed = protect(secret).expect("DPAPI available on Windows");
        // Sealed form must not contain the plaintext.
        assert!(!sealed.contains("correct"));
        assert_eq!(unprotect(&sealed).as_deref(), Some(secret));
    }

    #[test]
    fn rejects_garbage() {
        assert!(unprotect("not-hex").is_none());
        assert!(unprotect("00112233").is_none()); // valid hex, not a DPAPI blob
    }

    #[test]
    fn hex_roundtrip() {
        let b = [0x00u8, 0x7f, 0x80, 0xff];
        assert_eq!(hex_decode(&hex_encode(&b)), Some(b.to_vec()));
    }
}
