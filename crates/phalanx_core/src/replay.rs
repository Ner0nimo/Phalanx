//! Deterministic command replay harness (ADR-0001 Decision §8,
//! `RtsEngineDoc.md` §13.2–13.3).
//!
//! A [`ReplayRecording`] captures the replay metadata (format and simulation
//! schema versions, digest algorithm, tick rate, master RNG seed), the
//! tick-indexed command stream, and the per-tick canonical state digests.
//! [`replay_recording`] replays a recording into a fresh simulation instance
//! and reports the first tick whose state digest diverges, with
//! subsystem-agnostic tick granularity. Recordings made with a different
//! digest algorithm or an unsupported format version are rejected explicitly.
//!
//! A same-build replay on the same supported platform reproduces identical
//! per-tick digests. Cross-platform bit-identical lockstep is a separate
//! acceptance target and is not claimed here (ADR-0001 Decision §5).

use crate::command::{
    CommandEnvelope, CommandId, CommandRequest, CommandSubmitError, ENVELOPE_VERSION,
};
use crate::hash::{CanonicalSerialize, Digest, DigestAlgorithm};
use crate::simulation::{SIMULATION_SCHEMA_VERSION, SimulationRunner, SystemManifest, TickRate};

/// The replay container format version. Recorded in every replay; bump on
/// incompatible format changes.
pub const REPLAY_FORMAT_VERSION: u32 = 1;

/// An immutable record of one deterministic simulation run.
#[derive(Clone, Debug)]
pub struct ReplayRecording<P> {
    format_version: u32,
    simulation_version: u32,
    digest_algorithm: DigestAlgorithm,
    system_manifest: SystemManifest,
    tick_rate: TickRate,
    rng_seed: u64,
    commands: Vec<CommandEnvelope<P>>,
    tick_hashes: Vec<Digest>,
}

impl<P> ReplayRecording<P> {
    /// Creates an empty recording with the current format and simulation
    /// schema versions, the current digest algorithm, and an empty system
    /// manifest.
    #[must_use]
    pub fn new(tick_rate: TickRate, rng_seed: u64) -> Self {
        Self {
            format_version: REPLAY_FORMAT_VERSION,
            simulation_version: SIMULATION_SCHEMA_VERSION,
            digest_algorithm: DigestAlgorithm::current(),
            system_manifest: SystemManifest::default(),
            tick_rate,
            rng_seed,
            commands: Vec::new(),
            tick_hashes: Vec::new(),
        }
    }

    /// Returns the replay format version.
    #[must_use]
    pub const fn format_version(&self) -> u32 {
        self.format_version
    }

    /// Returns the simulation schema version the recording was made with.
    #[must_use]
    pub const fn simulation_version(&self) -> u32 {
        self.simulation_version
    }

    /// Returns the digest algorithm the per-tick digests were recorded with.
    /// Replays reject recordings made with a different algorithm.
    #[must_use]
    pub const fn digest_algorithm(&self) -> DigestAlgorithm {
        self.digest_algorithm
    }

    /// Returns the finalized system manifest the run was recorded with.
    /// Replays reject recordings whose required system manifest is
    /// incompatible with the replay runner's.
    #[must_use]
    pub const fn system_manifest(&self) -> &SystemManifest {
        &self.system_manifest
    }

    /// Returns the tick rate the recording was made with.
    #[must_use]
    pub const fn tick_rate(&self) -> TickRate {
        self.tick_rate
    }

    /// Returns the master RNG seed the recording was made with.
    #[must_use]
    pub const fn rng_seed(&self) -> u64 {
        self.rng_seed
    }

    /// Returns the recorded command stream in submission (command-ID) order.
    #[must_use]
    pub fn commands(&self) -> &[CommandEnvelope<P>] {
        &self.commands
    }

    /// Returns mutable access to the command stream, for harness tests that
    /// deliberately alter an input.
    pub fn commands_mut(&mut self) -> &mut Vec<CommandEnvelope<P>> {
        &mut self.commands
    }

