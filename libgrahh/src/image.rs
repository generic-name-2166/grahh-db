//! The database image envelope: a header plus a region directory.
//!
//! Each region is self-describing on its own, but nothing inside one says what
//! else shares the file. The envelope is that missing piece: it lists every
//! region's tag, byte offset and length.
//!
//! ```text
//! magic | version u32 | region_count u32
//! region_count x [ tag u32 | offset u64 | length u64 ]
//! region payloads, back to back
//! ```
//!
//! Offsets are absolute from the start of the image and regions are contiguous,
//! and that is checked on read: a directory with a gap, an overlap, or an entry
//! pointing past the end is rejected rather than interpreted.

use grahh_persist::{HEADER_LEN, PersistenceError, Reader, Writer};

/// Region tag for the label index.
pub(crate) const TAG_LABELS: u32 = 1;
/// Region tag for the adjacency registry.
pub(crate) const TAG_ADJACENCY: u32 = 2;

/// One directory entry: tag (u32) + offset (u64) + length (u64).
pub(crate) const ENTRY_LEN: usize = 4 + 8 + 8;

/// Bytes before the first region: header + region count + the directory.
fn regions_start(region_count: usize) -> usize {
    HEADER_LEN + 4 + region_count * ENTRY_LEN
}

/// A serializer for one region: appends its bytes to the image.
pub(crate) type RegionWriter<'a> = &'a dyn Fn(&mut Vec<u8>) -> Result<(), PersistenceError>;

/// One region located inside an image.
pub(crate) struct Region<'a> {
    /// What the region claims to be.
    pub(crate) tag: u32,
    /// The region's bytes, header included.
    pub(crate) bytes: &'a [u8],
}

/// Write a whole image, appending to `out`.
///
/// The directory is written first and backpatched as regions land, so no
/// intermediate buffers are needed.
pub(crate) fn write_image(
    out: &mut Vec<u8>,
    regions: &[(u32, RegionWriter<'_>)],
) -> Result<(), PersistenceError> {
    let mut w: Writer<'_> = Writer::new(out);
    w.header();
    w.u32(regions.len() as u32);

    let mut entries: Vec<usize> = Vec::with_capacity(regions.len());
    for _ in regions {
        entries.push(w.position());
        w.u32(0); // tag
        w.u64(0); // offset
        w.u64(0); // length
    }

    for (index, (tag, write)) in regions.iter().enumerate() {
        let start: usize = w.position();
        write(w.sink())?;
        let len: usize = w.position() - start;

        let mut entry: [u8; ENTRY_LEN] = [0; ENTRY_LEN];
        entry[..4].copy_from_slice(&tag.to_le_bytes());
        entry[4..12].copy_from_slice(&(start as u64).to_le_bytes());
        entry[12..].copy_from_slice(&(len as u64).to_le_bytes());
        w.patch(entries[index], &entry);
    }
    Ok(())
}

/// Parse and validate an image's envelope, returning its regions in order.
pub(crate) fn read_image(bytes: &[u8]) -> Result<Vec<Region<'_>>, PersistenceError> {
    let mut r: Reader<'_> = Reader::new(bytes);
    r.header()?;
    let count: usize = r.u32()? as usize;
    if count > r.remaining() / ENTRY_LEN {
        return Err(PersistenceError::Corrupt {
            reason: "region count exceeds the remaining bytes",
        });
    }

    let mut regions: Vec<Region<'_>> = Vec::with_capacity(count);
    let mut expected: usize = regions_start(count);
    for _ in 0..count {
        let tag: u32 = r.u32()?;
        let offset: usize = r.len_u64()?;
        let len: usize = r.len_u64()?;

        if offset != expected {
            return Err(PersistenceError::Corrupt {
                reason: "region offsets are not contiguous",
            });
        }
        let end: usize = offset.checked_add(len).ok_or(PersistenceError::Corrupt {
            reason: "region length overflows",
        })?;
        if end > bytes.len() {
            return Err(PersistenceError::Corrupt {
                reason: "region extends past the end of the image",
            });
        }
        if regions.iter().any(|region| region.tag == tag) {
            return Err(PersistenceError::Corrupt {
                reason: "duplicate region tag",
            });
        }

        regions.push(Region {
            tag,
            bytes: &bytes[offset..end],
        });
        expected = end;
    }

    if expected != bytes.len() {
        return Err(PersistenceError::TrailingBytes {
            extra: bytes.len() - expected,
        });
    }
    Ok(regions)
}
