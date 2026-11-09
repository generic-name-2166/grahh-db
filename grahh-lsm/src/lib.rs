//! GRAHH-DB's LSM key-value layer.
//!
//! The engine stores directed adjacency as `(node_id, kind_id) -> (target_id,
//! edge_id)` rows. It is an LSM tree with a WAL, a `BTreeMap` memtable, and
//! immutable columnar tablets:
//!
//! - [`Engine`] is the durable front door (`add`, `remove`, `get`, `flush`).
//! - [`tablet`] is the immutable columnar run format.
//! - [`wal`] is the durable write log.
//! - [`manifest`] and [`levels`] track which tablets are live.
//!
//! Values are the 16-byte adjacency pair `[target_id][edge_id]` in this MVP.
//! Property rows and variable-length values are a later layer on top of the
//! same key space.

mod bloom;
mod engine;
mod error;
mod key;
mod levels;
mod manifest;
mod memtable;
mod tablet;
mod wal;

pub use bloom::BloomFilter;
pub use engine::{Engine, EngineConfig};
pub use error::{LsmError, Result};
pub use key::{Key, Value};
pub use levels::Levels;
pub use manifest::{Manifest, ManifestOp};
pub use memtable::{Cell, Memtable};
pub use tablet::{TABLET_MAGIC, TABLET_VERSION, Tablet, TabletBuilder};
pub use wal::{Record, RecordKind, WAL_HEADER_LEN, WAL_MAGIC, WAL_VERSION, Wal, decode_records};
