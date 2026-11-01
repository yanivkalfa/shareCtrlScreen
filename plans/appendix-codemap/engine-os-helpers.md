<!-- Generated 2026-10-31 by a parallel code-reading sweep (20 haiku readers, one per subsystem). Line numbers refer to the tree at commit 5a0d195. Facts were spot-checked but not every line was re-verified; treat as a map, not a spec. -->

# engine-os-helpers

## summary
Unit engine-os-helpers covers four files in native/crates/engine/src: secret.rs (126 lines, DPAPI sealing of saved auto-login passwords), autostart.rs (202 lines, HKCU Run-key 'start with Windows'), clipboard.rs (239 lines, Win32 clipboard text and CF_HDROP file-list access plus a change counter used as an echo guard), and transfer.rs (220 lines, receive-side file writer that treats peer input as hostile). Only secret.rs, autostart.rs and clipboard.rs call Windows APIs. transfer.rs is std-only apart from one USERPROFILE env lookup (transfer.rs:24-27) and the protocol crate's sanitize_file_name, FileMeta and MAX_FILE_BYTES.

secret.rs seals a password with CryptProtectData (secret.rs:22-50, flags CRYPTPROTECT_UI_FORBIDDEN, no entropy or description), so the blob is bound to the current Windows user. The output buffer is copied and freed with LocalFree (secret.rs:48). unprotect (secret.rs:54-81) reverses this and requires valid UTF-8. Hex encoding is OS-agnostic (secret.rs:83-100). Callers: lib.rs:458 (unprotect on read) and lib.rs:517-530 (protect after the host accepts a remembered password; on None nothing is saved and a toast is shown, never plaintext).

autostart.rs writes a REG_SZ value named 'ShareCtrlScreen' under HKCU\Software\Microsoft\Windows\CurrentVersion\Run (autostart.rs:22-23). The command is the quoted current_exe path plus ' --autostart' (autostart.rs:27-34). Public functions: is_enabled (84), set_enabled (91-138), reconcile (146-168, called from app main.rs:314 at startup with config.start_with_windows). main.rs:399 checks for the --autostart marker to start minimized; main.rs:248 calls set_enabled from the UI toggle.

clipboard.rs: sequence() wraps GetClipboardSequenceNumber (54-57). get_text (61-85) reads CF_UNICODETEXT under a RAII ClipboardGuard (28-51; open retries 5 times with 10 ms sleeps, 31-41). set_text (89-119) and set_files (132-175) allocate GMEM_MOVEABLE globals, hand ownership to the clipboard via SetClipboardData, and read the sequence only after the guard drops. set_files builds a DROPFILES header (fWide=true, pFiles=header size) followed by UTF-16 NUL-separated paths and a double NUL (130-131, 159-164). Callers: pipeline.rs:1927 (set_files when a received file is delivered), pipeline.rs:1960-1995 (poll thread using sequence, get_text, set_text), pipeline.rs:1592 (inbound text length check against MAX_TEXT_BYTES).

transfer.rs: receive_dir() is %USERPROFILE%\Downloads\ShareCtrlScreen, falling back to temp_dir (23-28). Incoming (31-108) is a small state machine: begin (41-67) checks the declared size against MAX_FILE_BYTES, sanitizes the name, creates the dir, picks a unique path, checks path.parent()==dir, then File::create. write (71-82) enforces the declared size. finish (86-97) flushes and aborts on a short write. abort (100-103) deletes the partial file. unique_path (112-129) inserts ' (n)' before the extension up to n=9999, then ' (dup)'. Callers: pipeline.rs:1144 (Option<Incoming> per connection), pipeline.rs:1645-1647 (receive_dir, then Incoming::begin).

