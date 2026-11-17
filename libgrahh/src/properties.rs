//! Typed property store backed by the LSMT list/cell structure.
//!
//! Properties reuse the same LSMT primitives as adjacency: each property slot
//! is a `(node_id, kind_id)` key whose value is a [`grahh_lsm::memtable::Cell`]
//! (`One | ZeroOrOne | Many`). This means list-valued properties and adjacency
//! lists share the same replace-semantics, bloom/directory, and compaction
//! behavior in tablets.
//!
//! Property `kind_id` values must not collide with edge kinds or reserved
//! metadata kinds used by [`crate::Database`].

use grahh_lsm::{Engine, Key, LsmError};

use crate::identity::NodeId;

/// Opaque id for a property kind/definition in the storage key space.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PropertyKindId(pub u32);

/// Supported property value types (schema-level).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PropertyType {
    Bool,
    U64,
    I64,
    F64Bits,
    String,
    Bytes,
    /// Reference to a node (dense internal id).
    NodeRef,
}

/// How many values a property may hold. Matches LSM [`Cell`] shapes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cardinality {
    One,
    ZeroOrOne,
    Many,
}

/// A typed property value.
#[derive(Clone, Debug, PartialEq)]
pub enum PropertyValue {
    Bool(bool),
    U64(u64),
    I64(i64),
    F64(f64),
    String(String),
    Bytes(Vec<u8>),
    NodeRef(NodeId),
}

impl PropertyValue {
    pub fn matches_type(&self, ty: &PropertyType) -> bool {
        matches!(
            (self, ty),
            (Self::Bool(_), PropertyType::Bool)
                | (Self::U64(_), PropertyType::U64)
                | (Self::I64(_), PropertyType::I64)
                | (Self::F64(_), PropertyType::F64Bits)
                | (Self::String(_), PropertyType::String)
                | (Self::Bytes(_), PropertyType::Bytes)
                | (Self::NodeRef(_), PropertyType::NodeRef)
        )
    }

    pub fn encode(&self, expected: &PropertyType) -> Result<Vec<u8>, LsmError> {
        if !self.matches_type(expected) {
            return Err(LsmError::Corrupt(format!(
                "property type mismatch: expected {expected:?}, got {self:?}"
            )));
        }
        Ok(match self {
            Self::Bool(v) => vec![u8::from(*v)],
            Self::U64(v) => v.to_le_bytes().to_vec(),
            Self::I64(v) => v.to_le_bytes().to_vec(),
            Self::F64(v) => v.to_bits().to_le_bytes().to_vec(),
            Self::String(v) => {
                let bytes = v.as_bytes();
                let mut out = Vec::with_capacity(8 + bytes.len());
                out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
                out.extend_from_slice(bytes);
                out
            }
            Self::Bytes(v) => {
                let mut out = Vec::with_capacity(8 + v.len());
                out.extend_from_slice(&(v.len() as u64).to_le_bytes());
                out.extend_from_slice(v);
                out
            }
            Self::NodeRef(v) => v.to_le_bytes().to_vec(),
        })
    }

    pub fn decode(bytes: &[u8], ty: &PropertyType) -> Result<Self, LsmError> {
        let err = || LsmError::Corrupt(format!("invalid {ty:?} property encoding"));
        Ok(match ty {
            PropertyType::Bool => match bytes {
                [0] => Self::Bool(false),
                [1] => Self::Bool(true),
                _ => return Err(err()),
            },
            PropertyType::U64 => {
                Self::U64(u64::from_le_bytes(bytes.try_into().map_err(|_| err())?))
            }
            PropertyType::I64 => {
                Self::I64(i64::from_le_bytes(bytes.try_into().map_err(|_| err())?))
            }
            PropertyType::F64Bits => Self::F64(f64::from_bits(u64::from_le_bytes(
                bytes.try_into().map_err(|_| err())?,
            ))),
            PropertyType::String => Self::String(decode_len_prefixed_utf8(bytes)?),
            PropertyType::Bytes => Self::Bytes(decode_len_prefixed(bytes)?),
            PropertyType::NodeRef => {
                Self::NodeRef(u64::from_le_bytes(bytes.try_into().map_err(|_| err())?))
            }
        })
    }
}

fn decode_len_prefixed(bytes: &[u8]) -> Result<Vec<u8>, LsmError> {
    if bytes.len() < 8 {
        return Err(LsmError::Corrupt(
            "property blob missing length prefix".into(),
        ));
    }
    let len = u64::from_le_bytes(bytes[..8].try_into().unwrap()) as usize;
    if bytes.len() != 8 + len {
        return Err(LsmError::Corrupt(
            "property blob length prefix mismatch".into(),
        ));
    }
    Ok(bytes[8..].to_vec())
}

