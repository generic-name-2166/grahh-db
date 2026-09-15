//! Format tags and decode dispatch.
//!
//! Every encoded column begins with a single tag byte identifying which format
//! (codec) produced it, so a decoder can reverse the encoding without knowing
//! the plan in advance. This is what keeps the *open set* of formats
//! future-proof: adding a new format later just assigns a new tag, and old
//! data (written with old tags) still decodes — no migration.
//!
//! A 1-byte tag is plenty: `decode` for a given `T` already knows the type, so
//! the tag only needs to distinguish the handful of formats valid for that
//! type. 
//!
//! In this staged implementation the four formats below are the whole u64 tag
//! space. When string/enum types arrive they bring their own tag spaces.

use crate::CodecError;

/// Tag for the delta / frame-of-reference format.
pub(crate) const TAG_FOR: u8 = 0;
/// Tag for the fixed-width bit-packing format.
pub(crate) const TAG_BITPACK: u8 = 1;
/// Tag for the run-length format.
pub(crate) const TAG_RLE: u8 = 2;
/// Tag for the plain (raw) format.
pub(crate) const TAG_PLAIN: u8 = 3;

/// Dispatch a `u64` payload (after its tag byte) to the matching format's
/// decoder.
pub(crate) fn dispatch_decode_u64(
    bytes: &[u8],
    out: &mut Vec<u64>,
) -> Result<(), CodecError> {
    if bytes.is_empty() {
        return Err(CodecError::Truncated);
    }
    let tag = bytes[0];
    let payload = &bytes[1..];
    match tag {
        TAG_FOR => crate::for_::decode_u64(payload, out),
        TAG_BITPACK => crate::bitpack::decode_u64(payload, out),
        TAG_RLE => crate::rle::decode_u64(payload, out),
        TAG_PLAIN => crate::plain::decode_u64(payload, out),
        _ => Err(CodecError::Corrupt),
    }
}