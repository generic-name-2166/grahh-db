//! Byte sink abstraction for codec output.
//!
//! A codec writes its encoded bytes through a [`ByteSink`] rather than
//! directly into a `Vec<u8>`. This lets the *same* encoder logic run against
//! two sinks:
//!
//! - [`VecSink`] — the real sink: appends bytes to an owned `Vec<u8>`.
//! - [`CountingSink`] — the probe sink: discards bytes but tallies how many
//!   there *would* be. This is how the codec picks a format by smallest size
//!   without allocating a real buffer for each candidate.
//!
//! Because encoders are deterministic, the byte count from the counting probe
//! equals the length the real sink would produce, so scoring is exact.

/// A destination for encoded bytes.
pub(crate) trait ByteSink {
    /// Write a raw byte slice.
    fn write(&mut self, bytes: &[u8]);

    /// Write a single byte.
    fn write_u8(&mut self, v: u8) {
        self.write(&(v.to_le_bytes()));
    }

    /// Write a `u32` little-endian.
    fn write_u32(&mut self, v: u32) {
        self.write(&v.to_le_bytes());
    }

    /// Write a `u64` little-endian.
    fn write_u64(&mut self, v: u64) {
        self.write(&v.to_le_bytes());
    }
}

/// Real sink: appends bytes into the referenced `Vec<u8>`.
pub(crate) struct VecSink<'a> {
    out: &'a mut Vec<u8>,
}

impl<'a> VecSink<'a> {
    pub(crate) fn new(out: &'a mut Vec<u8>) -> Self {
        Self { out }
    }
}

impl<'a> ByteSink for VecSink<'a> {
    fn write(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }
}

/// Probe sink: counts how many bytes would be written, storing none.
pub(crate) struct CountingSink {
    count: usize,
}

impl CountingSink {
    pub(crate) fn new() -> Self {
        Self { count: 0 }
    }

    pub(crate) fn reset(&mut self) {
        self.count = 0;
    }

    pub(crate) fn count(&self) -> usize {
        self.count
    }
}

impl ByteSink for CountingSink {
    fn write(&mut self, bytes: &[u8]) {
        self.count += bytes.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counting_matches_real_length() {
        let mut real = Vec::new();
        let mut vec = VecSink::new(&mut real);
        vec.write_u32(7);
        vec.write_u64(u64::MAX);
        vec.write_u8(1);

        let mut counting = CountingSink::new();
        counting.write_u32(7);
        counting.write_u64(u64::MAX);
        counting.write_u8(1);

        assert_eq!(counting.count(), real.len());
        assert_eq!(
            real,
            vec![
                7, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 1
            ]
        );
    }
}
