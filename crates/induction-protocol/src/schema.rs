//! The declaration surface: what a guest listens to, and the two ways a host
//! learns it.
//!
//! A guest declares handlers at load time (registration is structurally closed
//! once its loop starts — a phase gate, not an error value). The declaration
//! reaches the host in either of two shipped forms, both typed here:
//!
//! - **pull** — the host sends a `call` whose event is [`SCHEMA_EVENT`]; the
//!   guest answers with the assembled [`Schema`]. This is also the form
//!   introspection tooling uses, before any session exists.
//! - **handshake** — the host writes [`initialize_line`] and the guest answers
//!   [`Hello`], carrying the same schema at load. A host that needs the
//!   subscription set before it delivers anything uses this; a host that does
//!   not may skip it.
//!
//! Both decode; a guest should answer both. Neither is a negotiation: the
//! vocabulary is static, and decode failure — not a capability list — is the
//! version-mismatch signal.

use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// The event name whose `call` frame asks for the schema.
pub const SCHEMA_EVENT: &str = "interface_schema";

/// The assembled declaration: receives / wildcard_receives / hooks / storage /
/// auth.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Schema {
    /// Event name → per-handler declaration object (the reserved place for
    /// tier/deadline; today's guests emit an empty object — see ADR-0001).
    pub receives: BTreeMap<String, Value>,
    /// Prefix patterns (`prefix.*`) — singleton consumers. A plain host may
    /// ignore these; wildcard expansion is a route-registry concern.
    pub wildcard_receives: Vec<String>,
    /// Reserved lifecycle hooks (`__sleep` / `__wake`).
    pub hooks: Vec<String>,
    /// The ctx.store plan source, verbatim.
    pub storage: Option<Value>,
    /// The per-handler auth declaration, verbatim (prism's gate reads
    /// `auth.<handler>` as a boolean from the persisted schema — the
    /// guest declares WHICH handlers need a bound user; the host owns
    /// the enforcement).
    pub auth: Option<Value>,
}

impl Schema {
    /// Build from a flat subscription set (the handshake's `on` spelling).
    pub fn from_on(events: impl IntoIterator<Item = String>) -> Self {
        Self {
            receives: events
                .into_iter()
                .map(|e| (e, Value::Object(Map::new())))
                .collect(),
            ..Self::default()
        }
    }

    /// The flat spelling, for hosts that only need the set.
    pub fn on(&self) -> Vec<String> {
        self.receives.keys().cloned().collect()
    }

    /// Exact-membership subscription test. No wildcard, no namespace routing.
    pub fn receives(&self, event: &str) -> bool {
        self.receives.contains_key(event)
    }

    pub fn to_value(&self) -> Value {
        let mut v = Map::new();
        v.insert(
            "receives".into(),
            Value::Object(self.receives.clone().into_iter().collect()),
        );
        v.insert(
            "wildcard_receives".into(),
            Value::Array(
                self.wildcard_receives
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
        if !self.hooks.is_empty() {
            v.insert(
                "hooks".into(),
                Value::Array(self.hooks.iter().cloned().map(Value::String).collect()),
            );
        }
        if let Some(storage) = &self.storage {
            v.insert("storage".into(), storage.clone());
        }
        if let Some(auth) = &self.auth {
            v.insert("auth".into(), auth.clone());
        }
        Value::Object(v)
    }

    /// Decode the rich form, or the flat `{"on": [ … ]}` spelling.
    pub fn from_value(v: &Value) -> Option<Self> {
        // flat form first: `schema.on`
        if let Some(on) = v.get("on").and_then(Value::as_array) {
            let events = on
                .iter()
                .map(|e| e.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()?;
            let mut s = Schema::from_on(events);
            if let Some(hooks) = v.get("hooks").and_then(Value::as_array) {
                s.hooks = hooks
                    .iter()
                    .map(|h| h.as_str().map(str::to_string))
                    .collect::<Option<Vec<_>>>()?;
            }
            return Some(s);
        }
        // rich form: `receives` is required; everything else defaults.
        let receives_v = v.get("receives")?.as_object()?;
        let mut receives = BTreeMap::new();
        for (k, val) in receives_v {
            receives.insert(k.clone(), val.clone());
        }
        let wildcard_receives = match v.get("wildcard_receives") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) => a
                .iter()
                .map(|w| w.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()?,
            Some(_) => return None,
        };
        let hooks = match v.get("hooks") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) => a
                .iter()
                .map(|h| h.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()?,
            Some(_) => return None,
        };
        Some(Schema {
            receives,
            wildcard_receives,
            hooks,
            storage: v.get("storage").cloned(),
            auth: v.get("auth").cloned(),
        })
    }
}

/// The load-time hello: the guest's name plus its declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct Hello {
    pub name: String,
    pub schema: Schema,
}

impl Hello {
    pub fn from_on(name: impl Into<String>, events: impl IntoIterator<Item = String>) -> Self {
        Self {
            name: name.into(),
            schema: Schema::from_on(events),
        }
    }

    pub fn to_value(&self) -> Value {
        json!({
            "type": "hello",
            "name": self.name,
            "schema": self.schema.to_value(),
        })
    }

    pub fn to_line(&self) -> String {
        self.to_value().to_string()
    }

    /// Decode a hello line. `None` on any shape violation: wrong type, missing
    /// name, missing/unparsable schema. An EMPTY declaration is valid (a guest
    /// that listens to nothing still has a shape).
    pub fn parse_line(line: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(line).ok()?;
        if v.get("type")?.as_str()? != "hello" {
            return None;
        }
        let name = v.get("name")?.as_str()?.to_string();
        let schema = Schema::from_value(v.get("schema")?)?;
        Some(Self { name, schema })
    }
}

/// The handshake request the host writes first.
pub fn initialize_line() -> String {
    json!({ "type": "initialize", "protocol": crate::PROTOCOL }).to_string()
}