## public_api
- native/crates/engine/src/secret.rs:22 pub fn protect(plain:&str)->Option<String>
- native/crates/engine/src/secret.rs:54 pub fn unprotect(hexed:&str)->Option<String>
- native/crates/engine/src/autostart.rs:27 pub const AUTOSTART_FLAG:&str = "--autostart"
- native/crates/engine/src/autostart.rs:84 pub fn is_enabled()->bool
- native/crates/engine/src/autostart.rs:91 pub fn set_enabled(on:bool)->Result<(),String>
- native/crates/engine/src/autostart.rs:146 pub fn reconcile(should_be_enabled:bool)
- native/crates/engine/src/clipboard.rs:24 pub const MAX_TEXT_BYTES:usize = 256*1024
- native/crates/engine/src/clipboard.rs:54 pub fn sequence()->u32
- native/crates/engine/src/clipboard.rs:61 pub fn get_text()->Option<String>
- native/crates/engine/src/clipboard.rs:89 pub fn set_text(text:&str)->Option<u32>
- native/crates/engine/src/clipboard.rs:132 pub fn set_files(paths:&[PathBuf])->Option<u32>
- native/crates/engine/src/transfer.rs:23 pub fn receive_dir()->PathBuf
- native/crates/engine/src/transfer.rs:31 pub struct Incoming (private fields file, path, written, expected)
- native/crates/engine/src/transfer.rs:41 pub fn Incoming::begin(meta:&FileMeta, dir:&Path)->Result<Self,String>
- native/crates/engine/src/transfer.rs:71 pub fn Incoming::write(&mut self, bytes:&[u8])->Result<(),String>
- native/crates/engine/src/transfer.rs:86 pub fn Incoming::finish(mut self)->Result<PathBuf,String>
- native/crates/engine/src/transfer.rs:100 pub fn Incoming::abort(self)
- native/crates/engine/src/transfer.rs:105 pub fn Incoming::progress(&self)->(u64,u64)

## platform_deps
- **CryptProtectData / CryptUnprotectData with no description, entropy, reserved or prompt struct, flags CRYPTPROTECT_UI_FORBIDDEN** @ native/crates/engine/src/secret.rs:15-18, 22-50 (protect), 54-81 (unprotect)
  - purpose: Seal and unseal the saved auto-login password for the current Windows user, unattended.
  - portable: Contract: encrypt so that only the same OS account on the same machine can decrypt; never return plaintext on failure; decrypt must need no user interaction; ciphertext stored as hex in config.json. Candidate semantics: macOS Keychain generic-password item (no Apple developer account needed for local keychain use); Linux Secret Service via libsecret, or a per-user key file with 0600 permissions (weaker than DPAPI; must be stated). unprotect must return None for foreign or corrupt blobs without panicking.
- **LocalFree on the DPAPI-allocated output buffer** @ native/crates/engine/src/secret.rs:48, 79
  - purpose: Release the buffer after copying it into a Vec.
  - portable: Ownership contract: copy then release once. Platforms whose API returns an owned Vec need no free.
- **HKCU Run key value 'ShareCtrlScreen' (REG_SZ) via RegOpenKeyExW, RegQueryValueExW (two-call sizing), RegSetValueExW, RegDeleteValueW, RegCloseKey** @ native/crates/engine/src/autostart.rs:14-19, 22-23, 40-50, 48-81, 91-138
  - purpose: Per-user logon autostart with no admin rights, visible in Task Manager Startup.
  - portable: Behavioural contract: per-user, no elevation; starts at user logon, not boot; registered command is the quoted absolute exe path plus ' --autostart'; set(false) on an absent entry is success; reconcile re-points a stale entry at startup; errors are human-readable strings. macOS: LaunchAgent plist in ~/Library/LaunchAgents with RunAtLoad and ProgramArguments [exe, --autostart]; writing the plist needs no Apple account. Linux: ~/.config/autostart/<name>.desktop (XDG autostart, quoted Exec with --autostart) or a systemd --user unit.
- **GetClipboardSequenceNumber** @ native/crates/engine/src/clipboard.rs:14-20, 53-57; used at 118, 174; pipeline.rs:1963, 1982
  - purpose: Monotonic OS counter bumped on every clipboard write; cheap change detection and echo guard.
  - portable: Contract: a per-write monotonic value readable without taking a lock. macOS: NSPasteboard changeCount (no lock needed). X11: no global counter; simulate by tracking selection owner and timestamp, or compare content. Wayland: no global counter without a data-control protocol; compare content. The echo guard must still ignore self-writes.