fn decode_len_prefixed_utf8(bytes: &[u8]) -> Result<String, LsmError> {
    String::from_utf8(decode_len_prefixed(bytes)?)
        .map_err(|_| LsmError::Corrupt("property string is not UTF-8".into()))
}

/// A property definition (schema).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyDef {
    pub id: PropertyKindId,
    pub name: String,
    pub value_type: PropertyType,
    pub cardinality: Cardinality,
}

/// LSMT-backed property operations.
///
/// Uses [`Engine::add_raw`]/[`Engine::remove_raw`]/[`Engine::get_raw`], sharing
/// the same list/cell storage implementation as adjacency.
#[derive(Debug, Default)]
pub struct PropertyStore;

impl PropertyStore {
    /// Append/set a property value. For `Many` this appends; for single-valued
    /// semantics callers should use replace-style logic at a higher layer or
    /// model via the LSM cell semantics used by the engine.
    pub fn add(
        engine: &mut Engine,
        node: NodeId,
        property: PropertyKindId,
        value: &PropertyValue,
        expected_type: &PropertyType,
    ) -> Result<(), LsmError> {
        let bytes = value.encode(expected_type)?;
        engine.add_raw(Key::new(node, property.0), bytes)
    }

    /// Remove a matching property value row.
    pub fn remove(
        engine: &mut Engine,
        node: NodeId,
        property: PropertyKindId,
        value: &PropertyValue,
        expected_type: &PropertyType,
    ) -> Result<(), LsmError> {
        let bytes = value.encode(expected_type)?;
        engine.remove_raw(Key::new(node, property.0), &bytes)
    }

    /// Read all rows for a property on a node.
    pub fn get(
        engine: &Engine,
        node: NodeId,
        property: PropertyKindId,
        value_type: &PropertyType,
    ) -> Result<Vec<PropertyValue>, LsmError> {
        engine
            .get_raw(Key::new(node, property.0))?
            .into_iter()
            .map(|bytes| PropertyValue::decode(&bytes, value_type))
            .collect()
    }

    /// Read the first/single value, if any.
    pub fn get_one(
        engine: &Engine,
        node: NodeId,
        property: PropertyKindId,
        value_type: &PropertyType,
    ) -> Result<Option<PropertyValue>, LsmError> {
        Ok(Self::get(engine, node, property, value_type)?.into_iter().next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("grahh-prop-{}-{id}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn typed_values_round_trip() {
        let cases = [
            (PropertyType::Bool, PropertyValue::Bool(true)),
            (PropertyType::U64, PropertyValue::U64(42)),
            (PropertyType::I64, PropertyValue::I64(-9)),
            (PropertyType::F64Bits, PropertyValue::F64(1.5)),
            (PropertyType::String, PropertyValue::String("hi".into())),
            (PropertyType::Bytes, PropertyValue::Bytes(vec![1, 2])),
            (PropertyType::NodeRef, PropertyValue::NodeRef(7)),
        ];
        for (ty, value) in cases {
            let bytes = value.encode(&ty).unwrap();
            let back = PropertyValue::decode(&bytes, &ty).unwrap();
            assert_eq!(back, value);
        }
    }

    #[test]
    fn properties_persist_through_lsmt() {
        let dir = TempDir::new();
        let prop = PropertyKindId(10_000);
        let ty = PropertyType::String;
        {
            let mut engine = Engine::open(&dir.0).unwrap();
            PropertyStore::add(&mut engine, 1, prop, &PropertyValue::String("alice".into()), &ty)
                .unwrap();
            engine.flush().unwrap();
        }
        let engine = Engine::open(&dir.0).unwrap();
        assert_eq!(
            PropertyStore::get_one(&engine, 1, prop, &ty).unwrap(),
            Some(PropertyValue::String("alice".into()))
        );
    }

    #[test]
    fn many_cardinality_properties_use_list_storage() {
        let dir = TempDir::new();
        let prop = PropertyKindId(10_001);
        let ty = PropertyType::U64;
        let mut engine = Engine::open(&dir.0).unwrap();
        PropertyStore::add(&mut engine, 5, prop, &PropertyValue::U64(1), &ty).unwrap();
        PropertyStore::add(&mut engine, 5, prop, &PropertyValue::U64(2), &ty).unwrap();
        engine.flush().unwrap();
        assert_eq!(
            PropertyStore::get(&engine, 5, prop, &ty).unwrap(),
            vec![PropertyValue::U64(1), PropertyValue::U64(2)]
        );
    }
}
