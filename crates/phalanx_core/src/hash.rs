//! Versioned canonical encoding and the state-digest interface (ADR-0001
//! Decision §8, `RtsEngineDoc.md` §13.3; issue #49).
//!
//! # Encoding contract
//!
//! [`CanonicalSerialize`] defines authoritative field traversal: fields are
//! written in a fixed, documented order; integers are little-endian; byte
//! strings and collections are length-prefixed (`u64`); enum variants write an
//! explicit `u32` tag first. The same traversal feeds any [`CanonicalSink`]:
//! a [`HashSink`] for divergence digests, or a [`SnapshotSink`] for snapshot
//! bytes — so hashing and snapshot encoding cannot silently disagree about
//! authoritative fields. The encoding is versioned by
//! [`CANONICAL_ENCODING_VERSION`]; any change to field order, lengths, tags, or
//! endianness must bump it.
//!
//! # Digest interface
//!
//! [`Digest`] hides the concrete algorithm and width behind a stable type with
//! an explicit [`DigestAlgorithm`]. FNV-1a 64 is a fast *divergence* check, not
//! a cryptographic/content-identity hash; it is not tamper-resistant. Replays
//! and saves record the algorithm and reject mismatches.

use std::collections::BTreeMap;

/// Version of the canonical field encoding. Bump on any incompatible change
/// (field order, lengths, enum tags, endianness); recorded in snapshots and
/// replay metadata.
pub const CANONICAL_ENCODING_VERSION: u32 = 1;

const FNV64_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
const FNV64_PRIME: u64 = 0x0000_0100_0000_01B3;
const FNV32_OFFSET: u32 = 0x811C_9DC5;
const FNV32_PRIME: u32 = 0x0100_0193;

/// A sink that receives the canonical byte stream. All authoritative field
/// traversal goes through this trait, never through a concrete hasher.
pub trait CanonicalSink {
    /// Writes one `u64` in little-endian byte order.
    fn write_u64(&mut self, value: u64);
    /// Writes one `u32` in little-endian byte order.
    fn write_u32(&mut self, value: u32);
    /// Writes one boolean as a single byte.
    fn write_bool(&mut self, value: bool);
    /// Writes a length-prefixed (`u64`) byte string.
    fn write_bytes(&mut self, bytes: &[u8]);
}

/// A type that can write its authoritative fields into a canonical stream in a
/// fixed, documented order.
pub trait CanonicalSerialize {
    /// Writes this value's canonical fields.
    fn write_canonical(&self, sink: &mut dyn CanonicalSink);
}

/// The digest algorithm and width used for a [`Digest`]. Versioned so replays
/// and saves can detect and reject incompatible algorithms.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum DigestAlgorithm {
    /// FNV-1a 64-bit (8-byte digest). Fast divergence check; NOT cryptographic.
    Fnv1a64 = 1,
    /// FNV-1a 32-bit (4-byte digest). Fast divergence check; NOT cryptographic.
    Fnv1a32 = 2,
}

impl DigestAlgorithm {
    /// The algorithm used by new digests in this build.
    #[must_use]
    pub const fn current() -> Self {
        Self::Fnv1a64
    }

    /// Returns the digest width in bytes for this algorithm.
    #[must_use]
    pub const fn width(self) -> usize {
        match self {
            Self::Fnv1a64 => 8,
            Self::Fnv1a32 => 4,
        }
    }
}

impl CanonicalSerialize for DigestAlgorithm {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u32(*self as u32);
    }
}

impl CanonicalDecode for DigestAlgorithm {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        match reader.read_u32()? {
            1 => Ok(Self::Fnv1a64),
            2 => Ok(Self::Fnv1a32),
            other => Err(CanonicalError::UnknownDigestAlgorithm(other)),
        }
    }
}

/// A state digest: an algorithm tag plus the algorithm's output bytes. The
/// concrete algorithm and width are hidden behind this type.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Digest {
    algorithm: DigestAlgorithm,
    value: Vec<u8>,
}

impl Digest {
    /// Returns the algorithm that produced this digest.
    #[must_use]
    pub const fn algorithm(&self) -> DigestAlgorithm {
        self.algorithm
    }

    /// Returns the raw digest bytes (algorithm-dependent width).
    #[must_use]
    pub fn value(&self) -> &[u8] {
        &self.value
    }