    /// Returns the recorded per-tick canonical state digests, in tick order.
    #[must_use]
    pub fn tick_hashes(&self) -> &[Digest] {
        &self.tick_hashes
    }

    /// Appends a command to the recording.
    pub fn push_command(&mut self, envelope: CommandEnvelope<P>) {
        self.commands.push(envelope);
    }

    /// Appends a per-tick state digest to the recording.
    pub fn push_tick_hash(&mut self, digest: Digest) {
        self.tick_hashes.push(digest);
    }
}

/// Records a deterministic run: wraps a [`SimulationRunner`], capturing every
/// accepted command envelope and every per-tick state hash.
pub struct ReplayRecorder<P> {
    runner: SimulationRunner<P>,
    recording: ReplayRecording<P>,
}

impl<P: CanonicalSerialize + Clone> ReplayRecorder<P> {
    /// Creates a recorder with the given tick rate and explicit master RNG
    /// seed.
    #[must_use]
    pub fn new(tick_rate: TickRate, rng_seed: u64) -> Self {
        Self {
            runner: SimulationRunner::new(tick_rate, rng_seed),
            recording: ReplayRecording::new(tick_rate, rng_seed),
        }
    }

    /// Returns the wrapped runner.
    #[must_use]
    pub const fn runner(&self) -> &SimulationRunner<P> {
        &self.runner
    }

    /// Returns mutable access to the wrapped runner, for registering systems.
    #[must_use]
    pub const fn runner_mut(&mut self) -> &mut SimulationRunner<P> {
        &mut self.runner
    }

    /// Submits a command to the run and records its envelope.
    pub fn submit_command(
        &mut self,
        request: CommandRequest<P>,
    ) -> Result<CommandId, CommandSubmitError> {
        let command_id = self.runner.submit_command(request.clone())?;
        let envelope = CommandEnvelope::new(
            ENVELOPE_VERSION,
            request.request_id,
            command_id,
            request.actor,
            request.tick,
            request.subject,
            request.payload,
        )
        .expect("ENVELOPE_VERSION is always supported");
        self.recording.push_command(envelope);
        Ok(command_id)
    }

    /// Advances the recorded run by exactly one tick and records its state
    /// digest.
    ///
    /// Fails with [`ReplayError::HashRecordingDisabled`] if per-tick digest
    /// recording was disabled on the runner; a stale previous digest is never
    /// recorded silently.
    pub fn step_tick(&mut self) -> Result<(), ReplayError> {
        let recorded_before = self.runner.tick_hashes().len();
        self.runner
            .step_tick()
            .map_err(|_| ReplayError::TickOverflow)?;
        let digest = self
            .runner
            .tick_hashes()
            .get(recorded_before)
            .cloned()
            .ok_or(ReplayError::HashRecordingDisabled)?;
        self.recording.push_tick_hash(digest);
        Ok(())
    }

    /// Advances the recorded run by exactly `count` ticks.
    pub fn advance_ticks(&mut self, count: u64) -> Result<(), ReplayError> {
        for _ in 0..count {
            self.step_tick()?;
        }
        Ok(())
    }

    /// Finishes the recording. The runner's registry is finalized (if it was
    /// not already) and the resulting system manifest is recorded as replay
    /// compatibility metadata.
    #[must_use]
    pub fn finish(mut self) -> ReplayRecording<P> {
        let manifest = self.runner.finalize_registry().clone();
        self.recording.system_manifest = manifest;
        self.recording
    }
}

/// The result of replaying a recording into a fresh simulation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayReport {
    ticks_executed: u64,
    tick_hashes: Vec<Digest>,
    first_divergent_tick: Option<u64>,
}

impl ReplayReport {
    /// Returns how many ticks the replay executed.
    #[must_use]
    pub const fn ticks_executed(&self) -> u64 {
        self.ticks_executed
    }

    /// Returns the per-tick canonical state digests produced by the replay.
    #[must_use]
    pub fn tick_hashes(&self) -> &[Digest] {
        &self.tick_hashes
    }

