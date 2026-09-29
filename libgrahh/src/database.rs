//! The database: identity plus topology, persisted as one image.

use grahh_adjacency::{AdjacencyStore, AdjacencyStoreBuilder, Direction, EdgeKindId};
use grahh_index::{LabelIndex, LabelIndexBuilder, NodeId};
use grahh_persist::PersistenceError;

use crate::image::{Region, RegionWriter, TAG_ADJACENCY, TAG_LABELS, read_image, write_image};

/// An immutable graph: user keys resolved to dense internal ids, topology per
/// edge kind, all in one persisted image.
#[derive(Debug)]
pub struct Database {
    labels: LabelIndex,
    adjacency: AdjacencyStore,
}

impl Database {
    /// Number of nodes (keys) in the database.
    pub fn node_count(&self) -> usize {
        self.labels.node_count()
    }

    /// Total edges across all kinds and directions.
    pub fn edge_count(&self) -> usize {
        self.adjacency.edge_count()
    }

    /// Resolve a user key to its internal node id.
    pub fn lookup(&self, key: u64) -> Option<NodeId> {
        self.labels.lookup(key)
    }

    /// Resolve an internal node id back to its user key.
    pub fn key_of(&self, node: NodeId) -> Option<u64> {
        self.labels.key_of(node)
    }

    /// Expand a node through `kind` in direction `dir`, as internal ids.
    pub fn expand(&self, node: NodeId, kind: EdgeKindId, dir: Direction) -> Vec<NodeId> {
        self.adjacency.neighbors(node, kind, dir)
    }

    /// Serialize the whole database, appending to `out`.
    ///
    /// # Errors
    /// Never fails for a database built by [`DatabaseBuilder`]; the `Result`
    /// mirrors [`Database::deserialize`].
    pub fn serialize(&self, out: &mut Vec<u8>) -> Result<(), PersistenceError> {
        let regions: [(u32, RegionWriter<'_>); 2] = [
            (TAG_LABELS, &|bytes| self.labels.serialize(bytes)),
            (TAG_ADJACENCY, &|bytes| self.adjacency.serialize(bytes)),
        ];
        write_image(out, &regions)
    }

    /// Rebuild a database written by [`Database::serialize`].
    ///
    /// The envelope is validated first, then each region by its own reader, and
    /// finally the regions against each other: topology may only reference ids
    /// the identity layer knows about.
    ///
    /// # Errors
    /// [`PersistenceError`] if the image is truncated or corrupt, if a required
    /// region is missing, or if it carries an unknown region tag.
    pub fn deserialize(bytes: &[u8]) -> Result<Self, PersistenceError> {
        let mut labels: Option<LabelIndex> = None;
        let mut adjacency: Option<AdjacencyStore> = None;

        for region in read_image(bytes)? {
            match region.tag {
                TAG_LABELS => {
                    labels = Some(region_parse(
                        region,
                        "label index",
                        LabelIndex::deserialize,
                    )?);
                }
                TAG_ADJACENCY => {
                    adjacency = Some(region_parse(
                        region,
                        "adjacency registry",
                        AdjacencyStore::deserialize,
                    )?);
                }
                tag => return Err(PersistenceError::UnknownRegion { tag }),
            }
        }

        let labels: LabelIndex = labels.ok_or(PersistenceError::Corrupt {
            reason: "image has no label index region",
        })?;
        let adjacency: AdjacencyStore = adjacency.ok_or(PersistenceError::Corrupt {
            reason: "image has no adjacency region",
        })?;

        if adjacency.node_count() > labels.node_count() {
            return Err(PersistenceError::Corrupt {
                reason: "adjacency references ids the label index does not know",
            });
        }

        Ok(Self { labels, adjacency })
    }
}

/// Run one region through its own validator, naming the region if it fails.
fn region_parse<T>(
    region: Region<'_>,
    name: &'static str,
    parse: fn(&[u8]) -> Result<T, PersistenceError>,
) -> Result<T, PersistenceError> {
    parse(region.bytes).map_err(|source| PersistenceError::Nested {
        context: name.to_string(),
        source: Box::new(source),
    })
}

/// Assemble a database from keys and key-to-key edges.
///
/// Ids are issued on first sight, so declaring edges by key never requires the
/// caller to know internal ids — and the same input sequence always produces the
/// same ids.
#[derive(Debug, Default)]
pub struct DatabaseBuilder {
    labels: LabelIndexBuilder,
    adjacency: AdjacencyStoreBuilder,
}

impl DatabaseBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// The internal id for `key`, assigning the next dense id if unseen.
    pub fn node(&mut self, key: u64) -> NodeId {
        self.labels.add(key)
    }