    /// Returns an error if this digest was not produced by `algorithm`.
    pub fn expect_algorithm(&self, algorithm: DigestAlgorithm) -> Result<(), DigestMismatch> {
        if self.algorithm == algorithm {
            Ok(())
        } else {
            Err(DigestMismatch {
                expected: algorithm,
                found: self.algorithm,
            })
        }
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:?}:0x", self.algorithm)?;
        for byte in &self.value {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// A digest was produced by a different algorithm than expected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DigestMismatch {
    /// The expected algorithm.
    pub expected: DigestAlgorithm,
    /// The algorithm found in the digest.
    pub found: DigestAlgorithm,
}

impl std::fmt::Display for DigestMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "digest algorithm mismatch: expected {:?}, found {:?}",
            self.expected, self.found
        )
    }
}

impl std::error::Error for DigestMismatch {}

impl CanonicalSerialize for Digest {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        self.algorithm.write_canonical(sink);
        sink.write_bytes(&self.value);
    }
}

impl CanonicalDecode for Digest {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        let algorithm = DigestAlgorithm::read_canonical(reader)?;
        let value = reader.read_bytes()?.to_vec();
        Ok(Self { algorithm, value })
    }
}

/// Computes the canonical digest of a value with the current algorithm.
#[must_use]
pub fn canonical_digest<T: CanonicalSerialize + ?Sized>(value: &T) -> Digest {
    let mut sink = HashSink::new(DigestAlgorithm::current());
    value.write_canonical(&mut sink);
    sink.finish()
}

enum HashState {
    Fnv1a64(u64),
    Fnv1a32(u32),
}

/// A [`CanonicalSink`] that computes a digest. Holds the concrete algorithm.
pub struct HashSink {
    algorithm: DigestAlgorithm,
    state: HashState,
}

impl HashSink {
    /// Creates a hash sink for the given algorithm.
    #[must_use]
    pub fn new(algorithm: DigestAlgorithm) -> Self {
        let state = match algorithm {
            DigestAlgorithm::Fnv1a64 => HashState::Fnv1a64(FNV64_OFFSET),
            DigestAlgorithm::Fnv1a32 => HashState::Fnv1a32(FNV32_OFFSET),
        };
        Self { algorithm, state }
    }

    /// Finishes the stream and returns the digest.
    #[must_use]
    pub fn finish(self) -> Digest {
        let value = match self.state {
            HashState::Fnv1a64(state) => state.to_le_bytes().to_vec(),
            HashState::Fnv1a32(state) => state.to_le_bytes().to_vec(),
        };
        Digest {
            algorithm: self.algorithm,
            value,
        }
    }

    fn mix_bytes(&mut self, bytes: &[u8]) {
        match &mut self.state {
            HashState::Fnv1a64(state) => {
                for byte in bytes {
                    *state ^= u64::from(*byte);
                    *state = state.wrapping_mul(FNV64_PRIME);
                }
            }
            HashState::Fnv1a32(state) => {
                for byte in bytes {
                    *state ^= u32::from(*byte);
                    *state = state.wrapping_mul(FNV32_PRIME);
                }
            }
        }
    }
}

impl CanonicalSink for HashSink {
    fn write_u64(&mut self, value: u64) {
        self.mix_bytes(&value.to_le_bytes());
    }

    fn write_u32(&mut self, value: u32) {
        self.mix_bytes(&value.to_le_bytes());
    }

    fn write_bool(&mut self, value: bool) {
        self.mix_bytes(&[u8::from(value)]);
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        self.write_u64(bytes.len() as u64);
        self.mix_bytes(bytes);
    }
}

/// A [`CanonicalSink`] that records the canonical byte stream verbatim, for
/// snapshots. Produces the same field stream a [`HashSink`] would consume, so
/// the traversal cannot diverge between hashing and snapshots.
#[derive(Default)]
pub struct SnapshotSink {
    /// The recorded canonical bytes.
    pub bytes: Vec<u8>,
}

impl SnapshotSink {
    /// Creates an empty snapshot sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl CanonicalSink for SnapshotSink {
    fn write_u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn write_u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn write_bool(&mut self, value: bool) {
        self.bytes.push(u8::from(value));
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        self.write_u64(bytes.len() as u64);
        self.bytes.extend_from_slice(bytes);
    }
}

