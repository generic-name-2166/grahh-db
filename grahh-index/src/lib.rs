//! External → internal identity: the only stable reference users hold.
//!
//! The topology layer indexes with dense `u64` node ids that are *ephemeral* —
//! renumbered at compaction, never a user-facing handle. This crate is the
//! translation between the two worlds: a [`LabelIndex`] maps a user key to the
//! dense internal id the stores index with, and back again.
//!
//! Like every store in the engine it persists through `grahh-persist`
//! primitives: fixed-width little-endian fields, length-prefixed columns,
//! compressed payloads copied verbatim, and full validation on load.

mod label;

pub use label::{LabelIndex, LabelIndexBuilder, NodeId};
