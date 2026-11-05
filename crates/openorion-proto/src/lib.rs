//! Pure protocol crate for the Antelope Orion32 HD: the device model,
//! request encoding and reply decoding. No I/O.
//!
//! Wire reference: `docs/protocol-spec.md`. Design: `docs/DESIGN.md` sec. 4.
//!
//! Ownership: `model.rs` and `error.rs` are contracts (WP0, lead only).
//! `codec.rs`, `decode.rs` and `names.rs` are implemented by WP1.

pub mod codec;
pub mod decode;
pub mod error;
pub mod model;
pub mod names;

pub use codec::{Get, Incoming, REPORT_LEN, Report, Risk, Set, parse};
pub use error::ProtoError;
pub use model::*;
