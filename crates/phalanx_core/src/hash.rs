//! Canonical state hashing (ADR-0001 Decision §8, `RtsEngineDoc.md` §13.3).
//!
//! [`CanonicalHasher`] is a dependency-free FNV-1a 64-bit hasher over an
//! explicitly ordered byte stream. [`CanonicalSerialize`] defines how
//! authoritative types write themselves into that stream: fields are written
//! in a fixed, documented order and byte strings are length-prefixed, so the
//! hash never depends on pointer values, padding, memory layout, or hash-map
//! iteration order. Renderer state, GPU handles, and wall-clock data do not
//! exist in the authoritative simulation types and can therefore never enter
//! the canonical hash.
//!
//! The hash detects divergence; it is not a cryptographic checksum.

use std::collections::BTreeMap;

const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

/// An FNV-1a 64-bit hasher over a canonical byte stream.
#[derive(Clone, Debug)]
pub struct CanonicalHasher {
    hash: u64,
}

impl CanonicalHasher {
    /// Creates a hasher with the FNV-1a offset basis.
    #[must_use]
    pub const fn new() -> Self {
        Self { hash: FNV_OFFSET }
    }

    /// Writes one `u64` in little-endian byte order.
    pub fn write_u64(&mut self, value: u64) {
        self.mix_bytes(&value.to_le_bytes());
    }

    /// Writes one `u32` in little-endian byte order.
    pub fn write_u32(&mut self, value: u32) {
        self.mix_bytes(&value.to_le_bytes());
    }

    /// Writes one boolean as a single byte.
    pub fn write_bool(&mut self, value: bool) {
        self.mix_bytes(&[u8::from(value)]);
    }

    /// Writes a length-prefixed byte string so field boundaries are
    /// unambiguous.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.write_u64(bytes.len() as u64);
        self.mix_bytes(bytes);
    }

    fn mix_bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.hash ^= u64::from(*byte);
            self.hash = self.hash.wrapping_mul(FNV_PRIME);
        }
    }

    /// Returns the current hash value.
    #[must_use]
    pub const fn finish(&self) -> u64 {
        self.hash
    }
}

impl Default for CanonicalHasher {
    fn default() -> Self {
        Self::new()
    }
}

/// A type that can write its authoritative fields into a canonical hash
/// stream in a fixed, documented order.
pub trait CanonicalSerialize {
    /// Writes this value's canonical fields.
    fn write_canonical(&self, hasher: &mut CanonicalHasher);
}

impl CanonicalSerialize for u64 {
    fn write_canonical(&self, hasher: &mut CanonicalHasher) {
        hasher.write_u64(*self);
    }
}

impl CanonicalSerialize for u32 {
    fn write_canonical(&self, hasher: &mut CanonicalHasher) {
        hasher.write_u32(*self);
    }
}

impl CanonicalSerialize for bool {
    fn write_canonical(&self, hasher: &mut CanonicalHasher) {
        hasher.write_bool(*self);
    }
}

impl CanonicalSerialize for () {
    fn write_canonical(&self, _hasher: &mut CanonicalHasher) {}
}

impl<T: CanonicalSerialize> CanonicalSerialize for Vec<T> {
    fn write_canonical(&self, hasher: &mut CanonicalHasher) {
        hasher.write_u64(self.len() as u64);
        for item in self {
            item.write_canonical(hasher);
        }
    }
}

/// Maps serialize in key order, so the hash is independent of insertion order.
impl<K: CanonicalSerialize + Ord, V: CanonicalSerialize> CanonicalSerialize for BTreeMap<K, V> {
    fn write_canonical(&self, hasher: &mut CanonicalHasher) {
        hasher.write_u64(self.len() as u64);
        for (key, value) in self {
            key.write_canonical(hasher);
            value.write_canonical(hasher);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_streams_hash_identically() {
        let mut first = CanonicalHasher::new();
        let mut second = CanonicalHasher::new();
        for value in 0..100_u64 {
            first.write_u64(value);
            second.write_u64(value);
        }
        first.write_bool(true);
        second.write_bool(true);
        assert_eq!(first.finish(), second.finish());
    }

    #[test]
    fn different_streams_diverge() {
        let mut first = CanonicalHasher::new();
        let mut second = CanonicalHasher::new();
        first.write_u64(1);
        second.write_u64(2);
        assert_ne!(first.finish(), second.finish());
    }

    #[test]
    fn length_prefix_disambiguates_field_boundaries() {
        let mut split = CanonicalHasher::new();
        split.write_u64(0x0102);
        let mut joined = CanonicalHasher::new();
        joined.write_bytes(&[0x01, 0x02]);
        assert_ne!(split.finish(), joined.finish());
    }

    #[test]
    fn map_hash_is_independent_of_insertion_order() {
        let mut first: BTreeMap<u64, u64> = BTreeMap::new();
        let mut second: BTreeMap<u64, u64> = BTreeMap::new();
        for i in 0..50_u64 {
            first.insert(i, i * 7);
        }
        for i in (0..50_u64).rev() {
            second.insert(i, i * 7);
        }

        let mut hasher_a = CanonicalHasher::new();
        let mut hasher_b = CanonicalHasher::new();
        first.write_canonical(&mut hasher_a);
        second.write_canonical(&mut hasher_b);
        assert_eq!(hasher_a.finish(), hasher_b.finish());
    }
}
