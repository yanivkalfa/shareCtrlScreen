//! Generates `platform-api/src/keymap.rs` from Chromium's `dom_code_data.inc`
//! (plan 04 §6, task D3).
//!
//! Chromium keeps one row per physical key: USB HID usage, Linux evdev code,
//! XKB code, Windows scancode (E0-prefixed ones as `0xe0xx`), macOS `kVK_*`
//! and the DOM `KeyboardEvent.code` string. This tool reads that file and
//! writes the Rust table every OS backend uses to translate between the DOM
//! codes on the wire and its native key codes.
//!
//! ## Usage
//!
//! ```text
//! cd native
//! cargo run -p gen-keymap                      # data/dom_code_data.inc -> keymap.rs
//! cargo run -p gen-keymap -- --input FILE --output FILE
//! ```
//!
//! The input is committed at `data/dom_code_data.inc`, byte for byte as
//! served at [`CHROMIUM_COMMIT`]. To move to a newer Chromium, download
//!
//! ```text
//! https://chromium.googlesource.com/chromium/src/+/<commit>/ui/events/keycodes/dom/dom_code_data.inc?format=TEXT
//! ```
//!
//! (the body is base64), decode it over `data/dom_code_data.inc`, update
//! [`CHROMIUM_COMMIT`], run the tool and commit all three files. The
//! `committed_output_is_current` test fails whenever the three disagree.
//!
//! ## Which rows become keys
//!
//! Only USB usage page 0x07 (keyboard/keypad) rows that have a DOM code and a
//! native code on at least one OS. Everything else Chromium lists (consumer
//! page, browser keys, ChromeOS top row, rows with a `NULL` code) is not a key
//! a remote desktop should inject. On top of that, [`EXCLUDED`] keeps the
//! deliberate omissions of the old hand-written Set-1 table (Pause, media
//! keys) and adds the power key and the Sun-style edit keys, which no PC or
//! Mac keyboard has and whose Windows codes are multimedia-keyboard scancodes.
//!
//! ## References
//!
//! * Chromium `ui/events/keycodes/dom/dom_code_data.inc`, whose header notes
//!   explain the platform quirks this tool relies on (W1: `Lang5`/`F24`
//!   share Windows scancode 0x76; M1: `Insert`/`Help` share `kVK_Help`; L1/L2:
//!   Linux has no `NumpadClear`, `ContextMenu` is `KEY_COMPOSE`).
//!   <https://chromium.googlesource.com/chromium/src/+/main/ui/events/keycodes/dom/dom_code_data.inc>
//! * W3C UI Events KeyboardEvent `code` Values, the DOM strings and which
//!   physical key each names. <https://www.w3.org/TR/uievents-code/>
//! * Linux `include/uapi/linux/input-event-codes.h`, the evdev numbering
//!   (`KEY_A` = 30 ... `KEY_KPENTER` = 96).
//! * Apple `HIToolbox/Events.h`, the `kVK_*` virtual key codes, which name
//!   key positions of the ANSI layout rather than characters.
//! * USB HID Usage Tables, section 10 "Keyboard/Keypad Page (0x07)".
//! * Microsoft "Keyboard Scan Code Specification" (Set 1 make codes; the
//!   `E0` prefix marks the extended keys).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

/// The Chromium revision `data/dom_code_data.inc` was downloaded at.
const CHROMIUM_COMMIT: &str = "18d83271faee745fdf6e8c25b129f91dc944146e";

/// The generated file's skeleton: header, types, lookups and self-checks.
/// `@COMMIT@` and the `@...@` blocks are replaced with the generated data.
const TEMPLATE: &str = include_str!("keymap.rs.in");

/// Chromium's marker for "this OS has no code for the key", per column. Zero
/// is a real code on macOS (`kVK_ANSI_A`), so macOS needs its own value.
const NO_SET1: u16 = 0;
const NO_EVDEV: u16 = 0;
const NO_MAC: u16 = 0xffff;

/// Chromium writes extended Windows scancodes as `0xe0xx`.
const EXTENDED_PREFIX: u16 = 0xe000;

/// USB usage page of the keyboard/keypad keys, the only page we generate.
const KEYBOARD_PAGE: u32 = 0x07;

