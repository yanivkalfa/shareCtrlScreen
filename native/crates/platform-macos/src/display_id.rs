//! Stable display ids for the `shareDisplayId` setting.
//!
//! A `CGDirectDisplayID` is not stable: macOS may hand an external display a
//! different id after a reboot, a re-plug or a GPU switch. The EDID identity
//! (vendor, model, serial — `CGDisplayVendorNumber` / `ModelNumber` /
//! `SerialNumber`) is, so ids are `"cg:<vendor>-<model>-<serial>"` in hex.
//! Two displays with the same identity (identical monitors without an EDID
//! serial) get `#2`, `#3`… in `CGDisplayUnitNumber` order, which follows the
//! physical connection and survives restarts while the cabling is unchanged —
//! the same promise the Windows `dxgi:<n>` ids make.
//!
//! OS-independent so the rules are unit-tested on every CI job.

const PREFIX: &str = "cg:";

/// The EDID identity of one display, as CoreGraphics reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DisplayIdentity {
    pub vendor: u32,
    pub model: u32,
    pub serial: u32,
}

/// Ids for `displays`, which the caller lists in `CGDisplayUnitNumber` order.
pub(crate) fn assign_ids(displays: &[DisplayIdentity]) -> Vec<String> {
    displays
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let base = format!("{PREFIX}{:x}-{:x}-{:x}", d.vendor, d.model, d.serial);
            let ordinal = displays[..i].iter().filter(|o| *o == d).count();
            if ordinal == 0 {
                base
            } else {
                format!("{base}#{}", ordinal + 1)
            }
        })
        .collect()
}

/// Whether `id` has the shape [`assign_ids`] produces. Ids saved by other
/// builds (Windows `dxgi:<n>`, Electron-era numeric ids) are rejected, so the
/// caller can say "not a macOS display" instead of "display unplugged".
pub(crate) fn is_mac_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix(PREFIX) else {
        return false;
    };
    let (key, ordinal) = match rest.split_once('#') {
        Some((k, n)) => (k, Some(n)),
        None => (rest, None),
    };
    let hex = |s: &str| !s.is_empty() && u32::from_str_radix(s, 16).is_ok();
    let parts: Vec<&str> = key.split('-').collect();
    parts.len() == 3
        && parts.iter().all(|p| hex(p))
        && ordinal.is_none_or(|n| n.parse::<u32>().is_ok_and(|n| n >= 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUILTIN: DisplayIdentity = DisplayIdentity {
        vendor: 0x610,
        model: 0xa050,
        serial: 0,
    };
    const DELL: DisplayIdentity = DisplayIdentity {
        vendor: 0x10ac,
        model: 0x41a2,
        serial: 0x3132_4c4b,
    };
    const NO_SERIAL: DisplayIdentity = DisplayIdentity {
        vendor: 0x4c2d,
        model: 0x7104,
        serial: 0,
    };

    #[test]
    fn ids_are_the_edid_identity_and_duplicates_are_numbered() {
        let ids = assign_ids(&[BUILTIN, NO_SERIAL, DELL, NO_SERIAL, NO_SERIAL]);
        assert_eq!(
            ids,
            vec![
                "cg:610-a050-0",
                "cg:4c2d-7104-0",
                "cg:10ac-41a2-31324c4b",
                "cg:4c2d-7104-0#2",
                "cg:4c2d-7104-0#3",
            ]
        );
        // Adding a different display does not renumber the existing ones.
        assert_eq!(assign_ids(&[BUILTIN, DELL])[1], ids[2]);
    }

    #[test]
    fn produced_ids_parse_and_foreign_ones_do_not() {
        for id in assign_ids(&[BUILTIN, NO_SERIAL, NO_SERIAL]) {
            assert!(is_mac_id(&id), "{id}");
        }
        for foreign in [
            "dxgi:0",
            "2528732444",
            "",
            "cg:",
            "cg:1-2",
            "cg:1-2-3-4",
            "cg:1-2-zz",
            "cg:1-2-3#1",
            "cg:1-2-3#x",
            "cg:1--3",
        ] {
            assert!(!is_mac_id(foreign), "{foreign}");
        }
    }
}
