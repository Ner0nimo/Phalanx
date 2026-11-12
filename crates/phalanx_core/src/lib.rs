//! Minimal CPU-only primitives for the Phalanx simulation.
//!
//! The [`simulation`] module provides the fixed-tick [`simulation::SimulationRunner`]
//! and the explicit [`simulation::Phase`] schedule required by ADR-0001 and
//! `RtsEngineDoc.md` §6.1. The [`entity`], [`rng`], and [`command`] modules
//! provide the stable entity IDs, deterministic RNG streams, and versioned
//! tick-indexed command envelope that authoritative state is built on. The
//! [`hash`] module provides the versioned canonical encoding contract and the
//! replaceable state-digest interface, and [`replay`] provides the
//! deterministic replay harness, with a small CI scenario in [`scenario`].
//! The smoke scenario below is infrastructure validation only; it is
//! deliberately not a gameplay simulation.

#![forbid(unsafe_code)]

pub mod command;
pub mod entity;
pub mod hash;
pub mod replay;
pub mod rng;
pub mod scenario;
pub mod simulation;

/// A deterministic workload used to prove that the core can advance without
/// initializing a renderer, window, graphics API, or GPU.
///
/// This is a bootstrap diagnostic, not gameplay state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HeadlessSmokeScenario {
    tick: u64,
    digest: u64,
}

impl HeadlessSmokeScenario {
    const INITIAL_DIGEST: u64 = 0x6a09_e667_f3bc_c909;
    const TICK_MIX: u64 = 0x9e37_79b9_7f4a_7c15;

    /// Creates a smoke scenario with a deterministic initial digest.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            tick: 0,
            digest: Self::INITIAL_DIGEST,
        }
    }

    /// Returns the number of ticks advanced so far.
    #[must_use]
    pub const fn tick(&self) -> u64 {
        self.tick
    }

    /// Returns a deterministic digest of the executed tick sequence.
    #[must_use]
    pub const fn digest(&self) -> u64 {
        self.digest
    }

    /// Advances the diagnostic scenario by one deterministic tick.
    ///
    /// Returns an error rather than wrapping the tick counter.
    pub fn advance_tick(&mut self) -> Result<(), TickOverflow> {
        let next_tick = self.tick.checked_add(1).ok_or(TickOverflow)?;
        self.digest = self.digest.rotate_left(7) ^ next_tick.wrapping_mul(Self::TICK_MIX);
        self.tick = next_tick;
        Ok(())
    }
}

impl Default for HeadlessSmokeScenario {
    fn default() -> Self {
        Self::new()
    }
}

/// The authoritative tick counter cannot represent another tick.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TickOverflow;

impl std::fmt::Display for TickOverflow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("simulation tick counter overflowed")
    }
}

impl std::error::Error for TickOverflow {}

#[cfg(test)]
mod tests {
    use super::HeadlessSmokeScenario;

    #[test]
    fn identical_tick_sequences_produce_identical_digests() {
        let mut first = HeadlessSmokeScenario::new();
        let mut second = HeadlessSmokeScenario::new();

        for _ in 0..32 {
            first
                .advance_tick()
                .expect("tick counter should not overflow");
            second
                .advance_tick()
                .expect("tick counter should not overflow");
        }

        assert_eq!(first, second);
        assert_eq!(first.tick(), 32);
    }

    #[test]
    fn zero_ticks_preserve_the_initial_state() {
        let scenario = HeadlessSmokeScenario::new();

        assert_eq!(scenario.tick(), 0);
        assert_eq!(scenario.digest(), HeadlessSmokeScenario::new().digest());
    }
}
