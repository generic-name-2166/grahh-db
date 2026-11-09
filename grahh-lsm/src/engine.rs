//! The top-level LSM engine.
//!
//! ```text
//! write:  WAL append + fsync -> materialize old state -> apply delta -> memtable
//! flush:  memtable -> tablet -> fsync/rename -> manifest(add, max_seq) -> truncate WAL
//! read:   memtable -> L0 newest-first -> L1, L2, ...
//! ```
//!
//! A key is authoritative in the newest place that contains it. A tablet's
//! exact key/length directory means "contains" is exact, not a bloom guess, so
//! a key whose list became empty shadows older rows correctly.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use crate::{
    error::{LsmError, Result},
    key::{Key, Value},
    levels::Levels,
    manifest::{Manifest, ManifestOp},
    memtable::{Cell, Memtable},
    tablet::{Tablet, TabletBuilder},
    wal::{Record, RecordKind, Wal},
};

/// Tunable engine settings.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// Approximate memtable size that triggers a flush.
    pub memtable_bytes: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            memtable_bytes: 4 * 1024 * 1024,
        }
    }
}

/// A durable LSM key-value store.
pub struct Engine {
    dir: PathBuf,
    config: EngineConfig,
    wal: Wal,
    manifest: Manifest,
    levels: Levels,
    tablets: BTreeMap<String, Tablet>,
    memtable: Memtable,
    max_seq: u64,
    next_tablet_id: u64,
}