/// Keys that stay out of the table even though Chromium lists them. A key
/// outside the table is `None` on every OS, which is how the old Set-1 table
/// treated Pause, media keys and Fn.
const EXCLUDED: &[&str] = &[
    // Pause is E1 1D 45 on the wire and cannot be sent as one scancode.
    "Pause",
    // Volume keys are media keys (Chromium lists them on the keyboard page).
    "AudioVolumeMute",
    "AudioVolumeUp",
    "AudioVolumeDown",
    // A remote peer must not be able to power the machine off.
    "Power",
    // Sun/ChromeOS-style edit keys: no PC or Mac keyboard has them, and the
    // Windows codes Chromium records for them are multimedia-keyboard ones.
    "Open",
    "Help",
    "Select",
    "Again",
    "Undo",
    "Cut",
    "Copy",
    "Paste",
    "Find",
    "Props",
];

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("malformed DOM_CODE entry `{entry}`: {reason}")]
    Malformed { entry: String, reason: &'static str },
    #[error("{column} {value} is used by both {first} and {second}")]
    Duplicate {
        column: &'static str,
        value: String,
        first: String,
        second: String,
    },
    #[error("the table has {0} keys; the generated indices are u8")]
    TooManyKeys(usize),
    #[error("rustfmt failed: {0}")]
    Rustfmt(String),
    #[error("usage: gen-keymap [--input FILE] [--output FILE]")]
    Usage,
}

/// One `DOM_CODE(...)` row of the input.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    usb: u32,
    evdev: u16,
    win: u16,
    mac: u16,
    dom: Option<String>,
}

/// One key of the generated table.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    usb: u16,
    dom: String,
    set1: u16,
    evdev: u16,
    mac: u16,
}

fn malformed(entry: &str, reason: &'static str) -> Error {
    Error::Malformed {
        entry: entry.trim().to_owned(),
        reason,
    }
}

fn parse_hex(entry: &str, field: &str) -> Result<u32, Error> {
    field
        .trim()
        .strip_prefix("0x")
        .and_then(|digits| u32::from_str_radix(digits, 16).ok())
        .ok_or_else(|| malformed(entry, "a numeric column is not a 0x… hex number"))
}

fn parse_hex16(entry: &str, field: &str) -> Result<u16, Error> {
    u16::try_from(parse_hex(entry, field)?)
        .map_err(|_| malformed(entry, "a scancode column does not fit in 16 bits"))
}

/// Reads every active `DOM_CODE(usb, evdev, xkb, win, mac, code, NAME)` row.
///
/// Rows are free-form C: they wrap across lines, and commented-out rows
/// (`//DOM_CODE(...)`) document unused codes. Dropping `//` comments first
/// handles both, since no field contains `//`.
fn parse(source: &str) -> Result<Vec<Row>, Error> {
    let code: String = source
        .lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ");
    let mut rows = Vec::new();
    let mut rest = code.as_str();
    while let Some(start) = rest.find("DOM_CODE(") {
        let body = &rest[start + "DOM_CODE(".len()..];
        let end = body
            .find(')')
            .ok_or_else(|| malformed(body, "missing closing parenthesis"))?;
        let entry = &body[..end];
        let fields: Vec<&str> = entry.split(',').collect();
        let [usb, evdev, _xkb, win, mac, dom, _name] = fields[..] else {
            return Err(malformed(entry, "expected 7 comma-separated columns"));
        };
        let dom = match dom.trim() {
            "NULL" => None,
            quoted => Some(
                quoted
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .ok_or_else(|| malformed(entry, "the code column is neither NULL nor quoted"))?
                    .to_owned(),
            ),
        };
        rows.push(Row {
            usb: parse_hex(entry, usb)?,
            evdev: parse_hex16(entry, evdev)?,
            win: parse_hex16(entry, win)?,
            mac: parse_hex16(entry, mac)?,
            dom,
        });
        rest = &body[end..];
    }
    Ok(rows)
}

/// Applies the selection rules from the module docs and sorts by USB usage.
fn select(rows: &[Row]) -> Result<Vec<Key>, Error> {
    let mut keys: Vec<Key> = rows
        .iter()
        .filter(|row| row.usb >> 16 == KEYBOARD_PAGE)
        .filter(|row| row.win != NO_SET1 || row.evdev != NO_EVDEV || row.mac != NO_MAC)
        .filter_map(|row| {
            let dom = row.dom.as_deref()?;
            if EXCLUDED.contains(&dom) {
                return None;
            }
            Some(Key {
                // Within page 0x07 the usage fits in 16 bits (the masked value
                // is at most 0xffff by construction).
                usb: (row.usb & 0xffff) as u16,
                dom: dom.to_owned(),
                set1: row.win,
                evdev: row.evdev,
                mac: row.mac,
            })
        })
        .collect();
    keys.sort_by_key(|key| key.usb);

    // Every inverse lookup is only well defined if a native code names one key.
    check_unique("usb", &keys, |key| Some(key.usb))?;
    check_unique("dom", &keys, |key| Some(key.dom.clone()))?;
    check_unique("set1", &keys, |key| {
        (key.set1 != NO_SET1).then_some(key.set1)
    })?;
    check_unique("evdev", &keys, |key| {
        (key.evdev != NO_EVDEV).then_some(key.evdev)
    })?;
    check_unique("mac", &keys, |key| (key.mac != NO_MAC).then_some(key.mac))?;

    if keys.len() > usize::from(u8::MAX) {
        return Err(Error::TooManyKeys(keys.len()));
    }
    for key in &keys {
        let prefix = key.set1 & 0xff00;
        if prefix != 0 && prefix != EXTENDED_PREFIX {
            return Err(malformed(
                &key.dom,
                "Windows scancode has a prefix other than E0",
            ));
        }
    }
    Ok(keys)
}

