//! Level bookkeeping for the LSM tree.
//!
//! The manifest records membership changes; this module turns that sequence
//! into the level layout the read path walks. Compaction is not implemented
//! yet, so in practice every tablet lives in L0. The ordering rule is still
//! encoded here: L0 is checked newest-first, then L1, L2, ...

use crate::manifest::ManifestOp;

/// The current tablets, grouped by level.
#[derive(Debug, Default)]
pub struct Levels {
    /// `levels[0]` is L0; each vector is in insertion order.
    levels: Vec<Vec<String>>,
}

impl Levels {
    /// Create an empty layout.
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one manifest operation.
    pub fn apply(&mut self, op: &ManifestOp) {
        match op {
            ManifestOp::Add {
                level, filename, ..
            } => self.add(*level, filename.clone()),
            ManifestOp::Remove { level, filename } => self.remove(*level, filename),
        }
    }

    /// Add a tablet to a level.
    pub fn add(&mut self, level: u32, filename: String) {
        let index = level as usize;
        if self.levels.len() <= index {
            self.levels.resize_with(index + 1, Vec::new);
        }
        if !self.levels[index].contains(&filename) {
            self.levels[index].push(filename);
        }
    }

    /// Remove a tablet from a level.
    pub fn remove(&mut self, level: u32, filename: &str) {
        if let Some(level) = self.levels.get_mut(level as usize) {
            level.retain(|name| name != filename);
        }
    }

    /// Tablets in read order: L0 newest-first, then higher levels in insertion
    /// order.
    pub fn tablets_newest_first(&self) -> Vec<(u32, String)> {
        let mut ordered = Vec::new();
        if let Some(l0) = self.levels.first() {
            for filename in l0.iter().rev() {
                ordered.push((0, filename.clone()));
            }
        }
        for (index, level) in self.levels.iter().enumerate().skip(1) {
            for filename in level {
                ordered.push((index as u32, filename.clone()));
            }
        }
        ordered
    }

    /// Number of tablets across all levels.
    pub fn tablet_count(&self) -> usize {
        self.levels.iter().map(Vec::len).sum()
    }

    /// Number of levels currently allocated (including empty holes).
    pub fn level_count(&self) -> usize {
        self.levels.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(level: u32, name: &str) -> ManifestOp {
        ManifestOp::Add {
            level,
            filename: name.into(),
            max_seq: 0,
        }
    }

    #[test]
    fn l0_is_newest_first() {
        let mut levels = Levels::new();
        levels.apply(&add(0, "a"));
        levels.apply(&add(0, "b"));
        levels.apply(&add(0, "c"));
        let ordered: Vec<String> = levels
            .tablets_newest_first()
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        assert_eq!(ordered, vec!["c", "b", "a"]);
    }

    #[test]
    fn higher_levels_follow_l0() {
        let mut levels = Levels::new();
        levels.apply(&add(1, "l1"));
        levels.apply(&add(0, "l0"));
        let ordered: Vec<(u32, String)> = levels.tablets_newest_first();
        assert_eq!(ordered, vec![(0, "l0".into()), (1, "l1".into())]);
    }

    #[test]
    fn remove_updates_membership() {
        let mut levels = Levels::new();
        levels.apply(&add(0, "a"));
        levels.apply(&add(0, "b"));
        levels.apply(&ManifestOp::Remove {
            level: 0,
            filename: "a".into(),
        });
        assert_eq!(levels.tablet_count(), 1);
        assert_eq!(levels.tablets_newest_first(), vec![(0, "b".into())]);
    }
}
