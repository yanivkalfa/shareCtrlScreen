//! The SPA pods the stream negotiates with: `EnumFormat` at connect, then `Buffers` and
//! `Meta` once a `Format` is fixed. Serialised with libspa's pure-Rust pod serializer,
//! so they are unit-tested by parsing them back.
//!
//! * `EnumFormat` has no `SPA_FORMAT_VIDEO_modifier` property. A producer only
//!   allocates DMA-BUFs for a consumer that lists modifiers (docs.pipewire.org "DMA-BUF
//!   sharing"), so this keeps every compositor on shared memory, the v1 path the plan
//!   locks (plans/02 §1: MemFd first, DMA-BUF is stretch task L1b).
//! * `Buffers.dataType` = MemPtr | MemFd, for the same reason and as a second guard.
//! * `Meta`: Header (corrupted-frame flag), Cursor (pointer position + sprite for
//!   `cursor_mode = Metadata`) and VideoDamage. The sizes mirror what Mutter offers
//!   and OBS requests; a producer that has no such meta simply never attaches it.

use std::io::Cursor;
use std::mem::size_of;

use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{Object, Property, PropertyFlags, Value};
use pipewire::spa::sys;
use pipewire::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes};

use super::pixels::PREFERRED;

/// Largest frame side offered. Above any monitor PipeWire screen-cast produces today
/// (8K is 7680) and small enough that every size computation stays far inside `i32`.
pub(crate) const MAX_SIDE: u32 = 16384;

/// The host never encodes faster than this (engine `tuning::MAX_FPS`).
const MAX_FPS: u32 = 60;

/// Damage rectangles per buffer we make room for (matches `damage::MAX_RECTS`).
pub(crate) const DAMAGE_REGIONS: usize = 16;

/// Cursor sprite sides we make room for: default and maximum. 64 covers 1× cursors;
/// HiDPI sprites (2×, 3×) fit within the maximum.
const CURSOR_DEFAULT: usize = 64;
const CURSOR_MAX: usize = 1024;

/// `CURSOR_META_SIZE(w, h)` from the producers: the cursor header, the bitmap header
/// that follows it, and `w × h` 32-bit pixels.
fn cursor_meta_size(side: usize) -> i32 {
    let bytes =
        size_of::<sys::spa_meta_cursor>() + size_of::<sys::spa_meta_bitmap>() + side * side * 4;
    // At most 28 + 20 + 4 MiB: fits an i32 by construction.
    bytes as i32
}

fn serialize(object: Object) -> Result<Vec<u8>, String> {
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
        .map(|(out, _)| out.into_inner())
        .map_err(|e| format!("cannot build a PipeWire parameter: {e:?}"))
}

fn id(v: u32) -> Value {
    Value::Id(Id(v))
}

fn int_range(default: i32, min: i32, max: i32) -> Value {
    Value::Choice(pipewire::spa::pod::ChoiceValue::Int(Choice(
        ChoiceFlags::empty(),
        ChoiceEnum::Range { default, min, max },
    )))
}

/// `EnumFormat`: raw video in one of [`PREFERRED`], any size, up to [`MAX_FPS`].
pub(crate) fn enum_format() -> Result<Vec<u8>, String> {
    let formats: Vec<Id> = PREFERRED.iter().map(|f| Id(f.as_raw())).collect();
    serialize(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: vec![
            Property::new(
                FormatProperties::MediaType.as_raw(),
                id(MediaType::Video.as_raw()),
            ),
            Property::new(
                FormatProperties::MediaSubtype.as_raw(),
                id(MediaSubtype::Raw.as_raw()),
            ),
            Property::new(
                FormatProperties::VideoFormat.as_raw(),
                Value::Choice(pipewire::spa::pod::ChoiceValue::Id(Choice(
                    ChoiceFlags::empty(),
                    // SPA enum choices list the default first and then every
                    // alternative, the default included.
                    ChoiceEnum::Enum {
                        default: formats[0],
                        alternatives: formats.clone(),
                    },
                ))),
            ),
            Property::new(
                FormatProperties::VideoSize.as_raw(),
                Value::Choice(pipewire::spa::pod::ChoiceValue::Rectangle(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: Rectangle {
                            width: 1920,
                            height: 1080,
                        },
                        min: Rectangle {
                            width: 1,
                            height: 1,
                        },
                        max: Rectangle {
                            width: MAX_SIDE,
                            height: MAX_SIDE,
                        },
                    },
                ))),
            ),
            Property::new(
                FormatProperties::VideoFramerate.as_raw(),
                Value::Choice(pipewire::spa::pod::ChoiceValue::Fraction(Choice(
                    ChoiceFlags::empty(),
                    // 0/1 = variable rate: screen casts only send frames on change.
                    ChoiceEnum::Range {
                        default: Fraction {
                            num: MAX_FPS,
                            denom: 1,
                        },
                        min: Fraction { num: 0, denom: 1 },
                        max: Fraction {
                            num: MAX_FPS,
                            denom: 1,
                        },
                    },
                ))),
            ),
        ],
    })
}

