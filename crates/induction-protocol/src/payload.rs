//! Opaque payloads: one CBOR document carried as BYTES, through a wire that is
//! otherwise `serde_json::Value`.
//!
//! The problem this solves is one sentence long: `serde_json::Value` has no
//! byte-string variant, so a peer that speaks CBOR cannot put real bytes in a
//! frame — and base64 is not a rounding error, it is a third of the payload.
//!
//! ## Two spellings of one thing
//!
//! An opaque payload has exactly TWO spellings, and which one a frame uses is
//! decided by the frame's CODEC, never by configuration:
//!
//! - **JSON** (`Encoding::Json`): `{"$cbor": "<base64>"}` — a map whose only key
//!   is [`CBOR_PAYLOAD_KEY`]. JSON has no binary, so text is the only option,
//!   and this is the spelling every host has read since the axis existed.
//! - **CBOR** (`Encoding::Cbor`): a **byte string** (`0x5a …`), the bytes
//!   themselves. No marker, no base64, no expansion.
//!
//! The marker is therefore not a wart: it is the JSON *mapping* of a bytes
//! field, the same way protobuf's JSON mapping spells `bytes` as base64 and its
//! binary form spells it as binary. [`cbor_frame`] performs that mapping on the
//! way out and [`split_bytes`] on the way in — one place each, so no second
//! copy of the rule can drift.
//!
//! ## The fallback is not optional
//!
//! A host whose payload type cannot hold bytes (every `Value`-typed host seam)
//! must still get a working frame, so the bytes can always be put back into the
//! JSON mapping where they sat: that is [`FrameBytes::embed`], and it reproduces
//! today's frame field for field. Losing the compactness of one hop is a cost;
//! losing the frame would be a bug.

use base64::Engine as _;
use serde_json::Value;

/// The key an opaque payload rides under in its JSON spelling: an object whose
/// ONLY key is this one holds base64 of a CBOR document.
pub const CBOR_PAYLOAD_KEY: &str = "$cbor";

/// The base64 alphabet, spelled once.
const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// The JSON spelling of one opaque payload: `{"$cbor": "<base64>"}`.
pub fn cbor_payload(bytes: &[u8]) -> Value {
    let mut payload = serde_json::Map::new();
    payload.insert(
        CBOR_PAYLOAD_KEY.to_string(),
        Value::String(BASE64.encode(bytes)),
    );
    Value::Object(payload)
}

/// The bytes a value stands for, when it is the JSON spelling: `Some` only for
/// an object whose ONLY key is [`CBOR_PAYLOAD_KEY`] and whose value is base64.
/// `None` is "not an opaque payload" — never an error, because that is the
/// majority shape on this wire.
pub fn payload_bytes(data: &Value) -> Option<Vec<u8>> {
    let object = data.as_object()?;
    if object.len() != 1 {
        return None;
    }
    BASE64
        .decode(object.get(CBOR_PAYLOAD_KEY)?.as_str()?)
        .ok()
}

/// One frame's opaque bytes: WHERE they sat and what they were.
///
/// The path is the chain of map keys from the frame's `args` down to the value
/// the bytes replaced, so a payload nested inside an envelope (gravity's
/// `{"name":…, "island":…, "data": <blob>}`) keeps its position when it is put
/// back. Empty path = the bytes WERE the `args` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameBytes {
    /// Map keys from `args` to the value the bytes occupied, outermost first.
    pub path: Vec<String>,
    /// The payload itself.
    pub bytes: Vec<u8>,
}

impl FrameBytes {
    /// A payload that occupied `path` inside the frame's `args`.
    pub fn new(path: Vec<String>, bytes: Vec<u8>) -> Self {
        Self { path, bytes }
    }

    /// The JSON spelling of this payload placed back where the bytes sat —
    /// `false` when the path no longer resolves to a value in `data` (a frame
    /// whose shape changed under us; the caller states that rather than
    /// silently dropping a payload).
    pub fn embed(&self, data: &mut Value) -> bool {
        // The path is absolute, recorded against the whole frame, while callers hand us the
        // payload the path lives IN (`emit`'s `data`), so the leading components — the routing
        // head — are already gone by the time the bytes are put back. Try the path as given
        // first, then each shorter suffix: the first one that resolves is the payload's base.
        for skip in 0..self.path.len() {
            if let Some(cursor) = walk(data, &self.path[skip..]) {
                *cursor = cbor_payload(&self.bytes);
                return true;
            }
        }
        false
    }
}

/// The value at `path`, if every component resolves.
fn walk<'a>(data: &'a mut Value, path: &[String]) -> Option<&'a mut Value> {
    let mut cursor = data;
    for key in path {
        cursor = cursor.get_mut(key.as_str())?;
    }
    Some(cursor)
}

