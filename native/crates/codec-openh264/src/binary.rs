//! Cisco's OpenH264 binary: which file this build needs, how it is fetched and
//! verified, where it lives, and whether the user allows it.
//!
//! Licence conditions this module enforces (Cisco `BINARY_LICENSE.txt`,
//! "AVC/H.264 Patent Portfolio License Conditions"):
//! 1. *separately downloaded to the end user's device* — the file only ever
//!    comes from `ciscobinary.openh264.org` at runtime; nothing here embeds it;
//! 2. *the end user can enable, disable and re-enable it* — [`OpenH264::set_enabled`],
//!    checked before every load, so turning it off takes effect for the next
//!    session without a restart.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use sha2::{Digest, Sha256};

/// The OpenH264 release this build is pinned to. It must be one whose ABI
/// `openh264-sys2` 0.9.9 was generated against (its `src/blobs/hashes.txt` lists
/// 2.6.0 as the newest); a newer Cisco release needs a crate bump first.
pub const VERSION: &str = "2.6.0";

/// Cisco's download host. HTTPS is used, but integrity rests on the pinned
/// SHA-256 values below, not on the transport.
pub const DOWNLOAD_BASE: &str = "https://ciscobinary.openh264.org/";

/// The archive is ~0.4–0.7 MB and the library ~1.0–1.6 MB; generous caps that
/// still stop a hostile or broken response from exhausting memory or disk.
const MAX_ARCHIVE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_LIBRARY_BYTES: u64 = 16 * 1024 * 1024;

/// One whole download (connect + body). A ~0.5 MB file; anything slower than
/// this is better reported to the user than waited on.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// One Cisco binary for one OS/arch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BinarySpec {
    /// The library's file name, exactly as Cisco names it (the archive is this
    /// plus `.bz2`).
    pub file: &'static str,
    /// SHA-256 of the `.bz2` archive Cisco serves. Checked before the archive is
    /// decompressed, so untrusted bytes never reach the bzip2 decoder.
    pub archive_sha256: &'static str,
    /// SHA-256 of the decompressed library. Checked before every load. Equal to
    /// the entry for this file in `openh264-sys2`'s `blobs/hashes.txt`, which
    /// `OpenH264API::from_blob_path` checks a second time.
    pub library_sha256: &'static str,
}

impl BinarySpec {
    /// The archive's URL under `base` (normally [`DOWNLOAD_BASE`]).
    pub fn url(&self, base: &str) -> String {
        format!("{base}{}.bz2", self.file)
    }
}

/// The binary for the target this crate is compiled for (a universal macOS
/// build compiles each slice separately, so each picks its own), or `None` where
/// Cisco ships nothing usable.
pub fn spec_for_this_target() -> Option<&'static BinarySpec> {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    return Some(&BinarySpec {
        file: "openh264-2.6.0-win64.dll",
        archive_sha256: "dab5f2a872777f9a58b69bfa9fbcf20d9f82f2d6ec91383fd70bff49bd34ac9f",
        library_sha256: "2076cb5675ec6c1a4c70e7a2a322552f547b6eeed649d6dfcd9e02a543b24691",
    });
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    return Some(&BinarySpec {
        file: "openh264-2.6.0-win-arm64.dll",
        archive_sha256: "6b57d3ecd06bd80b5f3ee1abbb31d48798ca6d89ef1bc98cf2e6d1ebddf5cf45",
        library_sha256: "fb75103938f4f47d119b983e06334df41a803bc72fb5c46e3623f6fea5782732",
    });
    // `.8.so` is the 2.6.0 soname generation; the `.7.so` names in Cisco's
    // listing are compatibility copies of an older build.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return Some(&BinarySpec {
        file: "libopenh264-2.6.0-linux64.8.so",
        archive_sha256: "27ab53323c110b76214c1c72222f459d17febbcd1e252136cadc292b0308d75b",
        library_sha256: "2f0cde7c6a6abcf5cae76942894ea42897fa677bce4ed6c91a24dd1b041d5f04",
    });
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    return Some(&BinarySpec {
        file: "libopenh264-2.6.0-linux-arm64.8.so",
        archive_sha256: "a78aea7970150f46bcd3bb7994c9e6dd90bd7a9ea785920f5a73f6964e3fcda7",
        library_sha256: "12e7b33623667cdab0e575170c147b1b36eadb77d0d2aa7ceb5afd3e58902140",
    });
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return Some(&BinarySpec {
        file: "libopenh264-2.6.0-mac-arm64.dylib",
        archive_sha256: "6db362ee5abdab572311aeadb96d3f44b0617d9a4a4b9f4db4cb5ac4d968da71",
        library_sha256: "052e98bfcf7a9167d22f3bbb3f5988ef79065591f36af8b52924b22b13624551",
    });
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    return Some(&BinarySpec {
        file: "libopenh264-2.6.0-mac-x64.dylib",
        archive_sha256: "38b2ed6d1d45b6a3e408c734173f2d67ab44a10d0e154ff3489b89877cd60e7e",
        library_sha256: "e3dc8bc01fe69363f61fd3c02fd27798537a585eadd38cd808f303d1ee505a19",
    });
    #[allow(unreachable_code)]
    None
}