fn check_unique<K: Ord + std::fmt::Debug>(
    column: &'static str,
    keys: &[Key],
    value: impl Fn(&Key) -> Option<K>,
) -> Result<(), Error> {
    let mut seen: BTreeMap<K, &str> = BTreeMap::new();
    for key in keys {
        let Some(value) = value(key) else { continue };
        let shown = format!("{value:?}");
        if let Some(first) = seen.insert(value, &key.dom) {
            return Err(Error::Duplicate {
                column,
                value: shown,
                first: first.to_owned(),
                second: key.dom.clone(),
            });
        }
    }
    Ok(())
}

/// A `static` array of `(native code, index into TABLE)` sorted by native
/// code, for binary search.
fn index_array(
    name: &str,
    keys: &[Key],
    native: impl Fn(&Key) -> Option<u16>,
    out: &mut String,
) -> Result<(), std::fmt::Error> {
    let mut entries: Vec<(u16, usize)> = keys
        .iter()
        .enumerate()
        .filter_map(|(index, key)| Some((native(key)?, index)))
        .collect();
    entries.sort_unstable();
    writeln!(out, "static {name}: [(u16, u8); {}] = [", entries.len())?;
    for (code, index) in entries {
        writeln!(out, "({code:#06x}, {index}),")?;
    }
    writeln!(out, "];")
}

fn render(keys: &[Key]) -> Result<String, std::fmt::Error> {
    let mut table = String::new();
    for key in keys {
        writeln!(
            table,
            "Key {{ usb: {:#06x}, dom: {:?}, set1: {:#06x}, evdev: {:#06x}, mac: {:#06x} }},",
            key.usb, key.dom, key.set1, key.evdev, key.mac
        )?;
    }

    let mut by_dom: Vec<(&str, usize)> = keys
        .iter()
        .enumerate()
        .map(|(index, key)| (key.dom.as_str(), index))
        .collect();
    by_dom.sort_unstable();
    let mut indices = String::new();
    writeln!(indices, "static BY_DOM: [(&str, u8); {}] = [", by_dom.len())?;
    for (dom, index) in by_dom {
        writeln!(indices, "({dom:?}, {index}),")?;
    }
    writeln!(indices, "];")?;
    index_array(
        "BY_SET1",
        keys,
        |key| (key.set1 != NO_SET1).then_some(key.set1),
        &mut indices,
    )?;
    index_array(
        "BY_EVDEV",
        keys,
        |key| (key.evdev != NO_EVDEV).then_some(key.evdev),
        &mut indices,
    )?;
    index_array(
        "BY_MAC",
        keys,
        |key| (key.mac != NO_MAC).then_some(key.mac),
        &mut indices,
    )?;

    // Direct index by usage: the O(1) path for every `KeyId` → native lookup.
    let usb_len = keys.last().map_or(0, |key| usize::from(key.usb) + 1);
    let mut by_usb = vec![u8::MAX; usb_len];
    for (index, key) in keys.iter().enumerate() {
        // `select` guarantees fewer than 255 keys, so the index fits.
        by_usb[usize::from(key.usb)] = index as u8;
    }
    let mut usb = String::new();
    writeln!(usb, "static BY_USB: [u8; {usb_len}] = [")?;
    for index in by_usb {
        writeln!(usb, "{index},")?;
    }
    writeln!(usb, "];")?;

    Ok(TEMPLATE
        .replace("@COMMIT@", CHROMIUM_COMMIT)
        .replace("@TABLE@", &table)
        .replace("@INDICES@", &indices)
        .replace("@BY_USB@", &usb))
}