    /// Declare a directed edge between two user keys.
    pub fn edge(&mut self, kind: EdgeKindId, source: u64, target: u64) {
        let source: NodeId = self.node(source);
        let target: NodeId = self.node(target);
        self.adjacency.add_edge(kind, source, target);
    }

    /// Declare that `kind` should also expose an incoming (reverse) adjacency.
    pub fn set_inverse(&mut self, kind: EdgeKindId) {
        self.adjacency.set_inverse(kind);
    }

    /// Freeze into an immutable [`Database`].
    pub fn build(self) -> Database {
        Database {
            labels: self.labels.build(),
            adjacency: self.adjacency.build(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::ENTRY_LEN;
    use grahh_persist::HEADER_LEN;

    const FOLLOWS: EdgeKindId = EdgeKindId(0);

    /// Keys 100/200/300, with 100 -> {200, 300} and 200 -> 300.
    fn small_graph() -> Database {
        let mut b = DatabaseBuilder::new();
        b.set_inverse(FOLLOWS);
        b.edge(FOLLOWS, 100, 200);
        b.edge(FOLLOWS, 100, 300);
        b.edge(FOLLOWS, 200, 300);
        b.build()
    }

    /// The minimum query: key -> id -> expand -> keys, through bytes.
    #[test]
    fn lookup_then_expand_survives_a_round_trip() {
        let db: Database = small_graph();

        let mut bytes: Vec<u8> = Vec::new();
        db.serialize(&mut bytes).unwrap();
        let db: Database = Database::deserialize(&bytes).unwrap();

        assert_eq!(db.node_count(), 3);
        // Three forward edges plus three reverse ones: `FOLLOWS` declares an
        // inverse, so both directions are stored.
        assert_eq!(db.edge_count(), 6);

        let key_of = |node: NodeId| db.key_of(node).expect("ids resolve back to keys");
        let follows = |key: u64, dir: Direction| -> Vec<u64> {
            db.expand(db.lookup(key).expect("known key"), FOLLOWS, dir)
                .into_iter()
                .map(key_of)
                .collect()
        };

        assert_eq!(follows(100, Direction::Outgoing), vec![200, 300]);
        assert_eq!(follows(200, Direction::Outgoing), vec![300]);
        assert_eq!(follows(100, Direction::Incoming), Vec::<u64>::new());
        assert_eq!(follows(300, Direction::Incoming), vec![100, 200]);
        assert_eq!(db.lookup(999), None);
    }

    #[test]
    fn serialization_is_byte_for_byte_stable() {
        let db: Database = small_graph();

        let mut first: Vec<u8> = Vec::new();
        db.serialize(&mut first).unwrap();
        let back: Database = Database::deserialize(&first).unwrap();

        let mut second: Vec<u8> = Vec::new();
        back.serialize(&mut second).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn empty_database_round_trips() {
        let db: Database = DatabaseBuilder::new().build();

        let mut bytes: Vec<u8> = Vec::new();
        db.serialize(&mut bytes).unwrap();
        let back: Database = Database::deserialize(&bytes).unwrap();

        assert_eq!(back.node_count(), 0);
        assert_eq!(back.edge_count(), 0);
        assert_eq!(back.lookup(1), None);
    }

    #[test]
    fn image_rejects_unknown_regions() {
        let mut image: Vec<u8> = Vec::new();
        let regions: [(u32, RegionWriter<'_>); 1] = [(99, &|bytes| {
            bytes.push(0);
            Ok(())
        })];
        write_image(&mut image, &regions).unwrap();

        assert!(matches!(
            Database::deserialize(&image),
            Err(PersistenceError::UnknownRegion { tag: 99 })
        ));
    }

    #[test]
    fn image_rejects_missing_regions() {
        let mut labels: Vec<u8> = Vec::new();
        let mut b = LabelIndexBuilder::new();
        b.add(7);
        b.build().serialize(&mut labels).unwrap();

        let mut image: Vec<u8> = Vec::new();
        let regions: [(u32, RegionWriter<'_>); 1] = [(TAG_LABELS, &|bytes| {
            bytes.extend_from_slice(&labels);
            Ok(())
        })];
        write_image(&mut image, &regions).unwrap();

        assert!(matches!(
            Database::deserialize(&image),
            Err(PersistenceError::Corrupt { .. })
        ));
    }

    #[test]
    fn image_rejects_topology_that_outruns_the_label_index() {
        let mut labels: Vec<u8> = Vec::new();
        let mut b = LabelIndexBuilder::new();
        b.add(7);
        b.build().serialize(&mut labels).unwrap();

        let mut registry: Vec<u8> = Vec::new();
        let mut adjacency = AdjacencyStoreBuilder::new();
        // A source id past the end of the identity layer: the store's id span is
        // derived from source ids, which is exactly what the cross-check bounds.
        adjacency.add_edge(FOLLOWS, 5, 0);
        adjacency.build().serialize(&mut registry).unwrap();

        let mut image: Vec<u8> = Vec::new();
        let regions: [(u32, RegionWriter<'_>); 2] = [
            (TAG_LABELS, &|bytes| {
                bytes.extend_from_slice(&labels);
                Ok(())
            }),
            (TAG_ADJACENCY, &|bytes| {
                bytes.extend_from_slice(&registry);
                Ok(())
            }),
        ];
        write_image(&mut image, &regions).unwrap();

        assert!(matches!(
            Database::deserialize(&image),
            Err(PersistenceError::Corrupt { .. })
        ));
    }

    #[test]
    fn region_failures_name_the_region() {
        let db: Database = small_graph();
        let mut bytes: Vec<u8> = Vec::new();
        db.serialize(&mut bytes).unwrap();

        // The first region starts right after the two-entry directory.
        let first_region_at: usize = HEADER_LEN + 4 + 2 * ENTRY_LEN;
        bytes[first_region_at] ^= 0xFF; // breaks its magic

        match Database::deserialize(&bytes) {
            Err(PersistenceError::Nested { context, .. }) => assert_eq!(context, "label index"),
            other => panic!("expected a nested error, got {other:?}"),
        }
    }

    #[test]
    fn image_rejects_a_directory_that_does_not_cover_the_buffer() {
        let db: Database = small_graph();
        let mut bytes: Vec<u8> = Vec::new();
        db.serialize(&mut bytes).unwrap();

        // First entry's offset field: header + region count + tag.
        let offset_at: usize = HEADER_LEN + 4 + 4;

        let mut gap: Vec<u8> = bytes.clone();
        gap[offset_at..offset_at + 8].copy_from_slice(&0_u64.to_le_bytes());
        assert!(matches!(
            Database::deserialize(&gap),
            Err(PersistenceError::Corrupt { .. })
        ));

        let truncated: &[u8] = &bytes[..bytes.len() - 1];
        assert!(matches!(
            Database::deserialize(truncated),
            Err(PersistenceError::Corrupt { .. })
        ));

        let mut extra: Vec<u8> = bytes.clone();
        extra.push(0);
        assert!(matches!(
            Database::deserialize(&extra),
            Err(PersistenceError::TrailingBytes { .. })
        ));
    }
}