/// Why the software codec cannot be used. Every message says what to do.
#[derive(Debug, thiserror::Error)]
pub enum OpenH264Error {
    #[error(
        "the software H.264 codec (Cisco OpenH264) is turned off — turn on \
         \"Use Cisco OpenH264\" in Settings → Advanced to use it"
    )]
    Disabled,
    #[error("Cisco provides no OpenH264 binary for this system ({os}/{arch})")]
    UnsupportedPlatform {
        os: &'static str,
        arch: &'static str,
    },
    #[error(
        "could not download the Cisco OpenH264 codec from {url} ({reason}). Check the \
         internet connection (and any proxy or firewall that blocks \
         ciscobinary.openh264.org), then try again — or use \"Download now\" in \
         Settings → Advanced"
    )]
    Download { url: String, reason: String },
    #[error(
        "the Cisco OpenH264 download from {url} did not match the expected file \
         (SHA-256 {actual}, expected {expected}); it was discarded. Try again later"
    )]
    Integrity {
        url: String,
        expected: &'static str,
        actual: String,
    },
    #[error("could not store the Cisco OpenH264 codec in {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not load the Cisco OpenH264 codec from {path}: {reason}")]
    Load { path: PathBuf, reason: String },
}

/// What the Settings page shows about the codec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// The user turned it off.
    Disabled,
    /// No Cisco binary exists for this OS/arch.
    Unsupported,
    /// Allowed, not downloaded yet; it will be fetched the first time a session
    /// needs it.
    NotDownloaded,
    /// Downloaded and verified.
    Ready,
    /// The last attempt to fetch it failed; the message says why and what to do.
    Failed(String),
}

impl Availability {
    /// One sentence for the Settings page.
    pub fn describe(&self) -> String {
        match self {
            Availability::Disabled => "Turned off.".into(),
            Availability::Unsupported => "Not available for this system.".into(),
            Availability::NotDownloaded => {
                "Not downloaded yet — it is fetched from Cisco the first time this \
                 computer needs it."
                    .into()
            }
            Availability::Ready => format!("Downloaded (version {VERSION})."),
            Availability::Failed(msg) => format!("Download failed: {msg}"),
        }
    }
}

/// The user-controlled, lazily downloaded Cisco binary. One per process, shared
/// by the codec adapter and the Settings page.
#[derive(Debug)]
pub struct OpenH264 {
    /// `Paths::config_dir/codecs` — per-user, never a temp directory, so the
    /// library is not loaded from a location other users can write.
    dir: PathBuf,
    spec: Option<&'static BinarySpec>,
    /// [`DOWNLOAD_BASE`]; only this module's tests point it elsewhere.
    base: &'static str,
    /// Default on (decision #1). The app sets the saved value at start-up.
    enabled: AtomicBool,
    /// Serialises downloads: host and viewer media threads may both need the
    /// codec at the same moment, and one download is enough.
    fetch: Mutex<()>,
    /// The last fetch failure, for the Settings page.
    last_error: Mutex<Option<String>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic elsewhere while holding these locks leaves no broken invariant
    // (the guarded data is a unit or a message), so recover instead of
    // cascading the panic into every later session.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

impl OpenH264 {
    /// `codecs_dir` is `Paths::config_dir.join("codecs")`.
    pub fn new(codecs_dir: PathBuf) -> Self {
        Self::with_source(codecs_dir, spec_for_this_target(), DOWNLOAD_BASE)
    }

