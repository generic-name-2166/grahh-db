//! A small, deterministic bloom filter over 12-byte tablet keys.
//!
//! Every tablet carries one of these. A negative answer is final; a positive
//! answer is checked against the tablet's exact key/length directory before a
//! chunk is decoded, so false positives never reach the read path.
//!
//! The filter uses double hashing: one FNV-1a hash of the encoded key, a
//! splitmix64-derived step, and `hash_count` probes. It is deterministic across
//! processes and platforms.

use grahh_persist::{Reader, Writer};

use crate::{
    error::{LsmError, Result},
    key::Key,
};

/// Target bits per key. 10 bits/key is the usual space/accuracy sweet spot.
const BITS_PER_KEY: u64 = 10;
/// Number of probes per lookup.
const HASH_COUNT: u32 = 7;
/// Minimum filter size, so an empty tablet still has a valid serialized form.
const MIN_BITS: u64 = 64;
/// Upper bound on a filter loaded from disk, so a corrupt count cannot drive an
/// absurd allocation.
const MAX_BITS: u64 = 1 << 34;

/// A bloom filter over `(node_id, kind_id)` keys.
#[derive(Debug, Clone)]
pub struct BloomFilter {
    bit_count: u64,
    hash_count: u32,
    bits: Box<[u8]>,
}

impl BloomFilter {
    /// Build a filter from an iterator of keys (duplicates are harmless).
    pub fn build<'a, I>(keys: I) -> Self
    where
        I: IntoIterator<Item = &'a Key>,
    {
        let keys: Vec<&Key> = keys.into_iter().collect();
        let bit_count = ((keys.len() as u64) * BITS_PER_KEY)
            .max(MIN_BITS)
            .next_multiple_of(8);
        let hash_count = HASH_COUNT;
        let byte_count = usize::try_from(bit_count / 8).unwrap_or(usize::MAX);
        let mut bits = vec![0_u8; byte_count].into_boxed_slice();

        for key in keys {
            for position in positions(key, bit_count, hash_count) {
                let index = usize::try_from(position / 8).expect("filter fits usize");
                let mask = 1_u8 << (position % 8);
                bits[index] |= mask;
            }
        }

        Self {
            bit_count,
            hash_count,
            bits,
        }
    }

    /// `true` if the filter might contain `key`. Never returns a false
    /// negative; false positives are resolved by the exact key directory.
    pub fn may_contain(&self, key: Key) -> bool {
        positions(&key, self.bit_count, self.hash_count).all(|position| {
            let index = usize::try_from(position / 8).expect("filter fits usize");
            let mask = 1_u8 << (position % 8);
            self.bits[index] & mask != 0
        })
    }

    /// Serialize as `[bit_count u64][hash_count u32][len-prefixed bits]`.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut w = Writer::new(&mut out);
        w.u64(self.bit_count);
        w.u32(self.hash_count);
        w.len_prefixed(&self.bits);
        out
    }

    /// Rebuild a filter written by [`BloomFilter::serialize`].
    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let bit_count = r.u64()?;
        let hash_count = r.u32()?;
        if bit_count == 0 || bit_count > MAX_BITS || bit_count % 8 != 0 {
            return Err(LsmError::Corrupt(
                "bloom bit count outside the supported range".into(),
            ));
        }
        if !(1..=32).contains(&hash_count) {
            return Err(LsmError::Corrupt("bloom hash count outside 1..=32".into()));
        }
        let bits = r.len_prefixed()?;
        let expected = usize::try_from(bit_count / 8)
            .map_err(|_| LsmError::Corrupt("bloom bit count does not fit usize".into()))?;
        if bits.len() != expected {
            return Err(LsmError::Corrupt(format!(
                "bloom has {} bytes, expected {expected}",
                bits.len()
            )));
        }
        r.finish()?;
        Ok(Self {
            bit_count,
            hash_count,
            bits: bits.to_vec().into_boxed_slice(),
        })
    }
}

/// The `hash_count` bit positions for a key.
fn positions(key: &Key, bit_count: u64, hash_count: u32) -> impl Iterator<Item = u64> {
    let h1 = fnv1a(&key.to_bytes());
    // Force the step odd so it is coprime with any power-of-two bit count and
    // still spreads well for non-power-of-two counts.
    let h2 = splitmix64(h1) | 1;
    (0..hash_count).map(move |i| {
        h1.wrapping_add((i as u64).wrapping_mul(h2))
            .wrapping_rem(bit_count)
    })
}

/// FNV-1a, 64-bit.
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// splitmix64 finalizer, used to derive a second independent-ish hash.
fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_false_negatives() {
        let keys: Vec<Key> = (0..1_000).map(|i| Key::new(i, (i % 7) as u32)).collect();
        let bloom = BloomFilter::build(&keys);
        for key in &keys {
            assert!(bloom.may_contain(*key), "false negative for {key:?}");
        }
    }

    #[test]
    fn empty_filter_contains_nothing() {
        let bloom = BloomFilter::build(std::iter::empty());
        for i in 0..100 {
            assert!(!bloom.may_contain(Key::new(i, 0)));
        }
    }

    #[test]
    fn round_trips_through_bytes() {
        let keys: Vec<Key> = (0..200).map(|i| Key::new(i * 3, 1)).collect();
        let bloom = BloomFilter::build(&keys);
        let bytes = bloom.serialize();
        let back = BloomFilter::deserialize(&bytes).unwrap();
        assert_eq!(back.bit_count, bloom.bit_count);
        assert_eq!(back.hash_count, bloom.hash_count);
        for key in &keys {
            assert!(back.may_contain(*key));
        }
    }

    #[test]
    fn false_positive_rate_is_reasonable() {
        let keys: Vec<Key> = (0..2_000).map(|i| Key::new(i, 0)).collect();
        let bloom = BloomFilter::build(&keys);
        let mut false_positives = 0;
        for i in 2_000..12_000 {
            if bloom.may_contain(Key::new(i, 0)) {
                false_positives += 1;
            }
        }
        // 10 bits/key / 7 probes is ~1% for an ideal filter; allow generous
        // slack for this simple construction.
        assert!(
            false_positives < 600,
            "{false_positives} false positives in 10k lookups"
        );
    }

    #[test]
    fn malformed_filters_are_rejected() {
        let mut out = Vec::new();
        let mut w = Writer::new(&mut out);
        w.u64(0);
        w.u32(7);
        w.len_prefixed(&[]);
        assert!(BloomFilter::deserialize(&out).is_err());

        let mut out = Vec::new();
        let mut w = Writer::new(&mut out);
        w.u64(64);
        w.u32(0);
        w.len_prefixed(&[0; 8]);
        assert!(BloomFilter::deserialize(&out).is_err());
    }
}