fn meta(kind: u32, size: Value) -> Result<Vec<u8>, String> {
    serialize(Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: ParamType::Meta.as_raw(),
        properties: vec![
            Property::new(sys::SPA_PARAM_META_type, id(kind)),
            Property::new(sys::SPA_PARAM_META_size, size),
        ],
    })
}

/// The `Buffers` and `Meta` parameters sent with `update_params` once the format is
/// fixed.
pub(crate) fn buffers_and_meta() -> Result<Vec<Vec<u8>>, String> {
    let shm = (1i32 << sys::SPA_DATA_MemPtr) | (1i32 << sys::SPA_DATA_MemFd);
    let buffers = serialize(Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![Property {
            key: sys::SPA_PARAM_BUFFERS_dataType,
            flags: PropertyFlags::empty(),
            value: Value::Int(shm),
        }],
    })?;
    let region = size_of::<sys::spa_meta_region>() as i32;
    Ok(vec![
        buffers,
        meta(
            sys::SPA_META_Header,
            Value::Int(size_of::<sys::spa_meta_header>() as i32),
        )?,
        meta(
            sys::SPA_META_Cursor,
            int_range(
                cursor_meta_size(CURSOR_DEFAULT),
                cursor_meta_size(1),
                cursor_meta_size(CURSOR_MAX),
            ),
        )?,
        meta(
            sys::SPA_META_VideoDamage,
            int_range(
                region * DAMAGE_REGIONS as i32,
                region,
                region * DAMAGE_REGIONS as i32,
            ),
        )?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use pipewire::spa::pod::deserialize::PodDeserializer;
    use pipewire::spa::pod::{ChoiceValue, Pod};

    fn parse(bytes: &[u8]) -> Object {
        assert!(Pod::from_bytes(bytes).is_some(), "not a pod");
        match PodDeserializer::deserialize_any_from(bytes) {
            Ok((_, Value::Object(o))) => o,
            other => panic!("not an object: {other:?}"),
        }
    }

    fn prop(o: &Object, key: u32) -> &Value {
        &o.properties
            .iter()
            .find(|p| p.key == key)
            .unwrap_or_else(|| panic!("no property {key}"))
            .value
    }

    #[test]
    fn enum_format_offers_bgr_first_shared_memory_only() {
        let o = parse(&enum_format().unwrap());
        assert_eq!(o.type_, SpaTypes::ObjectParamFormat.as_raw());
        assert_eq!(o.id, ParamType::EnumFormat.as_raw());
        assert_eq!(
            prop(&o, FormatProperties::MediaType.as_raw()),
            &id(MediaType::Video.as_raw())
        );
        let Value::Choice(ChoiceValue::Id(Choice(
            _,
            ChoiceEnum::Enum {
                default,
                alternatives,
            },
        ))) = prop(&o, FormatProperties::VideoFormat.as_raw())
        else {
            panic!("format is not an enum choice");
        };
        assert_eq!(default.0, PREFERRED[0].as_raw());
        assert_eq!(alternatives.len(), PREFERRED.len());
        // No modifier property: a producer then never picks DMA-BUF.
        assert!(o
            .properties
            .iter()
            .all(|p| p.key != sys::SPA_FORMAT_VIDEO_modifier));
    }

    #[test]
    fn buffers_ask_for_shared_memory_and_three_metas() {
        let params = buffers_and_meta().unwrap();
        assert_eq!(params.len(), 4);
        let buffers = parse(&params[0]);
        let Value::Int(mask) = prop(&buffers, sys::SPA_PARAM_BUFFERS_dataType) else {
            panic!("dataType is not an int");
        };
        assert_eq!(*mask & (1 << sys::SPA_DATA_DmaBuf), 0);
        assert_ne!(*mask & (1 << sys::SPA_DATA_MemFd), 0);
        let kinds: Vec<Value> = params[1..]
            .iter()
            .map(|p| prop(&parse(p), sys::SPA_PARAM_META_type).clone())
            .collect();
        assert_eq!(
            kinds,
            vec![
                id(sys::SPA_META_Header),
                id(sys::SPA_META_Cursor),
                id(sys::SPA_META_VideoDamage)
            ]
        );
    }

    #[test]
    fn cursor_meta_size_matches_the_producers_macro() {
        // CURSOR_META_SIZE(64, 64) in Mutter / xdg-desktop-portal-kde.
        assert_eq!(cursor_meta_size(64), 28 + 20 + 64 * 64 * 4);
    }
}