- **OpenClipboard(null HWND) and CloseClipboard, 5 tries at 10 ms, Drop-based guard** @ native/crates/engine/src/clipboard.rs:26-51
  - purpose: Serialize access to the single global clipboard and always close it.
  - portable: Contract: release on every path; keep hold time minimal; contention is normal, so retry briefly and return None rather than block. macOS has no open/close lock, so there is nothing to mirror. X11/Wayland reads can time out when the owner is slow, so a timeout budget replaces the lock.
- **GetClipboardData(CF_UNICODETEXT) with GlobalLock/GlobalUnlock; bounded scan of NUL-terminated UTF-16** @ native/crates/engine/src/clipboard.rs:61-85
  - purpose: Read clipboard text, bounded to MAX_TEXT_BYTES/2 code units.
  - portable: Contract: text only (no HTML, images or file lists); None when no text; bounded read so a malformed or huge clipboard cannot stall the control channel; copy out before the handle expires. macOS: NSPasteboard string(forType:) gives UTF-8 directly. Linux: text/plain;charset=utf-8 target. See gotcha on truncation.
- **EmptyClipboard + SetClipboardData(CF_UNICODETEXT) with a GMEM_MOVEABLE global whose ownership passes to the clipboard on success** @ native/crates/engine/src/clipboard.rs:89-119
  - purpose: Replace clipboard text with the peer's value.
  - portable: Contract: on success the OS owns the buffer; on failure the caller frees it; other formats are cleared. macOS: clearContents then setString. X11/Wayland: take selection ownership and keep answering SelectionRequest events while owned, which is a persistent-service requirement Windows does not have.
