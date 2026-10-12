//! The frame codec declaration.
//!
//! The codec is declared by the parent through [`ENCODING_ENV`], one encoding
//! per session's life (never negotiated per frame). Both encodings carry the
//! same `Value`-shaped frames: JSON is newline-delimited text; CBOR is one
//! self-delimited document per frame (no length prefix, no terminator).

/// The environment variable that carries the declared codec to the child.
///
/// Renamed from the pre-Induction spelling before the vocabulary was frozen;
/// the value string is part of the wire contract.
pub const ENCODING_ENV: &str = "INDUCTION_ENCODING";

/// Frame encoding, declared per session.
///
/// This is a SERIALIZED type, not just an in-process enum: a host that
/// carries the declaration in its own control frame (effector's `ToolCall`,
/// aura's persisted booth record) stores exactly these spellings, so the
/// derive and the rename rule are part of the wire contract — every consumer
/// takes this type rather than minting a second enum with the same values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    /// Newline-delimited JSON — the stdlib-reachable default.
    #[default]
    Json,
    /// One self-delimited CBOR document per frame.
    Cbor,
}

impl Encoding {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Encoding::Json => "json",
            Encoding::Cbor => "cbor",
        }
    }

    /// Decode the declared spelling. Unknown values are `None` — the caller
    /// falls back to [`Encoding::Json`]; an unrecognised codec name must not
    /// fail a session (the shipped behaviour, kept deliberately).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "json" => Some(Encoding::Json),
            "cbor" => Some(Encoding::Cbor),
            _ => None,
        }
    }

    /// The codec declared by this process's environment; absent or unknown
    /// resolves to JSON.
    pub fn from_env() -> Self {
        std::env::var(ENCODING_ENV)
            .ok()
            .and_then(|v| Encoding::parse(&v))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spellings_round_trip() {
        for e in [Encoding::Json, Encoding::Cbor] {
            assert_eq!(Encoding::parse(e.as_str()), Some(e));
        }
    }

    #[test]
    fn unknown_codec_is_none_and_defaults_to_json() {
        assert_eq!(Encoding::parse("msgpack"), None);
        assert_eq!(Encoding::default(), Encoding::Json);
    }

    #[test]
    fn env_name_is_frozen() {
        // The env name is wire-visible: a guest binary reads exactly this.
        assert_eq!(ENCODING_ENV, "INDUCTION_ENCODING");
    }
}