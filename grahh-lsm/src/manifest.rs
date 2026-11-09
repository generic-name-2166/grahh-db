//! The append-only manifest.
//!
//! The manifest is a text log of tablet membership changes. Replaying it on
//! open reconstructs the level layout without scanning the data directory.
//!
//! ```text
//! add 0 00000001.tablet
//! remove 0 00000001.tablet
//! ```

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use crate::error::{LsmError, Result};

/// One manifest operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestOp {
    /// A tablet became part of a level.
    Add {
        /// Level index (0 = memtable flush).
        level: u32,
        /// File name inside the database directory.
        filename: String,
        /// Highest WAL sequence contained in the tablet.
        ///
        /// This is the flush commit point: replay skips records at or below
        /// it, so a crash between the manifest append and WAL truncation cannot
        /// apply a flushed batch twice. It is `0` for manifests written before
        /// the field existed.
        max_seq: u64,
    },
    /// A tablet left a level.
    Remove {
        /// Level index.
        level: u32,
        /// File name inside the database directory.
        filename: String,
    },
}

impl ManifestOp {
    /// Format this operation as one manifest line (without the newline).
    pub fn encode(&self) -> String {
        match self {
            Self::Add {
                level,
                filename,
                max_seq,
            } => format!("add {level} {filename} {max_seq}"),
            Self::Remove { level, filename } => format!("remove {level} {filename}"),
        }
    }

    /// Parse one manifest line.
    pub fn decode(line: &str) -> Result<Self> {
        let mut parts = line.split_whitespace();
        let verb = parts
            .next()
            .ok_or_else(|| LsmError::Corrupt("empty manifest line".into()))?;
        let level = parts
            .next()
            .ok_or_else(|| LsmError::Corrupt("manifest line has no level".into()))?
            .parse::<u32>()
            .map_err(|_| LsmError::Corrupt(format!("invalid manifest level in {line:?}")))?;
        let filename = parts
            .next()
            .ok_or_else(|| LsmError::Corrupt("manifest line has no filename".into()))?
            .to_owned();
        match verb {
            "add" => {
                let max_seq = match parts.next() {
                    Some(token) => token.parse::<u64>().map_err(|_| {
                        LsmError::Corrupt(format!("invalid manifest sequence in {line:?}"))
                    })?,
                    None => 0,
                };
                if parts.next().is_some() {
                    return Err(LsmError::Corrupt(format!(
                        "trailing tokens in manifest line {line:?}"
                    )));
                }
                Ok(Self::Add {
                    level,
                    filename,
                    max_seq,
                })
            }
            "remove" => {
                if parts.next().is_some() {
                    return Err(LsmError::Corrupt(format!(
                        "trailing tokens in manifest line {line:?}"
                    )));
                }
                Ok(Self::Remove { level, filename })
            }
            other => Err(LsmError::Corrupt(format!(
                "unknown manifest operation {other:?}"
            ))),
        }
    }
}

/// An append-only manifest file.
#[derive(Debug)]
pub struct Manifest {
    file: File,
    path: PathBuf,
}

impl Manifest {
    /// Open (or create) the manifest at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        // Append mode matters: a plain read+write open leaves the cursor at
        // offset 0, so a later append would overwrite earlier manifest lines
        // after a reopen.
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)?;
        Ok(Self { file, path })
    }

    /// Append one operation and make it durable.
    pub fn append(&mut self, op: &ManifestOp) -> Result<()> {
        let mut line = op.encode();
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.flush()?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Replay every complete line in the manifest.
    ///
    /// A trailing line without a newline is treated as a crash artifact and
    /// ignored. A malformed complete line is corruption.
    pub fn replay(&self) -> Result<Vec<ManifestOp>> {
        let text = fs::read_to_string(&self.path)?;
        let mut ops = Vec::new();
        let complete = text.ends_with('\n');
        let lines: Vec<&str> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // If the file does not end in a newline, the final line is a
            // partial append and is not authoritative.
            if !complete && index + 1 == lines.len() {
                break;
            }
            ops.push(ManifestOp::decode(line)?);
        }
        Ok(ops)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TempFile(PathBuf);

    impl TempFile {
        fn new() -> Self {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("grahh-manifest-{}-{id}.log", std::process::id()));
            let _ = fs::remove_file(&path);
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[test]
    fn operations_round_trip_as_text() {
        let ops = vec![
            ManifestOp::Add {
                level: 0,
                filename: "00000001.tablet".into(),
                max_seq: 17,
            },
            ManifestOp::Remove {
                level: 0,
                filename: "00000001.tablet".into(),
            },
        ];
        for op in &ops {
            assert_eq!(&ManifestOp::decode(&op.encode()).unwrap(), op);
        }
        // Older manifests without the sequence field still parse.
        assert_eq!(
            ManifestOp::decode("add 0 old.tablet").unwrap(),
            ManifestOp::Add {
                level: 0,
                filename: "old.tablet".into(),
                max_seq: 0,
            }
        );
    }

    #[test]
    fn append_and_replay() {
        let file = TempFile::new();
        let mut manifest = Manifest::open(&file.0).unwrap();
        assert!(manifest.replay().unwrap().is_empty());

        manifest
            .append(&ManifestOp::Add {
                level: 0,
                filename: "a.tablet".into(),
                max_seq: 1,
            })
            .unwrap();
        manifest
            .append(&ManifestOp::Add {
                level: 1,
                filename: "b.tablet".into(),
                max_seq: 2,
            })
            .unwrap();

        let ops = manifest.replay().unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(
            ops[1],
            ManifestOp::Add {
                level: 1,
                filename: "b.tablet".into(),
                max_seq: 2,
            }
        );
    }

    #[test]
    fn reopening_appends_instead_of_overwriting() {
        let file = TempFile::new();
        {
            let mut manifest = Manifest::open(&file.0).unwrap();
            manifest
                .append(&ManifestOp::Add {
                    level: 0,
                    filename: "a.tablet".into(),
                    max_seq: 1,
                })
                .unwrap();
        }
        {
            let mut manifest = Manifest::open(&file.0).unwrap();
            manifest
                .append(&ManifestOp::Add {
                    level: 0,
                    filename: "b.tablet".into(),
                    max_seq: 2,
                })
                .unwrap();
        }

        let ops = Manifest::open(&file.0).unwrap().replay().unwrap();
        assert_eq!(
            ops,
            vec![
                ManifestOp::Add {
                    level: 0,
                    filename: "a.tablet".into(),
                    max_seq: 1,
                },
                ManifestOp::Add {
                    level: 0,
                    filename: "b.tablet".into(),
                    max_seq: 2,
                },
            ]
        );
    }

    #[test]
    fn partial_tail_is_ignored() {
        let file = TempFile::new();
        let mut manifest = Manifest::open(&file.0).unwrap();
        manifest
            .append(&ManifestOp::Add {
                level: 0,
                filename: "a.tablet".into(),
                max_seq: 1,
            })
            .unwrap();
        // Simulate a crash in the middle of the next line.
        let mut text = fs::read_to_string(&file.0).unwrap();
        text.push_str("add 0 b.tab");
        fs::write(&file.0, text).unwrap();

        let ops = manifest.replay().unwrap();
        assert_eq!(ops.len(), 1);
    }

    #[test]
    fn malformed_lines_are_rejected() {
        for line in ["", "add", "add x a", "nope 0 a", "add 0 a extra"] {
            assert!(ManifestOp::decode(line).is_err(), "line {line:?}");
        }
    }
}