/// Pipes the source through `rustfmt` so the committed file is exactly what
/// `cargo fmt --check` expects.
fn format(source: &str) -> Result<String, Error> {
    let mut child = Command::new("rustfmt")
        .args(["--edition", "2021"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Rustfmt(e.to_string()))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::Rustfmt("no stdin".to_owned()))?;
    // Feed stdin from a thread: rustfmt may start writing before it has read
    // everything, and a full stdout pipe would otherwise deadlock us.
    let input = source.to_owned();
    let feeder = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let output = child
        .wait_with_output()
        .map_err(|e| Error::Rustfmt(e.to_string()))?;
    feeder
        .join()
        .map_err(|_| Error::Rustfmt("stdin writer panicked".to_owned()))?
        .map_err(|e| Error::Rustfmt(e.to_string()))?;
    if !output.status.success() {
        return Err(Error::Rustfmt(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    String::from_utf8(output.stdout).map_err(|e| Error::Rustfmt(e.to_string()))
}

/// The whole pipeline: `dom_code_data.inc` text in, formatted `keymap.rs` out.
fn generate(source: &str) -> Result<String, Error> {
    let keys = select(&parse(source)?)?;
    let rendered = render(&keys).map_err(|e| Error::Rustfmt(e.to_string()))?;
    format(&rendered)
}

fn default_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn run() -> Result<(), Error> {
    let mut input = default_path("data/dom_code_data.inc");
    let mut output = default_path("../../crates/platform-api/src/keymap.rs");
    let mut args = std::env::args_os().skip(1);
    while let Some(flag) = args.next() {
        let target = match flag.to_str() {
            Some("--input") => &mut input,
            Some("--output") => &mut output,
            _ => return Err(Error::Usage),
        };
        *target = PathBuf::from(args.next().ok_or(Error::Usage)?);
    }
    let source = std::fs::read_to_string(&input).map_err(|source| Error::Read {
        path: input.clone(),
        source,
    })?;
    let generated = generate(&source)?;
    std::fs::write(&output, generated).map_err(|source| Error::Write {
        path: output.clone(),
        source,
    })?;
    println!("wrote {}", output.display());
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("gen-keymap: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INC: &str = include_str!("../data/dom_code_data.inc");

    #[test]
    fn parses_wrapped_and_skips_commented_rows() {
        let rows = parse(
            "DOM_CODE(0x070004, 0x001e, 0x0026, 0x001e, 0x0000, \"KeyA\", US_A), // aA\n\
             //DOM_CODE(0x070032, 0x0000, 0x0000, 0x0000, 0xffff, NULL, INTL_HASH),\n\
             DOM_CODE(0x070055, 0x0037, 0x003f, 0x0037, 0x0043, \"NumpadMultiply\",\n\
             NUMPAD_MULTIPLY),",
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].dom.as_deref(), Some("KeyA"));
        assert_eq!(rows[1].dom.as_deref(), Some("NumpadMultiply"));
        assert_eq!(rows[1].mac, 0x43);
    }

    #[test]
    fn rejects_a_short_row() {
        assert!(matches!(
            parse("DOM_CODE(0x070004, 0x001e)"),
            Err(Error::Malformed { .. })
        ));
    }

    #[test]
    fn rejects_two_keys_sharing_a_native_code() {
        let row = |usb, dom: &str, evdev| Row {
            usb,
            evdev,
            win: 0,
            mac: NO_MAC,
            dom: Some(dom.to_owned()),
        };
        let err = select(&[row(0x070004, "KeyA", 30), row(0x070005, "KeyB", 30)]).unwrap_err();
        assert!(matches!(
            err,
            Error::Duplicate {
                column: "evdev",
                ..
            }
        ));
    }

    #[test]
    fn selection_drops_other_pages_exclusions_and_unmapped_rows() {
        let keys = select(&parse(INC).unwrap()).unwrap();
        let has = |dom: &str| keys.iter().any(|key| key.dom == dom);
        assert!(has("KeyA") && has("NumpadEnter") && has("ContextMenu"));
        for excluded in ["Pause", "Fn", "Power", "AudioVolumeUp", "MediaPlayPause"] {
            assert!(!has(excluded), "{excluded} must not be in the table");
        }
        assert!(!has("NumpadMemoryStore"), "no native code on any OS");
    }

    #[test]
    fn numlock_keeps_chromiums_extended_scancode() {
        // Plain 0x45 is Pause to Windows; NumLock must stay E0 45 (task W3).
        let keys = select(&parse(INC).unwrap()).unwrap();
        let numlock = keys.iter().find(|key| key.dom == "NumLock");
        assert_eq!(numlock.map(|key| key.set1), Some(0xe045));
    }

    #[test]
    fn committed_output_is_current() {
        let committed =
            std::fs::read_to_string(default_path("../../crates/platform-api/src/keymap.rs"))
                .unwrap();
        // Git may check the committed file out with CRLF on Windows.
        assert_eq!(
            generate(INC).unwrap(),
            committed.replace("\r\n", "\n"),
            "keymap.rs is stale: run `cargo run -p gen-keymap` from native/"
        );
    }
}
