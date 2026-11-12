//! Deterministic RNG streams (ADR-0001 Decision §4, `RtsEngineDoc.md` §3.2).
//!
//! All gameplay randomness uses explicitly seeded streams owned by the CPU
//! simulation; stream state is part of authoritative state and must be
//! recordable in saves and replays. The generator is SplitMix64: it uses only
//! wrapping integer operations, which are exactly defined by Rust on every
//! platform, so output is reproducible from a recorded seed or state.

use std::collections::BTreeMap;

use crate::hash::{CanonicalSerialize, CanonicalSink};

const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;
const MIX1: u64 = 0xBF58_476D_1CE4_E5B9;
const MIX2: u64 = 0x94D0_49BB_1331_11EB;

fn splitmix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(MIX1);
    z = (z ^ (z >> 27)).wrapping_mul(MIX2);
    z ^ (z >> 31)
}

/// A single deterministic RNG stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RngStream {
    state: u64,
}

impl RngStream {
    /// Creates a stream from an explicit seed.
    #[must_use]
    pub const fn from_seed(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Restores a stream from a recorded state (for example from a savegame
    /// or replay checkpoint).
    #[must_use]
    pub const fn from_state(state: u64) -> Self {
        Self { state }
    }

    /// Returns the current state, suitable for recording in saves/replays.
    #[must_use]
    pub const fn state(&self) -> u64 {
        self.state
    }

    /// Returns the next deterministic `u64`.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(GAMMA);
        splitmix64(self.state)
    }

    /// Returns the next deterministic value uniformly below `bound`, or `None`
    /// if `bound` is zero. Uses rejection sampling so every value below
    /// `bound` stays equally likely without floating point.
    pub fn next_u64_below(&mut self, bound: u64) -> Option<u64> {
        if bound == 0 {
            return None;
        }
        let reject = (u64::MAX % bound + 1) % bound;
        let limit = u64::MAX - reject;
        loop {
            let value = self.next_u64();
            if value <= limit {
                return Some(value % bound);
            }
        }
    }

    /// Returns the next deterministic boolean.
    pub fn next_bool(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }
}

impl CanonicalSerialize for RngStream {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u64(self.state);
    }
}

/// Errors produced when registering or restoring RNG streams.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RngError {
    /// The snapshot names a stream that is not registered.
    UnknownStream { name: &'static str },
    /// A stream with this name is already registered; re-registering must
    /// not silently overwrite it.
    DuplicateStream { name: &'static str },
}

impl std::fmt::Display for RngError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownStream { name } => write!(formatter, "unknown RNG stream: {name}"),
            Self::DuplicateStream { name } => {
                write!(formatter, "RNG stream already registered: {name}")
            }
        }
    }
}

impl std::error::Error for RngError {}

/// Deterministically derives a sub-seed from a master seed and a stream name
/// (FNV-1a over the name bytes, finished with the SplitMix64 mixer).
fn derive_seed(master_seed: u64, name: &str) -> u64 {
    let mut hash = 0xCBF2_9CE4_8422_2325_u64 ^ master_seed;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    splitmix64(hash)
}

/// Owns the named RNG streams of one simulation.
///
/// Streams have separate ownership (for example per system: `"combat"`,
/// `"ai"`), so one system's draws never perturb another's. Streams can be
/// registered with explicit seeds or derived deterministically from the
/// master seed and their name.
#[derive(Clone, Debug)]
pub struct RngStreams {
    master_seed: u64,
    streams: BTreeMap<&'static str, RngStream>,
}

impl RngStreams {
    /// Creates an empty registry from the simulation's master seed.
    #[must_use]
    pub const fn new(master_seed: u64) -> Self {
        Self {
            master_seed,
            streams: BTreeMap::new(),
        }
    }

    /// Returns the master seed.
    #[must_use]
    pub const fn master_seed(&self) -> u64 {
        self.master_seed
    }

    /// Registers a stream with an explicit seed. Registering the same name
    /// twice is an error rather than a silent overwrite.
    pub fn register(&mut self, name: &'static str, seed: u64) -> Result<&mut RngStream, RngError> {
        if self.streams.contains_key(name) {
            return Err(RngError::DuplicateStream { name });
        }
        Ok(self
            .streams
            .entry(name)
            .or_insert_with(|| RngStream::from_seed(seed)))
    }

    /// Returns the stream registered under `name`, deriving it
    /// deterministically from the master seed if it does not exist yet.
    pub fn stream(&mut self, name: &'static str) -> &mut RngStream {
        self.streams
            .entry(name)
            .or_insert_with(|| RngStream::from_seed(derive_seed(self.master_seed, name)))
    }