// ------------------------------------------------------------ the codec --

/// One CBOR frame document as the wire's two halves: the JSON-shaped value and
/// the opaque bytes it carried, if any.
///
/// CBOR is a superset of JSON's data model, so every frame that existed before
/// bytes existed maps 1:1 — the byte string is the one variant with no JSON
/// counterpart, and it is lifted OUT of the tree (leaving `null` where it sat)
/// instead of failing the decode, which is what a `Value`-typed reader does
/// today.
///
/// Two shapes are refused, and both were refused before this function existed:
/// a non-text map key (JSON objects are string-keyed) and an integer outside
/// JSON's range. A TAG is its content — CBOR's tag is transport metadata, not
/// data, and the JSON data model has no place for it.
pub fn split_bytes(doc: ciborium::Value) -> Result<(Value, Option<FrameBytes>), String> {
    let mut lifted: Option<FrameBytes> = None;
    let mut path: Vec<String> = Vec::new();
    let value = to_json(doc, &mut path, &mut lifted)?;
    Ok((value, lifted))
}

fn to_json(
    value: ciborium::Value,
    path: &mut Vec<String>,
    lifted: &mut Option<FrameBytes>,
) -> Result<Value, String> {
    use ciborium::Value as Cbor;
    Ok(match value {
        Cbor::Null => Value::Null,
        Cbor::Bool(b) => Value::Bool(b),
        Cbor::Integer(i) => i64::try_from(i)
            .map(Value::from)
            .or_else(|_| u64::try_from(i).map(Value::from))
            .map_err(|_| format!("integer {i:?} is outside JSON's range"))?,
        Cbor::Float(f) => serde_json::Number::from_f64(f)
            .map(Value::Number)
            .ok_or_else(|| format!("float {f} has no JSON form"))?,
        Cbor::Text(t) => Value::String(t),
        Cbor::Bytes(bytes) => {
            if lifted.is_some() {
                return Err("one frame may carry at most one byte payload".to_string());
            }
            *lifted = Some(FrameBytes::new(path.clone(), bytes));
            // The hole the payload leaves: `null`, so the JSON shape stays
            // walkable (a host that reads the path still finds a value there).
            Value::Null
        }
        Cbor::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| to_json(item, path, lifted))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Cbor::Map(entries) => {
            let mut object = serde_json::Map::new();
            for (key, item) in entries {
                let Cbor::Text(key) = key else {
                    return Err("map key is not text (JSON objects are string-keyed)".to_string());
                };
                path.push(key.clone());
                let item = to_json(item, path, lifted);
                path.pop();
                object.insert(key, item?);
            }
            Value::Object(object)
        }
        // A tag is transport metadata (a content type, a big-number marker):
        // the JSON data model has no counterpart, so the VALUE is what it means.
        Cbor::Tag(_, inner) => to_json(*inner, path, lifted)?,
        // `Value` is `#[non_exhaustive]`: a variant this build does not know is a
        // decoder older than its peer, and saying so beats guessing.
        other => return Err(format!("unsupported CBOR value: {other:?}")),
    })
}

/// One JSON-shaped frame as a CBOR document, with every `{"$cbor": "<base64>"}`
/// mapping replaced by the byte string it stands for.
///
/// This is the WRITE half of [`split_bytes`], and the reason a guest needs no
/// new API to send real bytes: the payload it already builds (`cbor_payload`,
/// the JSON mapping) becomes a byte string on a CBOR link and stays text on a
/// JSON one. A marker whose base64 does not decode is left exactly as the
/// caller wrote it — the writer is not the place to swallow a malformed
/// payload.
pub fn cbor_frame(frame: &Value) -> Result<ciborium::Value, String> {
    let mut doc = ciborium::Value::serialized(frame)
        .map_err(|e| format!("frame is not encodable as CBOR: {e}"))?;
    substitute(&mut doc);
    Ok(doc)
}

/// Does this value contain the JSON mapping anywhere? The writer asks first, so
/// a frame with no payload pays nothing but the scan (no tree rebuild).
pub fn carries_payload(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            if map.len() == 1
                && map
                    .get(CBOR_PAYLOAD_KEY)
                    .is_some_and(|v| v.as_str().is_some_and(|s| BASE64.decode(s).is_ok()))
            {
                return true;
            }
            map.values().any(carries_payload)
        }
        Value::Array(items) => items.iter().any(carries_payload),
        _ => false,
    }
}

