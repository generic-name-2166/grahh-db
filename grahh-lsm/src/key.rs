//! The fixed-width key and adjacency value used by the LSM layer.
//!
//! The key is 12 bytes little-endian: a dense internal node id followed by an
//! edge-kind id. The value is 16 bytes: the target node id and an edge id that
//! distinguishes parallel edges between the same pair of nodes.

use crate::error::{LsmError, Result};

/// A tablet key: `(node_id, kind_id)`.
///
/// Ordering is numeric `(node_id, kind_id)`, which is the order rows are kept
/// in inside a tablet chunk. The encoded form is the fixed 12-byte LE layout
/// from the spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    /// Dense internal source node id.
    pub node_id: u64,
    /// Edge kind (or reserved metadata kind).
    pub kind_id: u32,
}

impl Key {
    /// Encoded width of a key, in bytes.
    pub const LEN: usize = 12;

    /// Construct a key.
    pub fn new(node_id: u64, kind_id: u32) -> Self {
        Self { node_id, kind_id }
    }

    /// Encode the key into its 12-byte little-endian form.
    pub fn to_bytes(self) -> [u8; Self::LEN] {
        let mut out = [0_u8; Self::LEN];
        out[..8].copy_from_slice(&self.node_id.to_le_bytes());
        out[8..].copy_from_slice(&self.kind_id.to_le_bytes());
        out
    }

    /// Decode a key from exactly 12 bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != Self::LEN {
            return Err(LsmError::Corrupt(format!(
                "key must be {} bytes, got {}",
                Self::LEN,
                bytes.len()
            )));
        }
        let node_id = u64::from_le_bytes(bytes[..8].try_into().expect("8-byte slice"));
        let kind_id = u32::from_le_bytes(bytes[8..].try_into().expect("4-byte slice"));
        Ok(Self { node_id, kind_id })
    }
}

/// One adjacency item: `(target_id, edge_id)`.
///
/// `edge_id` makes parallel edges distinct. For a plain graph with no parallel
/// edges it can be zero for every row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Value {
    /// Target node id.
    pub target_id: u64,
    /// Identifier distinguishing parallel edges to the same target.
    pub edge_id: u64,
}

impl Value {
    /// Encoded width of a value, in bytes.
    pub const LEN: usize = 16;

    /// Construct a value.
    pub fn new(target_id: u64, edge_id: u64) -> Self {
        Self { target_id, edge_id }
    }

    /// Encode the value into its 16-byte little-endian form.
    pub fn to_bytes(self) -> [u8; Self::LEN] {
        let mut out = [0_u8; Self::LEN];
        out[..8].copy_from_slice(&self.target_id.to_le_bytes());
        out[8..].copy_from_slice(&self.edge_id.to_le_bytes());
        out
    }

    /// Decode a value from exactly 16 bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != Self::LEN {
            return Err(LsmError::InvalidValueLength { found: bytes.len() });
        }
        let target_id = u64::from_le_bytes(bytes[..8].try_into().expect("8-byte slice"));
        let edge_id = u64::from_le_bytes(bytes[8..].try_into().expect("8-byte slice"));
        Ok(Self { target_id, edge_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_round_trips() {
        let key = Key::new(0xDEAD_BEEF_1234_5678, 0xAABB_CCDD);
        let bytes = key.to_bytes();
        assert_eq!(bytes.len(), Key::LEN);
        assert_eq!(Key::from_bytes(&bytes).unwrap(), key);
    }

    #[test]
    fn value_round_trips() {
        let value = Value::new(7, 9);
        let bytes = value.to_bytes();
        assert_eq!(bytes.len(), Value::LEN);
        assert_eq!(Value::from_bytes(&bytes).unwrap(), value);
    }

    #[test]
    fn wrong_widths_are_rejected() {
        assert!(Key::from_bytes(&[0; 11]).is_err());
        assert!(Value::from_bytes(&[0; 15]).is_err());
    }

    #[test]
    fn keys_sort_by_node_then_kind() {
        let mut keys = vec![
            Key::new(2, 0),
            Key::new(1, 9),
            Key::new(1, 3),
            Key::new(0, 1),
        ];
        keys.sort();
        assert_eq!(
            keys,
            vec![
                Key::new(0, 1),
                Key::new(1, 3),
                Key::new(1, 9),
                Key::new(2, 0),
            ]
        );
    }
}