    /// Returns the first tick whose state hash differs from the recording, or
    /// `None` if the replay reproduced the recording exactly. If the replay
    /// and the recording cover different numbers of ticks, the first tick
    /// not covered by both is reported as divergent.
    #[must_use]
    pub const fn first_divergent_tick(&self) -> Option<u64> {
        self.first_divergent_tick
    }

    /// Returns whether the replay diverged from the recording.
    #[must_use]
    pub const fn diverged(&self) -> bool {
        self.first_divergent_tick.is_some()
    }
}

/// Errors produced while replaying a recording.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplayError {
    /// The recording's format version is not supported by this harness.
    UnsupportedFormatVersion { found: u32, expected: u32 },
    /// The recording's digest algorithm is not the one this build produces;
    /// digests of a different algorithm or width cannot be compared.
    IncompatibleDigestAlgorithm {
        expected: DigestAlgorithm,
        found: DigestAlgorithm,
    },
    /// The recording's system manifest does not match the replay runner's
    /// finalized registry; a different system set can change behavior.
    IncompatibleSystemManifest { expected: Digest, found: Digest },
    /// A recorded command was rejected during replay.
    InvalidRecordedCommand(CommandSubmitError),
    /// The replay ran out of representable ticks.
    TickOverflow,
    /// Per-tick digest recording is disabled, so no fresh state digest exists
    /// for the tick that was just executed.
    HashRecordingDisabled,
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedFormatVersion { found, expected } => write!(
                formatter,
                "unsupported replay format version {found} (expected {expected})"
            ),
            Self::IncompatibleDigestAlgorithm { expected, found } => write!(
                formatter,
                "incompatible digest algorithm: this build produces {expected:?}, recording uses {found:?}"
            ),
            Self::IncompatibleSystemManifest { expected, found } => write!(
                formatter,
                "incompatible system manifest: recording requires {expected}, replay runner has {found}"
            ),
            Self::InvalidRecordedCommand(error) => {
                write!(formatter, "recorded command rejected: {error}")
            }
            Self::TickOverflow => formatter.write_str("replay tick counter overflowed"),
            Self::HashRecordingDisabled => {
                formatter.write_str("per-tick state hash recording is disabled")
            }
        }
    }
}

impl std::error::Error for ReplayError {}