fn substitute(value: &mut ciborium::Value) {
    use ciborium::Value as Cbor;
    match value {
        Cbor::Map(entries) => {
            let mapping = match entries.as_slice() {
                [(Cbor::Text(key), Cbor::Text(text))] if key == CBOR_PAYLOAD_KEY => {
                    BASE64.decode(text).ok()
                }
                _ => None,
            };
            if let Some(bytes) = mapping {
                *value = Cbor::Bytes(bytes);
                return;
            }
            for (_, item) in entries.iter_mut() {
                substitute(item);
            }
        }
        Cbor::Array(items) => {
            for item in items.iter_mut() {
                substitute(item);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The mapping round-trips: bytes → JSON spelling → bytes.
    #[test]
    fn the_json_spelling_round_trips() {
        let bytes = vec![0u8, 1, 2, 254, 255];
        let spelled = cbor_payload(&bytes);
        assert_eq!(spelled, json!({ "$cbor": "AAEC/v8=" }));
        assert_eq!(payload_bytes(&spelled), Some(bytes));
    }

    /// Only the exact shape is a payload: the marker beside a sibling key, or a
    /// non-base64 value, is ordinary data.
    #[test]
    fn only_the_exact_shape_is_a_payload() {
        assert_eq!(payload_bytes(&json!({ "$cbor": "AA==", "x": 1 })), None);
        assert_eq!(payload_bytes(&json!({ "$cbor": "not base64!" })), None);
        assert_eq!(payload_bytes(&json!([1, 2, 3])), None);
        assert_eq!(payload_bytes(&json!("AA==")), None);
    }

    /// A frame that carries the mapping writes a REAL BYTE STRING, at the same
    /// position, and decodes back to the bytes — no base64 on the CBOR wire.
    #[test]
    fn a_cbor_frame_carries_the_bytes_not_the_base64() {
        let content = vec![0xa1u8, 0x61, 0x78, 0x01];
        let frame = json!({
            "host": { "type": "emit", "args": {
                "event": "ui:turn.done",
                "data": { "name": "ui:turn.done", "island": "i", "data": cbor_payload(&content) },
            }},
        });
        assert!(carries_payload(&frame));

        let doc = cbor_frame(&frame).expect("encodes");
        let nested = doc
            .as_map()
            .and_then(|m| m.into_iter().next())
            .and_then(|(_, host)| host.as_map())
            .and_then(|m| m.into_iter().find(|(k, _)| k.as_text() == Some("args")))
            .map(|(_, args)| args)
            .expect("host.args");
        let data = nested
            .as_map()
            .and_then(|m| m.into_iter().find(|(k, _)| k.as_text() == Some("data")))
            .map(|(_, v)| v)
            .expect("args.data");
        let inner = data
            .as_map()
            .and_then(|m| m.into_iter().find(|(k, _)| k.as_text() == Some("data")))
            .map(|(_, v)| v)
            .expect("envelope.data");
        assert_eq!(
            inner.as_bytes().map(Vec::as_slice),
            Some(content.as_slice()),
            "the payload is a CBOR byte string, not the base64 mapping: {inner:?}"
        );

        // And the read half lifts it back out, at its path.
        let (value, lifted) = split_bytes(doc).expect("splits");
        let lifted = lifted.expect("the frame carried bytes");
        assert_eq!(lifted.bytes, content);
        assert_eq!(lifted.path, vec!["host", "args", "data", "data"]);
        assert_eq!(value["host"]["args"]["data"]["data"], Value::Null);
        assert_eq!(value["host"]["args"]["event"], "ui:turn.done");
    }

    /// The fallback reproduces the JSON-path frame exactly: bytes put back as
    /// the base64 mapping where they sat.
    #[test]
    fn embedding_the_bytes_reproduces_the_json_frame() {
        let content = vec![9u8, 8, 7];
        let original = json!({ "name": "e", "data": cbor_payload(&content) });
        let doc = cbor_frame(&json!({ "wrapped": original })).expect("encodes");
        let (value, lifted) = split_bytes(doc).expect("splits");
        let lifted = lifted.expect("bytes");
        let mut data = value;
        assert!(lifted.embed(&mut data));
        assert_eq!(data, json!({ "wrapped": original }));
    }

    /// Every frame that existed before bytes existed converts 1:1 — the
    /// regression that matters.
    #[test]
    fn a_frame_without_bytes_maps_one_to_one() {
        let frame = json!({
            "id": 7,
            "kind": "call",
            "event": "e",
            "args": { "a": [1, 2.5, true, null, "t"], "b": { "c": -3 } },
        });
        let doc = cbor_frame(&frame).expect("encodes");
        let (back, lifted) = split_bytes(doc).expect("splits");
        assert!(lifted.is_none());
        assert_eq!(back, frame);
        assert!(!carries_payload(&frame));
    }
}