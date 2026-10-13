//! # induction-protocol
//!
//! The wire vocabulary for **Induction**: a host embeds *resident extension
//! processes* that speak typed frames over a byte channel (stdio in the common
//! case). Two isolated processes, each with its own memory and state, exchanging
//! framed calls and effects — no shared address space, no port, no discovery
//! layer, no service registry.
//!
//! ## Why a protocol crate
//!
//! The vocabulary used to be spelled three times (the guest shim, the carrier,
//! and mudra's host), and the copies drifted. Here it has exactly one home:
//! `induction-host` drives sessions, `induction-guest` is the Rust guest face,
//! and both link this crate.
//!
//! ## Wire facts (all taken from shipped code, nothing invented here)
//!
//! - **Codec declaration**: the child learns the frame encoding from the
//!   [`ENCODING_ENV`] environment variable (`"json"` | `"cbor"`; absent or
//!   unknown = json — a bad value never fails a session). An env, not an argv:
//!   the fifo-shaped spawn specs pass exactly the author's own arguments, and a
//!   runtime-appended argv would break the author's arity.
//! - **parent → child**: `{"id":N,"kind":"call","event":…,"args":…}`; the
//!   iterate kinds additionally carry `op` (`start|next|dispose`) and
//!   `stream_id`. See [`CallFrame`].
//! - **child → parent**: `{"host":{"type":<arm>,…}}` answered by
//!   `{"host_reply":{"ok":…}}` / `{"host_reply":{"error":…}}` (see [`Reply`]),
//!   and `{"id":N,"result":…}` as a request's answer (see [`ResultFrame`]).
//! - **schema surface**: `receives` / `wildcard_receives` / `hooks` / `storage`,
//!   pulled with a `call` whose event is [`SCHEMA_EVENT`]. The load-time
//!   handshake ([`initialize_line`] → [`Hello`]) carries the same schema.
//! - **arms**: the ADR-0043 ctx members, dotted method paths, [`Arm::ALL`].
//!
//! Decode is strict everywhere: an unrecognised discriminator is a decode
//! failure (the version-mismatch signal), never a silent partial accept.

pub mod encoding;
pub mod frame;
pub mod schema;

pub use encoding::{ENCODING_ENV, Encoding};
pub use frame::{
    Arm, CallFrame, FrameKind, GuestFrame, HostFrame, InvokeCall, Reply, ResultFrame,
    parse_guest_line, parse_guest_value, parse_host_reply,
};
pub use schema::{Hello, SCHEMA_EVENT, Schema, initialize_line};

/// The protocol revision that rides the `initialize` frame. Decode failure is
/// the mismatch signal; there is no negotiation (the vocabulary is static and
/// both sides are installed together).
pub const PROTOCOL: u32 = 1;