    /// Returns a shared reference to the stream registered under `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&RngStream> {
        self.streams.get(name)
    }

    /// Returns whether a stream is registered under `name`.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.streams.contains_key(name)
    }

    /// Returns the number of registered streams.
    #[must_use]
    pub fn len(&self) -> usize {
        self.streams.len()
    }

    /// Returns whether no streams are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.streams.is_empty()
    }

    /// Records every stream's state. Iteration order is by name, so the
    /// snapshot is deterministic.
    #[must_use]
    pub fn snapshot(&self) -> BTreeMap<&'static str, u64> {
        self.streams
            .iter()
            .map(|(name, stream)| (*name, stream.state()))
            .collect()
    }

    /// Restores stream states from a snapshot. Every snapshot entry must name
    /// a registered stream; unknown names are an explicit error.
    pub fn restore(&mut self, snapshot: &BTreeMap<&'static str, u64>) -> Result<(), RngError> {
        for (name, state) in snapshot {
            let stream = self
                .streams
                .get_mut(name)
                .ok_or(RngError::UnknownStream { name })?;
            *stream = RngStream::from_state(*state);
        }
        Ok(())
    }
}

/// Canonical form: master seed, then streams in name order (the registry is a
/// `BTreeMap`), each with its recorded state.
impl CanonicalSerialize for RngStreams {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u64(self.master_seed);
        sink.write_u64(self.streams.len() as u64);
        for (name, stream) in &self.streams {
            sink.write_bytes(name.as_bytes());
            stream.write_canonical(sink);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_reproduces_the_same_sequence() {
        let mut first = RngStream::from_seed(42);
        let mut second = RngStream::from_seed(42);
        for _ in 0..1000 {
            assert_eq!(first.next_u64(), second.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut first = RngStream::from_seed(1);
        let mut second = RngStream::from_seed(2);
        let same = (0..100)
            .filter(|_| first.next_u64() == second.next_u64())
            .count();
        assert!(same < 100);
    }

    #[test]
    fn recorded_state_reproduces_the_sequence() {
        let mut original = RngStream::from_seed(7);
        for _ in 0..10 {
            original.next_u64();
        }
        let state = original.state();

        let mut restored = RngStream::from_state(state);
        let mut replayed = RngStream::from_seed(7);
        for _ in 0..10 {
            replayed.next_u64();
        }
        for _ in 0..100 {
            let expected = original.next_u64();
            assert_eq!(expected, restored.next_u64());
            assert_eq!(expected, replayed.next_u64());
        }
    }

    #[test]
    fn bounded_draws_stay_in_range_and_are_deterministic() {
        for bound in [1_u64, 2, 3, 7, 1 << 63, u64::MAX] {
            let mut first = RngStream::from_seed(99);
            let mut second = RngStream::from_seed(99);
            for _ in 0..1000 {
                let a = first.next_u64_below(bound).expect("bound > 0");
                let b = second.next_u64_below(bound).expect("bound > 0");
                assert_eq!(a, b);
                assert!(a < bound);
            }
        }
        let mut stream = RngStream::from_seed(1);
        assert_eq!(stream.next_u64_below(0), None);
    }

    #[test]
    fn bool_draws_are_deterministic() {
        let mut first = RngStream::from_seed(5);
        let mut second = RngStream::from_seed(5);
        for _ in 0..100 {
            assert_eq!(first.next_bool(), second.next_bool());
        }
    }

    #[test]
    fn derived_streams_are_deterministic_and_independent() {
        let mut first = RngStreams::new(1234);
        let mut second = RngStreams::new(1234);

        let a1 = first.stream("combat").next_u64();
        let a2 = second.stream("combat").next_u64();
        assert_eq!(a1, a2);

        // Drawing from one stream must not perturb another.
        let before = first.stream("ai").next_u64();
        let _ = first.stream("combat").next_u64();
        let after = first.stream("ai").next_u64();
        let mut expected_ai = RngStreams::new(1234);
        let expected_before = expected_ai.stream("ai").next_u64();
        let expected_after = expected_ai.stream("ai").next_u64();
        assert_eq!(before, expected_before);
        assert_eq!(after, expected_after);
        assert_ne!(before, after);
    }

    #[test]
    fn explicit_registration_is_reproducible_and_snapshot_round_trips() {
        let mut streams = RngStreams::new(77);
        streams.register("loot", 1001).expect("register");
        streams.stream("combat").next_u64();
        streams.stream("combat").next_u64();
        streams.stream("loot").next_u64();

        let snapshot = streams.snapshot();
        assert_eq!(snapshot.len(), 2);

        let mut restored = RngStreams::new(77);
        restored.register("loot", 1001).expect("register");
        restored.stream("combat");
        restored.restore(&snapshot).expect("restore");

        assert_eq!(
            streams.stream("combat").next_u64(),
            restored.stream("combat").next_u64()
        );
        assert_eq!(
            streams.stream("loot").next_u64(),
            restored.stream("loot").next_u64()
        );
    }

    #[test]
    fn restore_rejects_unknown_streams() {
        let mut streams = RngStreams::new(1);
        let mut snapshot = BTreeMap::new();
        snapshot.insert("missing", 0_u64);
        assert_eq!(
            streams.restore(&snapshot),
            Err(RngError::UnknownStream { name: "missing" })
        );
    }

    #[test]
    fn duplicate_registration_is_an_error() {
        let mut streams = RngStreams::new(1);
        streams.register("combat", 5).expect("first register");
        // Registering the same name again must not silently overwrite.
        assert!(streams.register("combat", 6).is_err());
        assert_eq!(streams.get("combat").map(RngStream::state), Some(5));
    }
}