/// Replays a recording into a fresh simulation instance for exactly `ticks`
/// ticks, registering systems through `setup`, and compares the per-tick
/// canonical state digests against the recorded ones.
///
/// All recorded commands are submitted up front in recorded order, mirroring
/// how the recording was made, so command IDs and bookkeeping match exactly.
#[allow(clippy::missing_panics_doc)]
pub fn replay_recording<P: CanonicalSerialize + Clone>(
    recording: &ReplayRecording<P>,
    ticks: u64,
    setup: impl FnOnce(&mut SimulationRunner<P>),
) -> Result<ReplayReport, ReplayError> {
    if recording.format_version() != REPLAY_FORMAT_VERSION {
        return Err(ReplayError::UnsupportedFormatVersion {
            found: recording.format_version(),
            expected: REPLAY_FORMAT_VERSION,
        });
    }
    let current_algorithm = DigestAlgorithm::current();
    if recording.digest_algorithm() != current_algorithm {
        return Err(ReplayError::IncompatibleDigestAlgorithm {
            expected: current_algorithm,
            found: recording.digest_algorithm(),
        });
    }
    for digest in recording.tick_hashes() {
        digest
            .expect_algorithm(current_algorithm)
            .map_err(|mismatch| ReplayError::IncompatibleDigestAlgorithm {
                expected: mismatch.expected,
                found: mismatch.found,
            })?;
    }

    let mut runner = SimulationRunner::new(recording.tick_rate(), recording.rng_seed());
    setup(&mut runner);
    // The finalized system manifest is replay compatibility metadata: a
    // recording made with a different system set is rejected explicitly.
    let manifest = runner.finalize_registry().clone();
    if manifest.digest() != recording.system_manifest().digest() {
        return Err(ReplayError::IncompatibleSystemManifest {
            expected: recording.system_manifest().digest(),
            found: manifest.digest(),
        });
    }
    for envelope in recording.commands() {
        let request = CommandRequest {
            request_id: envelope.request_id(),
            actor: envelope.actor(),
            tick: envelope.tick(),
            subject: envelope.subject(),
            payload: envelope.payload().clone(),
        };
        runner
            .submit_command(request)
            .map_err(ReplayError::InvalidRecordedCommand)?;
    }

    let mut tick_hashes = Vec::new();
    for _ in 0..ticks {
        let recorded_before = runner.tick_hashes().len();
        runner.step_tick().map_err(|_| ReplayError::TickOverflow)?;
        let digest = runner
            .tick_hashes()
            .get(recorded_before)
            .cloned()
            .ok_or(ReplayError::HashRecordingDisabled)?;
        tick_hashes.push(digest);
    }

    let recorded = recording.tick_hashes();
    let mut first_divergent_tick = tick_hashes
        .iter()
        .zip(recorded)
        .enumerate()
        .find_map(|(index, (actual, expected))| (actual != expected).then_some(index as u64 + 1));
    // A length mismatch means the replay did not verify every recorded tick;
    // report the first tick not covered by both as divergent.
    if first_divergent_tick.is_none() && !recorded.is_empty() && tick_hashes.len() != recorded.len()
    {
        first_divergent_tick = Some(tick_hashes.len().min(recorded.len()) as u64 + 1);
    }

    Ok(ReplayReport {
        ticks_executed: ticks,
        tick_hashes,
        first_divergent_tick,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{ActorId, CommandPayload, RequestId};
    use crate::scenario::{SCENARIO_TICKS, record_scenario, setup_scenario};
    use crate::simulation::TickRate;

    #[test]
    fn same_build_replay_reproduces_per_tick_hashes() {
        let recording = record_scenario(SCENARIO_TICKS);
        let report = replay_recording(&recording, SCENARIO_TICKS, setup_scenario)
            .expect("replay should succeed");

        assert_eq!(report.ticks_executed(), SCENARIO_TICKS);
        assert!(!report.diverged());
        assert_eq!(report.first_divergent_tick(), None);
        assert_eq!(report.tick_hashes(), recording.tick_hashes());

        // Replaying twice gives identical results.
        let again = replay_recording(&recording, SCENARIO_TICKS, setup_scenario)
            .expect("second replay should succeed");
        assert_eq!(report, again);
    }

    #[test]
    fn replay_metadata_includes_format_and_simulation_versions() {
        let recording = record_scenario(4);
        assert_eq!(recording.format_version(), REPLAY_FORMAT_VERSION);
        assert_eq!(recording.simulation_version(), SIMULATION_SCHEMA_VERSION);
        assert_eq!(recording.tick_rate(), TickRate::DEFAULT);
        assert_eq!(recording.tick_hashes().len(), 4);
        assert!(!recording.commands().is_empty());
    }

    #[test]
    fn unsupported_format_version_is_rejected() {
        let mut recording = record_scenario(4);
        recording.format_version = REPLAY_FORMAT_VERSION + 1;
        assert_eq!(
            replay_recording(&recording, 4, setup_scenario),
            Err(ReplayError::UnsupportedFormatVersion {
                found: REPLAY_FORMAT_VERSION + 1,
                expected: REPLAY_FORMAT_VERSION
            })
        );
    }

    #[test]
    fn altered_command_identifies_the_first_divergent_tick() {
        let mut recording = record_scenario(SCENARIO_TICKS);
        // Alter the payload of the command targeting tick 10 (index 4: ticks
        // 2, 4, 6, 8, 10, ...).
        let envelope = &recording.commands()[4];
        assert_eq!(envelope.tick(), 10);
        recording.commands_mut()[4] = CommandEnvelope::new(
            ENVELOPE_VERSION,
            envelope.request_id(),
            envelope.command_id(),
            envelope.actor(),
            envelope.tick(),
            envelope.subject(),
            CommandPayload::Diagnostics { value: 999_999 },
        )
        .expect("current version");

        // The pending command queue is authoritative input state and is part
        // of the canonical digest from submission, so the altered command
        // diverges the state already at tick 1.
        let report = replay_recording(&recording, SCENARIO_TICKS, setup_scenario)
            .expect("replay should succeed");
        assert!(report.diverged());
        assert_eq!(report.first_divergent_tick(), Some(1));
    }

    #[test]
    fn incompatible_digest_algorithm_is_rejected() {
        let mut recording = record_scenario(4);
        assert_eq!(
            recording.digest_algorithm(),
            crate::hash::DigestAlgorithm::current()
        );
        // A recording made with a different digest algorithm/width cannot be
        // compared against this build's digests and is rejected explicitly.
        recording.digest_algorithm = crate::hash::DigestAlgorithm::Fnv1a32;
        assert_eq!(
            replay_recording(&recording, 4, setup_scenario),
            Err(ReplayError::IncompatibleDigestAlgorithm {
                expected: crate::hash::DigestAlgorithm::Fnv1a64,
                found: crate::hash::DigestAlgorithm::Fnv1a32,
            })
        );
    }

    #[test]
    fn altered_rng_seed_diverges_at_the_first_rng_tick() {
        let mut recording = record_scenario(SCENARIO_TICKS);
        recording.rng_seed = recording.rng_seed().wrapping_add(1);
        let report = replay_recording(&recording, SCENARIO_TICKS, setup_scenario)
            .expect("replay should succeed");
        assert_eq!(report.first_divergent_tick(), Some(1));
    }

    #[test]
    fn recorded_commands_are_valid_replay_inputs() {
        let recording = record_scenario(SCENARIO_TICKS);
        for (index, envelope) in recording.commands().iter().enumerate() {
            assert_eq!(envelope.command_id(), CommandId(index as u64));
            assert_eq!(envelope.request_id(), RequestId(index as u64 + 1));
            assert_eq!(envelope.actor(), ActorId(0));
            assert!(matches!(
                envelope.payload(),
                CommandPayload::Diagnostics { .. }
            ));
        }
    }

    #[test]
    fn disabled_hash_recording_is_an_explicit_error_not_a_stale_hash() {
        // Recorder side: disabling recording must fail loudly.
        let mut recorder = ReplayRecorder::<CommandPayload>::new(TickRate::DEFAULT, 1);
        recorder.runner_mut().set_record_hashes(false);
        assert_eq!(
            recorder.advance_ticks(1),
            Err(ReplayError::HashRecordingDisabled)
        );
        assert!(recorder.finish().tick_hashes().is_empty());

        // Replay side: same guard.
        let recording = record_scenario(4);
        let result = replay_recording(&recording, 4, |runner| {
            setup_scenario(runner);
            runner.set_record_hashes(false);
        });
        assert_eq!(result, Err(ReplayError::HashRecordingDisabled));
    }

    #[test]
    fn replay_rejects_incompatible_system_manifest() {
        let recording = record_scenario(4);
        assert!(!recording.system_manifest().is_empty());
        // Replaying with a different (empty) system set must be rejected
        // explicitly, not silently replayed against different behavior.
        let result = replay_recording(&recording, 4, |_| {});
        assert!(matches!(
            result,
            Err(ReplayError::IncompatibleSystemManifest { .. })
        ));
        // The matching setup is accepted (covered by the other tests).
        let report = replay_recording(&recording, 4, setup_scenario).expect("matching manifest");
        assert!(!report.diverged());
    }

    #[test]
    fn replay_length_mismatch_reports_the_first_unverified_tick() {
        let recording = record_scenario(SCENARIO_TICKS);
        // Replay only half the recorded ticks: the common prefix matches, but
        // ticks 9..=16 were never verified.
        let report = replay_recording(&recording, SCENARIO_TICKS / 2, setup_scenario)
            .expect("replay should succeed");
        assert_eq!(report.ticks_executed(), SCENARIO_TICKS / 2);
        assert_eq!(report.first_divergent_tick(), Some(SCENARIO_TICKS / 2 + 1));
    }
}