/// Errors produced when decoding a canonical byte stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CanonicalError {
    /// The stream ended before a complete field.
    UnexpectedEof,
    /// A boolean was encoded as a byte other than 0/1.
    InvalidBool(u8),
    /// A length prefix exceeded the remaining bytes or addressable memory.
    InvalidLength(u64),
    /// A byte string was not valid UTF-8.
    InvalidUtf8,
    /// An unknown enum tag or digest algorithm.
    UnknownTag(u32),
    /// An unknown digest algorithm tag.
    UnknownDigestAlgorithm(u32),
    /// Bytes remained after decoding a complete value.
    TrailingBytes(usize),
}

impl std::fmt::Display for CanonicalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnexpectedEof => formatter.write_str("canonical stream ended early"),
            Self::InvalidBool(byte) => write!(formatter, "invalid bool byte {byte}"),
            Self::InvalidLength(len) => write!(formatter, "invalid length prefix {len}"),
            Self::InvalidUtf8 => formatter.write_str("invalid UTF-8 in canonical string"),
            Self::UnknownTag(tag) => write!(formatter, "unknown enum tag {tag}"),
            Self::UnknownDigestAlgorithm(tag) => {
                write!(formatter, "unknown digest algorithm {tag}")
            }
            Self::TrailingBytes(count) => write!(formatter, "{count} trailing bytes after value"),
        }
    }
}

impl std::error::Error for CanonicalError {}

/// A cursor over a canonical byte stream, for decoding snapshots.
pub struct CanonicalReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> CanonicalReader<'a> {
    /// Creates a reader over the given bytes.
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    /// Returns the number of unread bytes.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    /// Fails if unread bytes remain.
    pub fn finish(&self) -> Result<(), CanonicalError> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(CanonicalError::TrailingBytes(self.remaining()))
        }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], CanonicalError> {
        if self.remaining() < count {
            return Err(CanonicalError::UnexpectedEof);
        }
        let slice = &self.bytes[self.position..self.position + count];
        self.position += count;
        Ok(slice)
    }

    /// Reads one little-endian `u64`.
    pub fn read_u64(&mut self) -> Result<u64, CanonicalError> {
        let slice = self.take(8)?;
        Ok(u64::from_le_bytes(slice.try_into().expect("8 bytes")))
    }

    /// Reads one little-endian `u32`.
    pub fn read_u32(&mut self) -> Result<u32, CanonicalError> {
        let slice = self.take(4)?;
        Ok(u32::from_le_bytes(slice.try_into().expect("4 bytes")))
    }

    /// Reads one boolean byte.
    pub fn read_bool(&mut self) -> Result<bool, CanonicalError> {
        match self.take(1)?[0] {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(CanonicalError::InvalidBool(other)),
        }
    }

    /// Reads a length-prefixed byte string.
    pub fn read_bytes(&mut self) -> Result<&'a [u8], CanonicalError> {
        let length = self.read_u64()?;
        let length = usize::try_from(length).map_err(|_| CanonicalError::InvalidLength(length))?;
        if length > self.remaining() {
            return Err(CanonicalError::InvalidLength(length as u64));
        }
        self.take(length)
    }
}

/// A type that can be decoded from a canonical byte stream.
pub trait CanonicalDecode: Sized {
    /// Reads one value from the stream.
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError>;
}

/// Encodes a value into canonical snapshot bytes.
#[must_use]
pub fn canonical_encode<T: CanonicalSerialize + ?Sized>(value: &T) -> Vec<u8> {
    let mut sink = SnapshotSink::new();
    value.write_canonical(&mut sink);
    sink.bytes
}

/// Decodes a value from canonical snapshot bytes, requiring the stream to be
/// fully consumed.
pub fn canonical_decode<T: CanonicalDecode>(bytes: &[u8]) -> Result<T, CanonicalError> {
    let mut reader = CanonicalReader::new(bytes);
    let value = T::read_canonical(&mut reader)?;
    reader.finish()?;
    Ok(value)
}

impl CanonicalSerialize for u64 {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u64(*self);
    }
}

impl CanonicalDecode for u64 {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        reader.read_u64()
    }
}

impl CanonicalSerialize for u32 {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u32(*self);
    }
}

impl CanonicalDecode for u32 {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        reader.read_u32()
    }
}

impl CanonicalSerialize for u8 {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_bytes(&[*self]);
    }
}

impl CanonicalDecode for u8 {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        let bytes = reader.read_bytes()?;
        match bytes {
            [byte] => Ok(*byte),
            _ => Err(CanonicalError::UnknownTag(bytes.len() as u32)),
        }
    }
}

impl CanonicalSerialize for bool {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_bool(*self);
    }
}

impl CanonicalDecode for bool {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        reader.read_bool()
    }
}