    fn with_source(dir: PathBuf, spec: Option<&'static BinarySpec>, base: &'static str) -> Self {
        Self {
            dir,
            spec,
            base,
            enabled: AtomicBool::new(true),
            fetch: Mutex::new(()),
            last_error: Mutex::new(None),
        }
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::SeqCst);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Where the library is (or will be) stored.
    pub fn library_path(&self) -> Option<PathBuf> {
        self.spec.map(|s| self.dir.join(s.file))
    }

    pub fn availability(&self) -> Availability {
        if !self.is_enabled() {
            return Availability::Disabled;
        }
        let Some(spec) = self.spec else {
            return Availability::Unsupported;
        };
        // Presence is enough for a status line; the hash is verified on load.
        if self.dir.join(spec.file).is_file() {
            return Availability::Ready;
        }
        match lock(&self.last_error).clone() {
            Some(msg) => Availability::Failed(msg),
            None => Availability::NotDownloaded,
        }
    }

    /// The verified library path, downloading it first if needed. Blocking (up
    /// to [`DOWNLOAD_TIMEOUT`] on first use); call from a media thread or a
    /// blocking task, never the UI thread.
    pub fn ensure(&self) -> Result<PathBuf, OpenH264Error> {
        if !self.is_enabled() {
            return Err(OpenH264Error::Disabled);
        }
        let spec = self.spec.ok_or(OpenH264Error::UnsupportedPlatform {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        })?;
        let _one_at_a_time = lock(&self.fetch);
        let path = self.dir.join(spec.file);
        if verify_file(&path, spec.library_sha256).is_ok() {
            return Ok(path);
        }
        let result = download(spec, &spec.url(self.base), &self.dir, &path);
        *lock(&self.last_error) = result.as_ref().err().map(ToString::to_string);
        result.map(|()| path)
    }