impl Engine {
    /// Open (or create) a database in `dir` with default settings.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_config(dir, EngineConfig::default())
    }

    /// Open (or create) a database with explicit settings.
    pub fn open_with_config(dir: impl AsRef<Path>, config: EngineConfig) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        remove_stale_tmp_files(&dir)?;

        let manifest = Manifest::open(dir.join("manifest.log"))?;
        let ops = manifest.replay()?;
        let mut levels = Levels::new();
        let mut flushed_seq = 0_u64;
        let mut next_tablet_id = 1_u64;
        for op in &ops {
            levels.apply(op);
            if let ManifestOp::Add {
                filename, max_seq, ..
            } = op
            {
                flushed_seq = flushed_seq.max(*max_seq);
                next_tablet_id = next_tablet_id.max(tablet_id_of(filename) + 1);
            }
        }

        let mut tablets = BTreeMap::new();
        for (_, filename) in levels.tablets_newest_first() {
            let path = dir.join(&filename);
            let bytes = fs::read(&path)?;
            let tablet = Tablet::deserialize(&bytes)?;
            tablets.insert(filename, tablet);
        }
        // A tablet renamed into place but not committed to the manifest is an
        // orphan: its WAL records are still present, so replay will rebuild it.
        remove_unreferenced_tablets(&dir, &tablets)?;

        let mut wal = Wal::open(dir.join("wal.log"))?;
        let (records, valid_len) = wal.replay()?;
        if valid_len < wal.len()? {
            wal.truncate_to(valid_len)?;
        }

        let mut engine = Self {
            dir,
            config,
            wal,
            manifest,
            levels,
            tablets,
            memtable: Memtable::new(),
            max_seq: flushed_seq,
            next_tablet_id,
        };

        for record in records {
            engine.max_seq = engine.max_seq.max(record.seq);
            if record.seq <= flushed_seq {
                continue;
            }
            engine.apply_record(&record)?;
        }

        if engine.memtable.approximate_bytes() >= engine.config.memtable_bytes {
            engine.flush()?;
        }

        Ok(engine)
    }

    /// Append an adjacency item and make it durable.
    pub fn add(&mut self, key: Key, target_id: u64, edge_id: u64) -> Result<()> {
        self.write(RecordKind::Add, key, Value::new(target_id, edge_id))
    }

    /// Remove an adjacency item and make the removal durable.
    pub fn remove(&mut self, key: Key, target_id: u64, edge_id: u64) -> Result<()> {
        self.write(RecordKind::Remove, key, Value::new(target_id, edge_id))
    }

    /// Read every item for `key`.
    ///
    /// An absent key and a present-but-empty key both return an empty vector;
    /// callers that need to distinguish them should use
    /// [`Engine::contains_key`].
    pub fn get(&self, key: Key) -> Result<Vec<Value>> {
        if let Some(cell) = self.memtable.get(key) {
            return cell
                .iter()
                .map(Value::from_bytes)
                .collect::<Result<Vec<_>>>();
        }
        for (_, filename) in self.levels.tablets_newest_first() {
            let tablet = self.tablet(&filename)?;
            if let Some(values) = tablet.get(key)? {
                return Ok(values);
            }
        }
        Ok(Vec::new())
    }

    /// `true` if the key is present in the memtable or a tablet, even if its
    /// value list is empty.
    pub fn contains_key(&self, key: Key) -> Result<bool> {
        if self.memtable.get(key).is_some() {
            return Ok(true);
        }
        for (_, filename) in self.levels.tablets_newest_first() {
            if self.tablet(&filename)?.get(key)?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Flush the memtable into a new L0 tablet.
    ///
    /// No-op when the memtable is empty.
    pub fn flush(&mut self) -> Result<()> {
        if self.memtable.is_empty() {
            return Ok(());
        }
        let cells = self.memtable.take();
        let tablet = TabletBuilder::from_cells(cells).build()?;
        let filename = format!("{:08}.tablet", self.next_tablet_id);
        self.next_tablet_id += 1;

        let tmp_path = self.dir.join(format!("{filename}.tmp"));
        let final_path = self.dir.join(&filename);
        let bytes = tablet.serialize();
        {
            let mut file = File::create(&tmp_path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(&tmp_path, &final_path)?;

        // Commit point: the manifest line carries the sequence boundary. A
        // crash after this line but before WAL truncation replays nothing that
        // is already in the tablet.
        self.manifest.append(&ManifestOp::Add {
            level: 0,
            filename: filename.clone(),
            max_seq: self.max_seq,
        })?;
        self.levels.add(0, filename.clone());
        self.tablets.insert(filename, tablet);
        self.wal.truncate()?;
        Ok(())
    }

    /// Flush only if the memtable has reached its byte budget.
    pub fn flush_if_needed(&mut self) -> Result<bool> {
        if self.memtable.approximate_bytes() >= self.config.memtable_bytes {
            self.flush()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Every `(node_id, value)` row for one kind, with replace semantics
    /// applied across the memtable and all levels.
    pub fn scan_kind(&self, kind_id: u32) -> Result<Vec<(u64, Value)>> {
        let mut seen: BTreeSet<Key> = BTreeSet::new();
        let mut out: Vec<(u64, Value)> = Vec::new();

        for (key, cell) in self.memtable.iter() {
            if key.kind_id != kind_id {
                continue;
            }
            seen.insert(*key);
            for value in cell.iter() {
                out.push((key.node_id, Value::from_bytes(value)?));
            }
        }

        for (_, filename) in self.levels.tablets_newest_first() {
            let tablet = self.tablet(&filename)?;
            for (key, _) in tablet.keys() {
                if key.kind_id != kind_id || !seen.insert(key) {
                    continue;
                }
                if let Some(values) = tablet.get(key)? {
                    for value in values {
                        out.push((key.node_id, value));
                    }
                }
            }
        }

        out.sort_unstable();
        Ok(out)
    }

    /// Every kind id currently present in the memtable or any tablet.
    pub fn kinds(&self) -> Result<Vec<u32>> {
        let mut kinds = BTreeSet::new();
        for (key, _) in self.memtable.iter() {
            kinds.insert(key.kind_id);
        }
        for tablet in self.tablets.values() {
            for (key, _) in tablet.keys() {
                kinds.insert(key.kind_id);
            }
        }
        Ok(kinds.into_iter().collect())
    }

    /// Highest sequence number observed so far.
    pub fn max_seq(&self) -> u64 {
        self.max_seq
    }

    /// Number of distinct keys buffered in memory.
    pub fn memtable_len(&self) -> usize {
        self.memtable.len()
    }

    /// Approximate bytes buffered in memory.
    pub fn memtable_bytes(&self) -> usize {
        self.memtable.approximate_bytes()
    }

    /// Number of committed tablets across all levels.
    pub fn tablet_count(&self) -> usize {
        self.levels.tablet_count()
    }

    /// Database directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn write(&mut self, kind: RecordKind, key: Key, value: Value) -> Result<()> {
        let value = value.to_bytes().to_vec();
        let seq = self.max_seq + 1;
        let record = Record {
            seq,
            kind,
            key,
            value: value.clone(),
        };
        // Durable first, visible second.
        self.wal.append(&record)?;
        self.max_seq = seq;
        self.ensure_materialized(key)?;
        match kind {
            RecordKind::Add => self.memtable.add(key, value),
            RecordKind::Remove => self.memtable.remove(key, &value),
        }
        self.flush_if_needed()?;
        Ok(())
    }

    fn apply_record(&mut self, record: &Record) -> Result<()> {
        Value::from_bytes(&record.value)?;
        self.ensure_materialized(record.key)?;
        match record.kind {
            RecordKind::Add => self.memtable.add(record.key, record.value.clone()),
            RecordKind::Remove => self.memtable.remove(record.key, &record.value),
        }
        Ok(())
    }

    /// Make sure the memtable holds the full current value of `key`, not just
    /// a delta. This is what makes a flush a complete replacement generation.
    fn ensure_materialized(&mut self, key: Key) -> Result<()> {
        if self.memtable.get(key).is_none() {
            let cell = self.materialize(key)?;
            self.memtable.insert(key, cell);
        }
        Ok(())
    }

    fn materialize(&self, key: Key) -> Result<Cell> {
        if let Some(cell) = self.memtable.get(key) {
            return Ok(cell.clone());
        }
        for (_, filename) in self.levels.tablets_newest_first() {
            let tablet = self.tablet(&filename)?;
            if let Some(values) = tablet.get(key)? {
                return Ok(Cell::Many(
                    values
                        .into_iter()
                        .map(|value| value.to_bytes().to_vec())
                        .collect(),
                ));
            }
        }
        Ok(Cell::empty_many())
    }

    fn tablet(&self, filename: &str) -> Result<&Tablet> {
        self.tablets.get(filename).ok_or_else(|| {
            LsmError::Corrupt(format!("manifest references missing tablet {filename}"))
        })
    }
}

fn tablet_id_of(filename: &str) -> u64 {
    filename
        .strip_suffix(".tablet")
        .and_then(|stem| stem.parse::<u64>().ok())
        .unwrap_or(0)
}

fn remove_stale_tmp_files(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".tablet.tmp") {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}

fn remove_unreferenced_tablets(dir: &Path, tablets: &BTreeMap<String, Tablet>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".tablet") && !tablets.contains_key(name.as_ref()) {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("grahh-engine-{}-{id}", std::process::id()));
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

    fn key(node: u64, kind: u32) -> Key {
        Key::new(node, kind)
    }

    #[test]
    fn writes_survive_reopen() {
        let dir = TempDir::new();
        {
            let mut engine = Engine::open(&dir.0).unwrap();
            engine.add(key(1, 0), 2, 0).unwrap();
            engine.add(key(1, 0), 3, 0).unwrap();
            engine.add(key(2, 0), 3, 0).unwrap();
            engine.flush().unwrap();
            engine.add(key(1, 0), 4, 0).unwrap();
        }

        let engine = Engine::open(&dir.0).unwrap();
        assert_eq!(
            engine.get(key(1, 0)).unwrap(),
            vec![Value::new(2, 0), Value::new(3, 0), Value::new(4, 0)]
        );
        assert_eq!(engine.get(key(2, 0)).unwrap(), vec![Value::new(3, 0)]);
    }

    #[test]
    fn flush_then_reopen_sees_the_tablet() {
        let dir = TempDir::new();
        let mut engine = Engine::open(&dir.0).unwrap();
        engine.add(key(7, 2), 8, 0).unwrap();
        engine.flush().unwrap();
        assert_eq!(engine.tablet_count(), 1);
        assert_eq!(engine.memtable_len(), 0);
        drop(engine);

        let engine = Engine::open(&dir.0).unwrap();
        assert_eq!(engine.get(key(7, 2)).unwrap(), vec![Value::new(8, 0)]);
        assert_eq!(engine.tablet_count(), 1);
    }

    #[test]
    fn removal_shadows_older_tablet_rows() {
        let dir = TempDir::new();
        let mut engine = Engine::open(&dir.0).unwrap();
        engine.add(key(1, 0), 2, 0).unwrap();
        engine.add(key(1, 0), 3, 0).unwrap();
        engine.flush().unwrap();

        // Remove one item; the memtable materializes the full list first.
        engine.remove(key(1, 0), 2, 0).unwrap();
        assert_eq!(engine.get(key(1, 0)).unwrap(), vec![Value::new(3, 0)]);
        engine.flush().unwrap();
        assert_eq!(engine.get(key(1, 0)).unwrap(), vec![Value::new(3, 0)]);
        drop(engine);

        let engine = Engine::open(&dir.0).unwrap();
        assert_eq!(engine.get(key(1, 0)).unwrap(), vec![Value::new(3, 0)]);
    }

    #[test]
    fn removing_the_last_item_is_authoritative() {
        let dir = TempDir::new();
        let mut engine = Engine::open(&dir.0).unwrap();
        engine.add(key(1, 0), 2, 0).unwrap();
        engine.flush().unwrap();

        engine.remove(key(1, 0), 2, 0).unwrap();
        engine.flush().unwrap();
        assert!(engine.get(key(1, 0)).unwrap().is_empty());
        assert!(engine.contains_key(key(1, 0)).unwrap());
        drop(engine);

        let engine = Engine::open(&dir.0).unwrap();
        assert!(engine.get(key(1, 0)).unwrap().is_empty());
        assert!(engine.contains_key(key(1, 0)).unwrap());
    }

    #[test]
    fn parallel_edges_survive() {
        let dir = TempDir::new();
        let mut engine = Engine::open(&dir.0).unwrap();
        engine.add(key(1, 0), 2, 10).unwrap();
        engine.add(key(1, 0), 2, 11).unwrap();
        engine.flush().unwrap();

        assert_eq!(
            engine.get(key(1, 0)).unwrap(),
            vec![Value::new(2, 10), Value::new(2, 11)]
        );
    }

    #[test]
    fn scan_kind_respects_replace_semantics() {
        let dir = TempDir::new();
        let mut engine = Engine::open(&dir.0).unwrap();
        engine.add(key(1, 5), 10, 0).unwrap();
        engine.add(key(2, 5), 11, 0).unwrap();
        engine.add(key(1, 6), 99, 0).unwrap();
        engine.flush().unwrap();
        engine.add(key(1, 5), 12, 0).unwrap();

        let rows = engine.scan_kind(5).unwrap();
        assert_eq!(
            rows,
            vec![
                (1, Value::new(10, 0)),
                (1, Value::new(12, 0)),
                (2, Value::new(11, 0)),
            ]
        );
    }

    #[test]
    fn replay_skips_records_committed_by_the_manifest() {
        let dir = TempDir::new();
        {
            let mut engine = Engine::open(&dir.0).unwrap();
            engine.add(key(1, 0), 2, 0).unwrap();
            engine.add(key(1, 0), 3, 0).unwrap();
            engine.flush().unwrap();
        }

        // Simulate a crash after the manifest commit but before WAL truncation:
        // the WAL still holds the flushed records, plus one newer write.
        {
            let mut wal = Wal::open(dir.0.join("wal.log")).unwrap();
            for (seq, target) in [(1, 2), (2, 3), (3, 4)] {
                wal.append(&Record {
                    seq,
                    kind: RecordKind::Add,
                    key: key(1, 0),
                    value: Value::new(target, 0).to_bytes().to_vec(),
                })
                .unwrap();
            }
        }

        let engine = Engine::open(&dir.0).unwrap();
        // The first two records are already in the tablet and must not be
        // applied twice; seq 3 is newer and must survive.
        assert_eq!(
            engine.get(key(1, 0)).unwrap(),
            vec![Value::new(2, 0), Value::new(3, 0), Value::new(4, 0)]
        );
    }

    #[test]
    fn open_removes_orphan_tablets_and_stale_tmp_files() {
        let dir = TempDir::new();
        {
            let mut engine = Engine::open(&dir.0).unwrap();
            engine.add(key(1, 0), 2, 0).unwrap();
            engine.flush().unwrap();
        }

        // A tablet renamed into place before its manifest line, and a partial
        // flush that never reached the rename.
        fs::write(dir.0.join("00009999.tablet"), b"not a tablet").unwrap();
        fs::write(dir.0.join("00000042.tablet.tmp"), b"partial").unwrap();

        let engine = Engine::open(&dir.0).unwrap();
        assert!(!dir.0.join("00009999.tablet").exists());
        assert!(!dir.0.join("00000042.tablet.tmp").exists());
        assert_eq!(engine.get(key(1, 0)).unwrap(), vec![Value::new(2, 0)]);
    }

    #[test]
    fn reopen_continues_the_tablet_sequence() {
        let dir = TempDir::new();
        {
            let mut engine = Engine::open(&dir.0).unwrap();
            engine.add(key(1, 0), 2, 0).unwrap();
            engine.flush().unwrap();
        }
        {
            let mut engine = Engine::open(&dir.0).unwrap();
            engine.add(key(1, 0), 3, 0).unwrap();
            engine.flush().unwrap();
            assert_eq!(engine.tablet_count(), 2);
        }

        let engine = Engine::open(&dir.0).unwrap();
        assert_eq!(engine.tablet_count(), 2);
        assert_eq!(
            engine.get(key(1, 0)).unwrap(),
            vec![Value::new(2, 0), Value::new(3, 0)]
        );
    }

    #[test]
    fn auto_flush_keeps_the_memtable_bounded() {
        let dir = TempDir::new();
        let config = EngineConfig { memtable_bytes: 64 };
        let mut engine = Engine::open_with_config(&dir.0, config).unwrap();
        for target in 0..100_u64 {
            engine.add(key(1, 0), target, 0).unwrap();
        }
        assert_eq!(engine.get(key(1, 0)).unwrap().len(), 100);
        assert!(engine.tablet_count() > 0);
    }
}