impl CanonicalSerialize for () {
    fn write_canonical(&self, _sink: &mut dyn CanonicalSink) {}
}

impl CanonicalDecode for () {
    fn read_canonical(_reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        Ok(())
    }
}

impl CanonicalSerialize for str {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_bytes(self.as_bytes());
    }
}

impl CanonicalSerialize for String {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_bytes(self.as_bytes());
    }
}

impl CanonicalDecode for String {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        let bytes = reader.read_bytes()?;
        String::from_utf8(bytes.to_vec()).map_err(|_| CanonicalError::InvalidUtf8)
    }
}

impl<T: CanonicalSerialize> CanonicalSerialize for Vec<T> {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u64(self.len() as u64);
        for item in self {
            item.write_canonical(sink);
        }
    }
}

impl<T: CanonicalDecode> CanonicalDecode for Vec<T> {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        let length = reader.read_u64()?;
        let length = usize::try_from(length).map_err(|_| CanonicalError::InvalidLength(length))?;
        let mut items = Vec::with_capacity(length.min(reader.remaining()));
        for _ in 0..length {
            items.push(T::read_canonical(reader)?);
        }
        Ok(items)
    }
}

/// Maps serialize in key order, so the encoding is independent of insertion
/// order.
impl<K: CanonicalSerialize + Ord, V: CanonicalSerialize> CanonicalSerialize for BTreeMap<K, V> {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u64(self.len() as u64);
        for (key, value) in self {
            key.write_canonical(sink);
            value.write_canonical(sink);
        }
    }
}

impl<K: CanonicalDecode + Ord, V: CanonicalDecode> CanonicalDecode for BTreeMap<K, V> {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        let length = reader.read_u64()?;
        let length = usize::try_from(length).map_err(|_| CanonicalError::InvalidLength(length))?;
        let mut map = BTreeMap::new();
        for _ in 0..length {
            let key = K::read_canonical(reader)?;
            let value = V::read_canonical(reader)?;
            map.insert(key, value);
        }
        Ok(map)
    }
}

impl<T: CanonicalSerialize> CanonicalSerialize for Option<T> {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        match self {
            None => sink.write_u32(0),
            Some(value) => {
                sink.write_u32(1);
                value.write_canonical(sink);
            }
        }
    }
}

impl<T: CanonicalDecode> CanonicalDecode for Option<T> {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        match reader.read_u32()? {
            0 => Ok(None),
            1 => Ok(Some(T::read_canonical(reader)?)),
            other => Err(CanonicalError::UnknownTag(other)),
        }
    }
}

macro_rules! impl_tuple {
    ($(($name:ident, $bind:ident)),+) => {
        impl<$($name: CanonicalSerialize),+> CanonicalSerialize for ($($name,)+) {
            fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
                let ($(ref $bind,)+) = *self;
                $($bind.write_canonical(sink);)+
            }
        }
        impl<$($name: CanonicalDecode),+> CanonicalDecode for ($($name,)+) {
            fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
                Ok(($($name::read_canonical(reader)?,)+))
            }
        }
    };
}