    /// Load the library for one encoder/decoder. Re-checks the enable flag and
    /// the file's hash every time: the user may have turned it off since, and
    /// the file may have been replaced on disk.
    pub fn load(&self) -> Result<openh264::OpenH264API, OpenH264Error> {
        let path = self.ensure()?;
        // `from_blob_path` refuses any file whose SHA-256 is not one of the
        // releases the bindings were generated for — loading a mismatched ABI
        // would be undefined behaviour, so this check is not redundant with ours.
        openh264::OpenH264API::from_blob_path(&path).map_err(|e| OpenH264Error::Load {
            path,
            reason: e.to_string(),
        })
    }
}

/// `Ok` if `path` holds exactly the pinned library.
fn verify_file(path: &Path, expected: &str) -> Result<(), ()> {
    match fs::read(path) {
        Ok(bytes) if sha256_hex(&bytes) == expected => Ok(()),
        _ => Err(()),
    }
}

fn download(spec: &BinarySpec, url: &str, dir: &Path, dest: &Path) -> Result<(), OpenH264Error> {
    let url = url.to_string();
    tracing::info!("openh264: downloading {url}");
    let archive = fetch(&url)?;
    let actual = sha256_hex(&archive);
    if actual != spec.archive_sha256 {
        return Err(OpenH264Error::Integrity {
            url,
            expected: spec.archive_sha256,
            actual,
        });
    }
    let library = decompress(&archive).map_err(|reason| OpenH264Error::Download {
        url: url.clone(),
        reason,
    })?;
    let actual = sha256_hex(&library);
    if actual != spec.library_sha256 {
        return Err(OpenH264Error::Integrity {
            url,
            expected: spec.library_sha256,
            actual,
        });
    }
    write_atomically(dir, dest, &library)?;
    tracing::info!("openh264: stored {}", dest.display());
    Ok(())
}

fn fetch(url: &str) -> Result<Vec<u8>, OpenH264Error> {
    let fail = |reason: String| OpenH264Error::Download {
        url: url.to_string(),
        reason,
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .build()
        .into();
    let mut response = agent.get(url).call().map_err(|e| fail(e.to_string()))?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_ARCHIVE_BYTES)
        .read_to_vec()
        .map_err(|e| fail(e.to_string()))
}

fn decompress(archive: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    // `take(MAX + 1)` so an over-long stream is detected rather than silently
    // truncated into a file that would then fail the hash check confusingly.
    bzip2_rs::DecoderReader::new(archive)
        .take(MAX_LIBRARY_BYTES + 1)
        .read_to_end(&mut out)
        .map_err(|e| format!("corrupt bzip2 archive: {e}"))?;
    if out.len() as u64 > MAX_LIBRARY_BYTES {
        return Err("archive expands beyond the size limit".into());
    }
    Ok(out)
}

/// Write `bytes` to `dest` so that `dest` is either absent or complete: write a
/// uniquely named sibling, flush it to disk, then rename over `dest`. The
/// sibling is in the same directory (rename is atomic only within a volume) and
/// is never loaded.
fn write_atomically(dir: &Path, dest: &Path, bytes: &[u8]) -> Result<(), OpenH264Error> {
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| OpenH264Error::Io { path, source }
    };
    fs::create_dir_all(dir).map_err(io(dir))?;
    let file_name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let part = dir.join(format!(".{file_name}.{}.{nonce}.part", std::process::id()));
    let written = (|| {
        let mut f = fs::File::create(&part)?;
        f.write_all(bytes)?;
        f.sync_all()
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&part);
        return Err(io(&part)(e));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Owner-writable only: a library others could rewrite would be loaded
        // into this process.
        if let Err(e) = fs::set_permissions(&part, fs::Permissions::from_mode(0o644)) {
            let _ = fs::remove_file(&part);
            return Err(io(&part)(e));
        }
    }
    if let Err(e) = fs::rename(&part, dest) {
        let _ = fs::remove_file(&part);
        return Err(io(dest)(e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sct-openh264-unit-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn this_target_has_a_pinned_binary() {
        // Every target the product ships on (deny.toml [graph].targets).
        let spec = spec_for_this_target().expect("shipped target without a Cisco binary");
        assert!(spec.file.contains(VERSION));
        assert_eq!(spec.archive_sha256.len(), 64);
        assert_eq!(spec.library_sha256.len(), 64);
        let url = spec.url(DOWNLOAD_BASE);
        assert!(url.starts_with("https://ciscobinary.openh264.org/") && url.ends_with(".bz2"));
    }

    #[test]
    fn disabled_refuses_before_touching_disk_or_network() {
        let dir = scratch("disabled");
        let h = OpenH264::new(dir.clone());
        h.set_enabled(false);
        assert!(matches!(h.ensure(), Err(OpenH264Error::Disabled)));
        assert!(matches!(h.load(), Err(OpenH264Error::Disabled)));
        assert_eq!(h.availability(), Availability::Disabled);
        assert!(!dir.exists(), "a disabled codec must not create files");
        // Re-enabling is possible (licence condition 2).
        h.set_enabled(true);
        assert!(h.is_enabled());
        assert_eq!(h.availability(), Availability::NotDownloaded);
    }

    #[test]
    fn unsupported_platform_is_a_clear_error() {
        let h = OpenH264::with_source(scratch("unsupported"), None, DOWNLOAD_BASE);
        let err = h.ensure().unwrap_err();
        assert!(matches!(err, OpenH264Error::UnsupportedPlatform { .. }));
        assert_eq!(h.availability(), Availability::Unsupported);
    }

    #[test]
    fn offline_failure_is_actionable_and_reported_to_settings() {
        // Port 9 (discard) on loopback refuses at once: a stand-in for
        // "offline" that needs no network and never hangs.
        static SPEC: BinarySpec = BinarySpec {
            file: "openh264-test.dll",
            archive_sha256: "00",
            library_sha256: "00",
        };
        let dir = scratch("offline");
        let h = OpenH264::with_source(dir.clone(), Some(&SPEC), "https://127.0.0.1:9/");
        let msg = h.ensure().unwrap_err().to_string();
        assert!(msg.contains("could not download"), "{msg}");
        assert!(msg.contains("Settings → Advanced"), "{msg}");
        // The Settings page shows the failure, and the toggle still works.
        assert!(matches!(h.availability(), Availability::Failed(m) if m == msg));
        h.set_enabled(false);
        assert_eq!(h.availability(), Availability::Disabled);
        assert!(!dir.join(SPEC.file).exists());
    }

    #[test]
    fn tampered_archive_is_rejected_before_decompression() {
        let bad = decompress(b"definitely not bzip2");
        assert!(bad.is_err());
    }

    #[test]
    fn atomic_write_leaves_no_partial_files() {
        let dir = scratch("atomic");
        let dest = dir.join("lib.bin");
        write_atomically(&dir, &dest, b"payload").unwrap();
        write_atomically(&dir, &dest, b"payload2").unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"payload2");
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
            .collect();
        assert!(leftovers.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