- **SetClipboardData(CF_HDROP) with a DROPFILES header (pFiles offset, fWide=true), NUL-separated UTF-16 path list, double NUL; DROPFILES type from Win32::UI::Shell** @ native/crates/engine/src/clipboard.rs:121-175 (imports at 14, 20, 133)
  - purpose: Make a received file pasteable as a real file in Explorer, browsers, chat clients and Office.
  - portable: Contract: put a file reference list on the clipboard that other apps accept as a paste of real files. macOS: NSPasteboard writeObjects with NSURL or public.file-url (Finder accepts). Linux: text/uri-list (file:// URIs, newline-separated); GNOME also uses x-special/gnome-copied-files. Paths must be absolute and exist at paste time. The URI form has no header struct. Percent-encoding is required.
- **std::env::var('USERPROFILE') for the receive folder** @ native/crates/engine/src/transfer.rs:23-28
  - purpose: Build %USERPROFILE%\Downloads\ShareCtrlScreen.
  - portable: No Windows API is used; only the variable name differs. Contract: a per-user Downloads directory created on demand. macOS and Linux: $HOME/Downloads/ShareCtrlScreen (Linux may honour XDG_DOWNLOAD_DIR).
- **Backslash as a separator in the traversal test name** @ native/crates/engine/src/transfer.rs:165; sanitize_file_name in protocol crate (called at transfer.rs:49, not read in this unit)
  - purpose: Traversal defence against Windows-style separators sent by a peer.
  - portable: On POSIX a backslash is a legal filename character. sanitize_file_name must strip both separators on every platform, or the test must be rewritten. The direct-child check at transfer.rs:56 is the backstop on all platforms.
- **std::fs create_dir_all, File::create, exists, remove_file** @ native/crates/engine/src/transfer.rs:50, 60, 102, 114, 124
  - purpose: Create the receive directory and file; remove partial files.
  - portable: Cross-platform std. The exists() then File::create sequence at transfer.rs:114 and 60 is a TOCTOU window; use create_new (O_EXCL) on all platforms. remove_file errors are ignored at transfer.rs:102; antivirus or indexers can hold the file on Windows, so the port should not assume deletion always succeeds.

## portable_parts
- transfer.rs:31-108 Incoming state machine (begin, write, finish, abort, progress) is OS-agnostic std code apart from the receive_dir() seam at transfer.rs:23-28.
- transfer.rs:112-129 unique_path (pure std, OS-agnostic).
- transfer.rs:21-22 and 1-13 policy and documentation are OS-agnostic.
- secret.rs:83-100 hex_encode and hex_decode are pure functions.
- autostart.rs:27-34 desired_command shape (quoted exe + flag) is OS-agnostic in intent; only the quoting rule is Windows-specific. autostart.rs:146-168 reconcile is OS-agnostic logic over is_enabled/set_enabled/current_command.
- autostart.rs:36-38 to_utf16 is a pure helper, Windows-only in use.
- clipboard.rs:24 MAX_TEXT_BYTES and the bounded-read policy are OS-agnostic. clipboard.rs:131-163 DROPFILES layout is Windows-specific and sits behind the set_files seam.
- pipeline.rs:1960-1995 (poll loop) depends only on the four clipboard operations and the constant, so it is already portable once a backend exists (outside this unit; cited for the seam).

## abstraction_points
- secret.rs protect/unprotect can become a trait with seal(&str)->Option<String> and open(&str)->Option<String>. Impls: DPAPI (Windows), Keychain (macOS), libsecret or 0600 key file (Linux). hex_encode/hex_decode (secret.rs:83-100) are shared.
- autostart.rs is_enabled/set_enabled/current_command can become a trait with those three operations. Impls: HKCU Run (Windows), LaunchAgent (macOS), XDG .desktop or systemd --user (Linux). AUTOSTART_FLAG (autostart.rs:27), desired_command shape and reconcile (autostart.rs:146-168) stay shared; only the quoting rule and storage are per-OS.
- clipboard.rs sequence/get_text/set_text/set_files can become a trait ClipboardBackend with change_counter()->u32, get_text()->Option<String>, set_text(&str)->Option<u32>, set_files(&[PathBuf])->Option<u32>. The poll loop at pipeline.rs:1960-1995 already depends only on these. Linux may need a content-derived counter, which changes the echo-guard semantics.
- transfer.rs needs no trait; receive_dir() at transfer.rs:23 is the only seam, plus the std::fs calls.

## cross_module_contracts
- Threading: clipboard get_text/set_text/set_files run on the pipeline poll thread (pipeline.rs:1960-1995, 300 ms sleep, stop and enabled are Arc<AtomicBool>, inbound is an mpsc receiver of String). They may block for up to about 50 ms of retry (clipboard.rs:34-39). set_files is also called from deliver_received_file (pipeline.rs:1927).
- Global OS resource: the Windows clipboard is process-wide. The only in-process serialization is the test-only CLIPBOARD_TEST_LOCK (clipboard.rs:185). Production relies on OpenClipboard. Any port must serialize clipboard access if two threads can call it concurrently.
- Ownership: set_text and set_files pass the HGLOBAL to the clipboard on success (clipboard.rs:106-110, 166-169). On success nothing Rust-side outlives the call.
- Echo-guard contract: the caller stores the returned sequence as last_seq (pipeline.rs:1975-1976) and treats later changes as local only when get_text differs from last_text (pipeline.rs:1982-1987). Both guards are required. The sequence must be read only after the clipboard is closed, which these functions guarantee (clipboard.rs:117, 173).
- Secret contract: lib.rs:458 calls unprotect on read (None means no auto-login). lib.rs:517-530 calls protect only after the host accepts the password and only when the viewer chose remember; on None it does not save and shows a toast. saved_logins (lib.rs:521-523) stores the hex blob keyed by host, and auto_login is a bool. A blob from another account or machine returns None (secret.rs:52-53), so auto-login fails silently there.
- Autostart contract: main.rs:314 calls reconcile(config.start_with_windows) at startup. main.rs:248 calls set_enabled(b) on UI change and surfaces the Err string. main.rs:399 checks argv for AUTOSTART_FLAG to start minimized. A port must keep the flag and the reconcile-on-startup call.
- Transfer contract: pipeline.rs:1144 holds Option<Incoming> per connection. pipeline.rs:1645-1647 calls receive_dir then Incoming::begin on a peer FileMeta, and refuses on Err without creating anything. write() does not remove the file on overflow (transfer.rs:74-76), so the caller must call abort(). finish() returns the path, which is then passed to set_files (pipeline.rs:1927). Whether every disconnect path calls abort was not verified in this unit.
- Size constants: MAX_TEXT_BYTES is used at pipeline.rs:1592 (inbound) and 1987 (outbound). MAX_FILE_BYTES comes from protocol::transfer (transfer.rs:19, 42) and is part of the peer wire contract.

## gotchas
- clipboard.rs:59-60 says get_text returns None when text is larger than MAX_TEXT_BYTES, but clipboard.rs:76-81 truncates silently at MAX_TEXT_BYTES/2 UTF-16 code units and returns Some. pipeline.rs:1987 then checks UTF-8 byte length, so non-ASCII text can be truncated or dropped. The doc and the code disagree; a port must pick one and match the doc.
- clipboard.rs:112-116: reading the sequence before the clipboard is closed returns a stale value; the echo guard then mistakes its own write for a local edit and the two peers ping-pong text forever. This is a hard invariant. Any replacement counter must advance only after the write is committed, and the port needs a test for this loop.
- clipboard.rs:95-96 and 149-150: after a successful SetClipboardData the OS owns the global; freeing it is a double free. On failure the caller must free it (clipboard.rs:107-108, 167-168).
- clipboard.rs:99-102 (set_text): if GlobalLock returns null after GlobalAlloc succeeded, the function returns None without GlobalFree(h), so the global leaks. set_files (clipboard.rs:154-158) does free it. This is a real leak on a rare path.
- clipboard.rs:31-41: the open budget is 5 tries at 10 ms (about 50 ms). A busy clipboard makes set_text or set_files return None silently. deliver_received_file then warns and returns false (pipeline.rs:1927-1929).
- clipboard.rs:83: empty text returns None, so copying an empty string is not synced.
- clipboard.rs:204 (test): after set_files, get_text must be None so a file drop does not overwrite the peer's text clipboard. A port that also writes a text fallback (for example a file path as text/plain) breaks this and must be re-tested.
- clipboard.rs:160-163 and 131: DROPFILES fields must be exact (pFiles = header size, pt=(0,0), fNC=false, fWide=true), and the list needs a double NUL. Without it Explorer reads past the buffer.
- clipboard.rs:140: paths go through to_string_lossy, so non-UTF-16-representable names are altered. Unreachable from the peer because names are sanitized first, but relevant to any port that passes local paths.
- secret.rs:42-48 and 73-79: the library buffer is freed only on the success path after copying. Some DPAPI failure modes may still allocate output; that output is not freed on the error branch. A theoretical leak.
- secret.rs:80: unprotect requires the plaintext to be valid UTF-8; otherwise it returns None and auto-login fails silently.
- secret.rs:1-13 is the threat model: protection is against file exposure only, not against code running as the same user. A Keychain or keyfile port must keep that statement.
- autostart.rs:150: reconcile compares the stored command string exactly, including quoting and the --autostart suffix. Changing either re-registers every existing install on next start.
- autostart.rs:113 and 150: if current_exe fails, desired_command returns None and reconcile logs a warning; it never removes the stale entry.
- autostart.rs:54-68: the value is read in two calls (size, then data), so it is not atomic. Harmless at logon.
- autostart.rs:70-80: UTF-16 is decoded up to the first NUL and the value type (kind) is read but never checked, so a non-REG_SZ value is decoded as if it were REG_SZ.
- autostart.rs:157-165: reconcile failures are logged (warn) and never shown in the UI; only the UI-initiated set_enabled surfaces errors.
- autostart.rs:189-199 (test) mutates the real HKCU Run key and restores the prior state. On a developer machine that is a side effect; the port's test must redirect to a temp home.
- transfer.rs:114-115 and 60: exists() is checked, then File::create is called without create_new, so a file created in between is truncated. Use create_new (O_EXCL / CREATE_NEW).
- transfer.rs:122-128: the dedup loop stops at n=9999 and falls back to '<stem> (dup).<ext>' without an existence check, so that name can be overwritten.
- transfer.rs:74-76: write() returns Err on overflow but does not remove the file. Incoming has no Drop impl (transfer.rs:31-108), so dropping it without abort leaves the partial file on disk. The pipeline's error paths must call abort (not verified here).
- transfer.rs:102: abort() ignores remove_file errors; on Windows antivirus or indexers can keep the file open, leaving a partial file in the receive folder.
- transfer.rs:116-120: a name such as 'a.' splits to stem 'a' and extension '.', producing 'a (2).' on collision. Edge case worth a test.
- transfer.rs:24-27: if USERPROFILE is unset the folder falls back to temp_dir, which is shared between users on a machine. HOME is the POSIX equivalent and is always set in a login session, so the fallback matters only for headless runs.

## tests
- secret.rs:106-113 roundtrips_through_dpapi: protect then unprotect round-trips, and the sealed hex must not contain the plaintext. Windows-only (DPAPI). The contract (round-trip, no plaintext in the sealed form) is portable.
- secret.rs:115-119 rejects_garbage: unprotect('not-hex') and unprotect('00112233') are both None. Portable as a contract; the second input needs the port's decrypt to reject arbitrary bytes.
- secret.rs:121-125 hex_roundtrip: pure, OS-agnostic, portable as is.
- autostart.rs:174-183 desired_command_is_quoted_and_flagged: asserts a leading quote, the flag at the end, and that the closing quote precedes the flag. Portable as a contract; the quoting rule is Windows-specific, and POSIX needs a different encoding (plist array or desktop Exec).
- autostart.rs:185-201 enable_then_disable_roundtrips: mutates the real HKCU Run key, returns early on a locked registry, and restores prior state. Windows-only; a port needs an isolated home directory.
- clipboard.rs:193-206 file_list_roundtrips_as_a_drop_list: writes a temp file, calls set_files, asserts the sequence matches and get_text is None, then deletes the file. Windows-only; the 'file list is not text' assertion must be restated per backend.
- clipboard.rs:208-211 empty_file_list_is_rejected: set_files(&[]) is None, decided before any OS call (clipboard.rs:135). Portable as is.
- clipboard.rs:213-217 sequence_is_readable: smoke test only. Portable.
- clipboard.rs:219-230 roundtrips_text_and_reports_new_sequence: round-trips a non-ASCII marker and asserts the returned sequence equals sequence(). The equality is the echo-guard contract and must hold on any backend that claims a counter. Windows-only; returns early if locked.
- clipboard.rs:232-238 empty_text_is_not_reported: set_text('') then get_text is None when the set succeeded. Portable as a contract.
- transfer.rs:151-160 writes_a_file_and_reports_path: write, finish, read back. OS-agnostic, portable as is.
- transfer.rs:162-172 traversal_name_stays_inside_the_directory: uses a backslash name. Passes on Windows where backslash is a separator. On POSIX the name is a single component and must be checked, or the test rewritten with both separators.
- transfer.rs:174-186 never_overwrites_an_existing_file: expects 'a (2).txt'. OS-agnostic.
- transfer.rs:188-194 rejects_more_bytes_than_declared: writing 5 bytes against a declared 4 errors. OS-agnostic.
- transfer.rs:196-201 rejects_absurd_declared_size: MAX_FILE_BYTES+1 is refused at begin. OS-agnostic.
- transfer.rs:203-208 rejects_unusable_name: '..' is refused. OS-agnostic.
- transfer.rs:210-219 truncated_transfer_is_discarded: 3 of 10 bytes, finish errors and the partial file is gone. OS-agnostic; relies on finish calling abort.