impl_tuple!((A, a));
impl_tuple!((A, a), (B, b));
impl_tuple!((A, a), (B, b), (C, c));
impl_tuple!((A, a), (B, b), (C, c), (D, d));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_digest_pins_the_encoding() {
        // Pins the exact field encoding: write_u64(1) then write_bytes(b"phalanx").
        // Any change to field order, lengths, tags, or endianness must bump
        // CANONICAL_ENCODING_VERSION and update this value.
        let mut sink = HashSink::new(DigestAlgorithm::Fnv1a64);
        sink.write_u64(1);
        sink.write_bytes(b"phalanx");
        let digest = sink.finish();
        assert_eq!(digest.algorithm(), DigestAlgorithm::Fnv1a64);
        assert_eq!(digest.value(), 0xea717ad78dea6263_u64.to_le_bytes());
        assert_eq!(CANONICAL_ENCODING_VERSION, 1);
    }

    #[test]
    fn hash_sink_and_snapshot_sink_see_the_same_stream() {
        let mut hash = HashSink::new(DigestAlgorithm::current());
        let mut snapshot = SnapshotSink::new();
        for value in 0..10_u64 {
            value.write_canonical(&mut hash);
            value.write_canonical(&mut snapshot);
        }
        "phalanx".write_canonical(&mut hash);
        "phalanx".write_canonical(&mut snapshot);

        // The snapshot bytes decode back to the same values.
        let bytes = snapshot.bytes;
        let mut reader = CanonicalReader::new(&bytes);
        for expected in 0..10_u64 {
            assert_eq!(u64::read_canonical(&mut reader).unwrap(), expected);
        }
        assert_eq!(String::read_canonical(&mut reader).unwrap(), "phalanx");
        reader.finish().unwrap();

        // The digest of the same traversal is stable across independent sinks.
        let mut again = HashSink::new(DigestAlgorithm::current());
        for value in 0..10_u64 {
            value.write_canonical(&mut again);
        }
        "phalanx".write_canonical(&mut again);
        assert_eq!(hash.finish(), again.finish());
    }

    #[test]
    fn encode_decode_round_trips_collections_and_tuples() {
        let mut map: BTreeMap<String, Vec<(u64, bool)>> = BTreeMap::new();
        map.insert("a".to_owned(), vec![(1, true), (2, false)]);
        map.insert("b".to_owned(), vec![]);
        let bytes = canonical_encode(&map);
        let decoded: BTreeMap<String, Vec<(u64, bool)>> = canonical_decode(&bytes).unwrap();
        assert_eq!(map, decoded);
    }

    #[test]
    fn decode_rejects_truncated_and_trailing_input() {
        let bytes = canonical_encode(&vec![1_u64, 2, 3]);
        assert_eq!(
            canonical_decode::<Vec<u64>>(&bytes[..bytes.len() - 1]),
            Err(CanonicalError::UnexpectedEof)
        );
        let mut extended = bytes.clone();
        extended.push(0);
        assert_eq!(
            canonical_decode::<Vec<u64>>(&extended),
            Err(CanonicalError::TrailingBytes(1))
        );
    }

    #[test]
    fn golden_digest_pins_each_algorithm_and_width() {
        // FNV-1a 32 of the same stream: pins the 4-byte width variant.
        let mut sink = HashSink::new(DigestAlgorithm::Fnv1a32);
        sink.write_u64(1);
        sink.write_bytes(b"phalanx");
        let digest = sink.finish();
        assert_eq!(digest.algorithm(), DigestAlgorithm::Fnv1a32);
        assert_eq!(digest.value(), 0x59064f23_u32.to_le_bytes());
        assert_eq!(DigestAlgorithm::Fnv1a32.width(), 4);
        assert_eq!(DigestAlgorithm::Fnv1a64.width(), 8);
    }

    #[test]
    fn digest_algorithm_mismatch_is_detected() {
        let digest = canonical_digest(&42_u64);
        assert_eq!(digest.algorithm(), DigestAlgorithm::current());
        assert!(digest.expect_algorithm(DigestAlgorithm::Fnv1a64).is_ok());
        // A digest produced by a different algorithm is not equal and errors.
        let mut other_sink = HashSink::new(DigestAlgorithm::Fnv1a32);
        42_u64.write_canonical(&mut other_sink);
        let other = other_sink.finish();
        assert_ne!(digest, other);
        assert_eq!(
            digest.expect_algorithm(DigestAlgorithm::Fnv1a32),
            Err(DigestMismatch {
                expected: DigestAlgorithm::Fnv1a32,
                found: DigestAlgorithm::Fnv1a64,
            })
        );
    }

    #[test]
    fn unknown_digest_algorithm_tag_is_rejected_on_decode() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&99_u32.to_le_bytes());
        bytes.extend_from_slice(&8_u64.to_le_bytes());
        bytes.extend_from_slice(&[0; 8]);
        assert_eq!(
            canonical_decode::<Digest>(&bytes),
            Err(CanonicalError::UnknownDigestAlgorithm(99))
        );
    }

    #[test]
    fn digest_round_trips_through_the_canonical_stream() {
        let digest = canonical_digest(&"phalanx".to_owned());
        let bytes = canonical_encode(&digest);
        let decoded: Digest = canonical_decode(&bytes).expect("digest decodes");
        assert_eq!(decoded, digest);
        assert_eq!(decoded.value().len(), digest.algorithm().width());
    }

    #[test]
    fn map_encoding_is_independent_of_insertion_order() {
        let mut first: BTreeMap<u64, u64> = BTreeMap::new();
        let mut second: BTreeMap<u64, u64> = BTreeMap::new();
        for i in 0..50_u64 {
            first.insert(i, i * 7);
        }
        for i in (0..50_u64).rev() {
            second.insert(i, i * 7);
        }
        assert_eq!(canonical_digest(&first), canonical_digest(&second));
    }
}
