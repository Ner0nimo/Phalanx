//! Deterministic command replay harness (ADR-0001 Decision §8,
//! `RtsEngineDoc.md` §13.2–13.3).
//!
//! A [`ReplayRecording`] captures the replay metadata (format and simulation
//! schema versions, digest algorithm, tick rate, master RNG seed), the
//! tick-indexed command stream, and the per-tick canonical state digests.
//! [`replay_recording`] replays a recording into a fresh simulation instance
//! and reports the first tick whose state digest diverges, with
//! subsystem-agnostic tick granularity. Recordings made with a different
//! digest algorithm, an unsupported replay format version, or an unsupported
//! simulation schema version are rejected explicitly at the boundary, before
//! the replay runner is constructed.
//!
//! A same-build replay on the same supported platform reproduces identical
//! per-tick digests. Cross-platform bit-identical lockstep is a separate
//! acceptance target and is not claimed here (ADR-0001 Decision §5).

use std::collections::{BTreeMap, BTreeSet};

use crate::command::{
    ActorId, CommandEnvelope, CommandId, CommandLimits, CommandLimitsError, CommandRequest,
    CommandSubmitError, ENVELOPE_VERSION, RequestId, SessionId,
};
use crate::hash::{CanonicalDecode, CanonicalSerialize, Digest, DigestAlgorithm};
use crate::rng::SEED_DERIVATION_VERSION;
use crate::simulation::{
    SIMULATION_CONFIG_VERSION, SIMULATION_SCHEMA_VERSION, SimulationRunner, StepError,
    SystemManifest, TickRate,
};

/// The replay container format version. Recorded in every replay; bump on
/// incompatible format changes. Version 2 added the RNG seed-derivation
/// version (issue #47); version 3 added the explicit end tick, the recorded
/// command limits, and named content digests (issue #52); version 4 added the
/// simulation configuration version (issue #37).
pub const REPLAY_FORMAT_VERSION: u32 = 4;

/// An immutable record of one deterministic simulation run.
#[derive(Clone, Debug)]
pub struct ReplayRecording<P> {
    format_version: u32,
    simulation_version: u32,
    config_version: u32,
    rng_derivation_version: u32,
    digest_algorithm: DigestAlgorithm,
    system_manifest: SystemManifest,
    tick_rate: TickRate,
    rng_seed: u64,
    command_limits: CommandLimits,
    content_digests: BTreeMap<String, Digest>,
    commands: Vec<CommandEnvelope<P>>,
    tick_hashes: Vec<Digest>,
    end_tick: u64,
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
            config_version: SIMULATION_CONFIG_VERSION,
            rng_derivation_version: SEED_DERIVATION_VERSION,
            digest_algorithm: DigestAlgorithm::current(),
            system_manifest: SystemManifest::default(),
            tick_rate,
            rng_seed,
            command_limits: CommandLimits::DEFAULT,
            content_digests: BTreeMap::new(),
            commands: Vec::new(),
            tick_hashes: Vec::new(),
            end_tick: 0,
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

    /// Returns the simulation configuration version the recording was made
    /// with (issue #37). Replays reject recordings whose gameplay-affecting
    /// configuration layout differs.
    #[must_use]
    pub const fn config_version(&self) -> u32 {
        self.config_version
    }

    /// Returns the RNG seed-derivation algorithm version the recording was
    /// made with (issue #47). Replays reject recordings whose derived stream
    /// seeds could differ.
    #[must_use]
    pub const fn rng_derivation_version(&self) -> u32 {
        self.rng_derivation_version
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

    /// Returns the command ingress limits the recording was made with
    /// (issue #52); replay applies them to the fresh runner.
    #[must_use]
    pub const fn command_limits(&self) -> &CommandLimits {
        &self.command_limits
    }

    /// Returns the named content digests the recording requires (issue #52):
    /// canonical digests of scenario/map/gameplay content, keyed by name. They
    /// are content identities, not filesystem paths, so they do not depend on
    /// machine-local layout.
    #[must_use]
    pub fn content_digests(&self) -> &BTreeMap<String, Digest> {
        &self.content_digests
    }

    /// Returns the explicit final recorded tick: the number of executed ticks
    /// (= checkpoint hash count) when the recording was finished (issue #52).
    #[must_use]
    pub const fn end_tick(&self) -> u64 {
        self.end_tick
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

    /// Appends a per-tick state digest to the recording. Each executed tick
    /// advances the explicit end tick.
    pub fn push_tick_hash(&mut self, digest: Digest) {
        self.tick_hashes.push(digest);
        self.end_tick = self.tick_hashes.len() as u64;
    }

    /// Records a required content identity (issue #52): a canonical digest of
    /// named scenario/map/gameplay content that replay must verify.
    pub fn push_content_digest(&mut self, name: &str, digest: Digest) {
        self.content_digests.insert(name.to_owned(), digest);
    }
}

/// Records a deterministic run: wraps a [`SimulationRunner`], capturing every
/// accepted command envelope and every per-tick state hash.
pub struct ReplayRecorder<P, E = ()> {
    runner: SimulationRunner<P, E>,
    recording: ReplayRecording<P>,
}

impl<P: CanonicalSerialize + Clone, E: CanonicalSerialize> ReplayRecorder<P, E> {
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
    pub const fn runner(&self) -> &SimulationRunner<P, E> {
        &self.runner
    }

    /// Returns mutable access to the wrapped runner, for registering systems.
    #[must_use]
    pub const fn runner_mut(&mut self) -> &mut SimulationRunner<P, E> {
        &mut self.runner
    }

    /// Records a required content identity on the recording (issue #52).
    pub fn push_content_digest(&mut self, name: &str, digest: Digest) {
        self.recording.push_content_digest(name, digest);
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
            request.session,
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
        // Stepping an already-terminated run is an explicit error, not a
        // silent no-op (issue #37).
        if let Some(termination) = self.runner.simulation().terminated() {
            return Err(ReplayError::RunTerminated {
                tick: termination.tick(),
            });
        }
        let recorded_before = self.runner.tick_hashes().len();
        match self.runner.step_tick() {
            Ok(()) => {}
            Err(StepError::TickOverflow(_)) => return Err(ReplayError::TickOverflow),
            // A fatal system error terminates the run (issue #37), but the
            // terminating tick still recorded its checkpoint digest, so it is
            // recorded like any other tick and the run simply ends.
            Err(StepError::Terminated { .. }) => {}
        }
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
        self.recording.command_limits = *self.runner.command_limits();
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
    /// `None` if the replay reproduced the recording exactly. Requesting more
    /// ticks than the recording has checkpoint hashes for is rejected up front
    /// with [`ReplayError::IncompleteCheckpointCoverage`] (issue #51), so
    /// every executed tick is always compared.
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
    /// The recording's simulation schema version is not supported by this
    /// build. Replay-format compatibility and simulation-schema compatibility
    /// are distinct checks: a recording can use a supported container format
    /// but encode state against an incompatible simulation schema.
    UnsupportedSimulationVersion { found: u32, expected: u32 },
    /// The recording's RNG seed-derivation version differs from this build's;
    /// derived stream seeds would differ, so the replay is rejected (issue
    /// #47).
    IncompatibleRngDerivationVersion { found: u32, expected: u32 },
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
    /// A recorded envelope uses an unsupported envelope schema version
    /// (issue #51).
    UnsupportedRecordedEnvelopeVersion {
        index: usize,
        found: u32,
        expected: u32,
    },
    /// A recorded request identity `(actor, session, request_id)` is not
    /// unique (issue #51).
    DuplicateRecordedRequest {
        index: usize,
        actor: ActorId,
        session: SessionId,
        request_id: RequestId,
    },
    /// Recorded command IDs are not the expected ascending sequence from 0 in
    /// submission order, or the replay runner assigned a different ID
    /// (issue #51).
    InvalidRecordedCommandIdSequence {
        index: usize,
        found: CommandId,
        expected: CommandId,
    },
    /// A recorded command targets tick 0, which is never a valid future tick
    /// at submission (issue #51).
    InvalidRecordedCommandTick { index: usize, tick: u64 },
    /// The recording does not contain per-tick hashes for the whole requested
    /// replay interval, so playback could not verify it (issue #51) — in
    /// particular a recording with zero expected hashes cannot verify any
    /// positive tick count.
    IncompleteCheckpointCoverage { recorded: usize, requested: u64 },
    /// The recording's explicit end tick does not match its checkpoint count
    /// (issue #52).
    InconsistentEndTick { end_tick: u64, checkpoints: usize },
    /// The recording requires content that the replay environment did not
    /// provide (issue #52).
    MissingContent { name: String },
    /// A required content digest does not match the replay environment's
    /// content (issue #52).
    IncompatibleContent {
        name: String,
        expected: Digest,
        found: Digest,
    },
    /// The recorded command limits are not valid (issue #52).
    InvalidRecordedCommandLimits(CommandLimitsError),
    /// A recorded command targets a tick beyond the recording's declared end
    /// tick, so it was never applied within the recorded horizon (issue #55).
    CommandBeyondEndTick {
        index: usize,
        tick: u64,
        end_tick: u64,
    },
    /// The simulation configuration version of the recording is not supported
    /// by this build (issue #37): gameplay-affecting configuration is
    /// replay compatibility data.
    IncompatibleConfigVersion { found: u32, expected: u32 },
    /// The replay run was terminated by a fatal system error (issue #37) and
    /// stepping was requested again after the termination.
    RunTerminated { tick: u64 },
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
            Self::UnsupportedSimulationVersion { found, expected } => write!(
                formatter,
                "unsupported simulation schema version {found} (expected {expected})"
            ),
            Self::IncompatibleRngDerivationVersion { found, expected } => write!(
                formatter,
                "incompatible RNG seed-derivation version {found} (expected {expected})"
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
            Self::UnsupportedRecordedEnvelopeVersion {
                index,
                found,
                expected,
            } => write!(
                formatter,
                "recorded envelope {index} has unsupported version {found} (expected {expected})"
            ),
            Self::DuplicateRecordedRequest {
                index,
                actor,
                session,
                request_id,
            } => write!(
                formatter,
                "recorded command {index} duplicates request identity (actor {}, session {}, request {})",
                actor.0, session.0, request_id.0
            ),
            Self::InvalidRecordedCommandIdSequence {
                index,
                found,
                expected,
            } => write!(
                formatter,
                "recorded command {index} has id {} (expected {})",
                found.0, expected.0
            ),
            Self::InvalidRecordedCommandTick { index, tick } => write!(
                formatter,
                "recorded command {index} targets invalid tick {tick}"
            ),
            Self::IncompleteCheckpointCoverage {
                recorded,
                requested,
            } => write!(
                formatter,
                "recording has {recorded} checkpoint hashes but {requested} ticks were requested"
            ),
            Self::InconsistentEndTick {
                end_tick,
                checkpoints,
            } => write!(
                formatter,
                "recorded end tick {end_tick} does not match {checkpoints} checkpoint hashes"
            ),
            Self::MissingContent { name } => {
                write!(formatter, "recording requires missing content: {name}")
            }
            Self::IncompatibleContent {
                name,
                expected,
                found,
            } => write!(
                formatter,
                "content {name} digest mismatch: recording requires {expected:?}, replay has {found:?}"
            ),
            Self::InvalidRecordedCommandLimits(error) => {
                write!(formatter, "recorded command limits are invalid: {error}")
            }
            Self::CommandBeyondEndTick {
                index,
                tick,
                end_tick,
            } => write!(
                formatter,
                "recorded command {index} targets tick {tick}, beyond the recorded end tick {end_tick}"
            ),
            Self::IncompatibleConfigVersion { found, expected } => write!(
                formatter,
                "incompatible simulation config version {found} (expected {expected})"
            ),
            Self::RunTerminated { tick } => {
                write!(formatter, "replay run already terminated at tick {tick}")
            }
            Self::TickOverflow => formatter.write_str("replay tick counter overflowed"),
            Self::HashRecordingDisabled => {
                formatter.write_str("per-tick state hash recording is disabled")
            }
        }
    }
}

impl std::error::Error for ReplayError {}

/// Validates the recording's structure before any playback (issue #51):
/// envelope versions, unique request identities, the ascending command-ID
/// sequence from 0 in submission order, target ticks in the future, and
/// per-tick hash coverage of the whole requested interval. Malformed or
/// incomplete recordings are rejected as invalid input, never silently
/// normalized.
fn validate_recording_structure<P>(
    recording: &ReplayRecording<P>,
    ticks: u64,
    content: &BTreeMap<String, Digest>,
) -> Result<(), ReplayError> {
    // The explicit end tick must be consistent with the checkpoints (issue
    // #52): one hash per executed tick.
    let recorded_hashes = recording.tick_hashes().len();
    if recording.end_tick != recorded_hashes as u64 {
        return Err(ReplayError::InconsistentEndTick {
            end_tick: recording.end_tick,
            checkpoints: recorded_hashes,
        });
    }
    let mut seen_identities = BTreeSet::new();
    for (index, envelope) in recording.commands().iter().enumerate() {
        if envelope.version() != ENVELOPE_VERSION {
            return Err(ReplayError::UnsupportedRecordedEnvelopeVersion {
                index,
                found: envelope.version(),
                expected: ENVELOPE_VERSION,
            });
        }
        let identity = (envelope.actor(), envelope.session(), envelope.request_id());
        if !seen_identities.insert(identity) {
            return Err(ReplayError::DuplicateRecordedRequest {
                index,
                actor: identity.0,
                session: identity.1,
                request_id: identity.2,
            });
        }
        let expected_id = CommandId(index as u64);
        if envelope.command_id() != expected_id {
            return Err(ReplayError::InvalidRecordedCommandIdSequence {
                index,
                found: envelope.command_id(),
                expected: expected_id,
            });
        }
        if envelope.tick() == 0 {
            return Err(ReplayError::InvalidRecordedCommandTick {
                index,
                tick: envelope.tick(),
            });
        }
        // Commands must lie within the recorded horizon (issue #55).
        if envelope.tick() > recording.end_tick {
            return Err(ReplayError::CommandBeyondEndTick {
                index,
                tick: envelope.tick(),
                end_tick: recording.end_tick,
            });
        }
    }
    if ticks > recorded_hashes as u64 {
        return Err(ReplayError::IncompleteCheckpointCoverage {
            recorded: recorded_hashes,
            requested: ticks,
        });
    }
    // Required content identities must be provided and match (issue #52).
    for (name, expected) in recording.content_digests() {
        match content.get(name) {
            None => return Err(ReplayError::MissingContent { name: name.clone() }),
            Some(found) if found != expected => {
                return Err(ReplayError::IncompatibleContent {
                    name: name.clone(),
                    expected: expected.clone(),
                    found: found.clone(),
                });
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Replays a recording into a fresh simulation instance for exactly `ticks`
/// ticks, registering systems through `setup`, and compares the per-tick
/// canonical state digests against the recorded ones.
///
/// `content` provides the replay environment's named content identities (map,
/// roster, ...) as canonical digests; every content digest the recording
/// requires must be present and match (issue #52).
///
/// The recording is validated first (issues #51/#52), then all recorded
/// commands are submitted up front in recorded order, mirroring how the
/// recording was made, so command IDs and bookkeeping match exactly. The
/// recorded command limits are applied to the fresh runner, with the
/// look-ahead limit relaxed to the maximum because replay submits all
/// recorded commands at tick 0.
#[allow(clippy::missing_panics_doc)]
pub fn replay_recording<
    P: CanonicalSerialize + Clone + CanonicalDecode,
    E: CanonicalSerialize + CanonicalDecode,
>(
    recording: &ReplayRecording<P>,
    ticks: u64,
    content: &BTreeMap<String, Digest>,
    setup: impl FnOnce(&mut SimulationRunner<P, E>),
) -> Result<ReplayReport, ReplayError> {
    if recording.format_version() != REPLAY_FORMAT_VERSION {
        return Err(ReplayError::UnsupportedFormatVersion {
            found: recording.format_version(),
            expected: REPLAY_FORMAT_VERSION,
        });
    }
    if recording.simulation_version() != SIMULATION_SCHEMA_VERSION {
        return Err(ReplayError::UnsupportedSimulationVersion {
            found: recording.simulation_version(),
            expected: SIMULATION_SCHEMA_VERSION,
        });
    }
    if recording.config_version() != SIMULATION_CONFIG_VERSION {
        return Err(ReplayError::IncompatibleConfigVersion {
            found: recording.config_version(),
            expected: SIMULATION_CONFIG_VERSION,
        });
    }
    if recording.rng_derivation_version() != SEED_DERIVATION_VERSION {
        return Err(ReplayError::IncompatibleRngDerivationVersion {
            found: recording.rng_derivation_version(),
            expected: SEED_DERIVATION_VERSION,
        });
    }
    validate_recording_structure(recording, ticks, content)?;
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

    let mut runner: SimulationRunner<P, E> =
        SimulationRunner::new(recording.tick_rate(), recording.rng_seed());
    let mut limits = *recording.command_limits();
    limits.max_lookahead_ticks = CommandLimits::MAX_LOOKAHEAD_TICKS;
    runner
        .set_command_limits(limits)
        .map_err(ReplayError::InvalidRecordedCommandLimits)?;
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
    for (index, envelope) in recording.commands().iter().enumerate() {
        let request = CommandRequest {
            request_id: envelope.request_id(),
            actor: envelope.actor(),
            session: envelope.session(),
            tick: envelope.tick(),
            subject: envelope.subject(),
            payload: envelope.payload().clone(),
        };
        let assigned = runner
            .submit_command(request)
            .map_err(ReplayError::InvalidRecordedCommand)?;
        if assigned != envelope.command_id() {
            return Err(ReplayError::InvalidRecordedCommandIdSequence {
                index,
                found: assigned,
                expected: envelope.command_id(),
            });
        }
    }

    let mut tick_hashes = Vec::new();
    for _ in 0..ticks {
        let recorded_before = runner.tick_hashes().len();
        match runner.step_tick() {
            Ok(()) => {}
            Err(StepError::TickOverflow(_)) => return Err(ReplayError::TickOverflow),
            // A fatal system error terminated the replay run (issue #37).
            // The terminating tick still produced a checkpoint digest, which
            // is compared below like any other tick, then playback stops: the
            // termination is canonical state, so a replay that terminates at
            // a different tick than the recording diverges exactly there.
            Err(StepError::Terminated { .. }) => {
                if let Some(digest) = runner.tick_hashes().get(recorded_before) {
                    tick_hashes.push(digest.clone());
                }
                break;
            }
        }
        let digest = runner
            .tick_hashes()
            .get(recorded_before)
            .cloned()
            .ok_or(ReplayError::HashRecordingDisabled)?;
        tick_hashes.push(digest);
    }

    let recorded = recording.tick_hashes();
    // Pre-flight validation guarantees ticks <= recorded.len() (issue #51), so
    // every executed tick is compared; an empty expected-hash vector with a
    // positive tick count is rejected before playback instead of silently
    // reporting no divergence.
    let first_divergent_tick = tick_hashes
        .iter()
        .zip(recorded)
        .enumerate()
        .find_map(|(index, (actual, expected))| (actual != expected).then_some(index as u64 + 1));

    Ok(ReplayReport {
        ticks_executed: tick_hashes.len() as u64,
        tick_hashes,
        first_divergent_tick,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{ActorId, CommandPayload, CommandVerdict, RequestId, SessionId};
    use crate::context::{SystemError, SystemOutput, TickContext};
    use crate::scenario::{SCENARIO_TICKS, record_scenario, setup_scenario};
    use crate::simulation::{
        Phase, SIMULATION_CONFIG_VERSION, Simulation, SimulationRunner, SimulationSystem, SystemId,
        TickRate,
    };

    #[test]
    fn same_build_replay_reproduces_per_tick_hashes() {
        let recording = record_scenario(SCENARIO_TICKS);
        let report = replay_recording(&recording, SCENARIO_TICKS, &BTreeMap::new(), setup_scenario)
            .expect("replay should succeed");

        assert_eq!(report.ticks_executed(), SCENARIO_TICKS);
        assert!(!report.diverged());
        assert_eq!(report.first_divergent_tick(), None);
        assert_eq!(report.tick_hashes(), recording.tick_hashes());

        // Replaying twice gives identical results.
        let again = replay_recording(&recording, SCENARIO_TICKS, &BTreeMap::new(), setup_scenario)
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
            replay_recording(&recording, 4, &BTreeMap::new(), setup_scenario),
            Err(ReplayError::UnsupportedFormatVersion {
                found: REPLAY_FORMAT_VERSION + 1,
                expected: REPLAY_FORMAT_VERSION
            })
        );
    }

    #[test]
    fn unsupported_simulation_version_is_rejected_before_any_setup() {
        let mut recording = record_scenario(4);
        recording.simulation_version = SIMULATION_SCHEMA_VERSION + 1;
        // The rejection must happen before the replay runner is even
        // constructed: the setup closure panics if it is ever called.
        let result = replay_recording(
            &recording,
            4,
            &BTreeMap::new(),
            |_: &mut SimulationRunner<CommandPayload>| {
                panic!("unsupported simulation version must be rejected before setup")
            },
        );
        assert_eq!(
            result,
            Err(ReplayError::UnsupportedSimulationVersion {
                found: SIMULATION_SCHEMA_VERSION + 1,
                expected: SIMULATION_SCHEMA_VERSION
            })
        );
        // An unmodified recording still replays and verifies.
        let recording = record_scenario(4);
        let report = replay_recording(&recording, 4, &BTreeMap::new(), setup_scenario)
            .expect("valid recording replays");
        assert!(!report.diverged());
        assert_eq!(report.tick_hashes(), recording.tick_hashes());
    }

    struct RejectingValidator;

    impl SimulationSystem<CommandPayload> for RejectingValidator {
        fn id(&self) -> SystemId {
            SystemId("rejecting-validator")
        }

        fn phase(&self) -> Phase {
            Phase::ApplyCommands
        }

        fn order(&self) -> u64 {
            0
        }

        fn name(&self) -> &'static str {
            "rejecting-validator"
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }

        fn validate_command(
            &mut self,
            _ctx: &TickContext,
            _simulation: &Simulation<CommandPayload>,
            envelope: &CommandEnvelope<CommandPayload>,
        ) -> CommandVerdict {
            match envelope.payload() {
                CommandPayload::Diagnostics { value: 42 } => CommandVerdict::Reject { reason: 7 },
                _ => CommandVerdict::Accept,
            }
        }

        fn run(
            &mut self,
            _ctx: &TickContext,
            _simulation: &mut Simulation<CommandPayload>,
            _phase: Phase,
        ) -> Result<SystemOutput<CommandPayload>, SystemError> {
            Ok(SystemOutput::default())
        }
    }

    #[test]
    fn recorded_commands_must_lie_within_the_end_tick() {
        // A command beyond the declared end tick is rejected (issue #55).
        let mut recording = record_scenario(4);
        let envelope = &recording.commands()[0];
        recording.push_command(
            CommandEnvelope::new(
                ENVELOPE_VERSION,
                RequestId(100),
                CommandId(2),
                envelope.actor(),
                envelope.session(),
                100,
                None,
                CommandPayload::Diagnostics { value: 1 },
            )
            .expect("current version"),
        );
        assert_eq!(
            replay_recording(&recording, 4, &BTreeMap::new(), setup_scenario),
            Err(ReplayError::CommandBeyondEndTick {
                index: recording.commands().len() - 1,
                tick: 100,
                end_tick: 4
            })
        );
    }

    #[test]
    fn replay_metadata_includes_end_tick_limits_and_content() {
        let recording = record_scenario(4);
        assert_eq!(recording.end_tick(), 4);
        assert_eq!(recording.end_tick(), recording.tick_hashes().len() as u64);
        assert_eq!(recording.command_limits(), &CommandLimits::DEFAULT);
        assert!(recording.content_digests().is_empty());

        // Inconsistent end tick is rejected before playback (issue #52).
        let mut recording = record_scenario(4);
        recording.end_tick = 99;
        assert_eq!(
            replay_recording(&recording, 4, &BTreeMap::new(), setup_scenario),
            Err(ReplayError::InconsistentEndTick {
                end_tick: 99,
                checkpoints: 4
            })
        );
    }

    #[test]
    fn required_content_identities_are_validated_before_playback() {
        let mut recorder = ReplayRecorder::new(TickRate::DEFAULT, 0x00C0_FFEE);
        setup_scenario(recorder.runner_mut());
        let map_digest = crate::hash::canonical_digest(&"crossing-map-bytes".to_owned());
        recorder.push_content_digest("map", map_digest.clone());
        recorder.advance_ticks(2).expect("ticks");
        let recording = recorder.finish();
        assert_eq!(recording.content_digests().get("map"), Some(&map_digest));

        // Missing content is rejected.
        assert_eq!(
            replay_recording(&recording, 2, &BTreeMap::new(), setup_scenario),
            Err(ReplayError::MissingContent {
                name: "map".to_owned()
            })
        );
        // A different content digest is rejected.
        let wrong = BTreeMap::from([(
            "map".to_owned(),
            crate::hash::canonical_digest(&"other".to_owned()),
        )]);
        assert_eq!(
            replay_recording(&recording, 2, &wrong, setup_scenario),
            Err(ReplayError::IncompatibleContent {
                name: "map".to_owned(),
                expected: map_digest.clone(),
                found: crate::hash::canonical_digest(&"other".to_owned()),
            })
        );
        // The matching content replays and verifies.
        let right = BTreeMap::from([("map".to_owned(), map_digest)]);
        let report =
            replay_recording(&recording, 2, &right, setup_scenario).expect("replay succeeds");
        assert!(!report.diverged());
    }

    #[test]
    fn recorded_command_limits_are_applied_to_the_replay_runner() {
        let limits = CommandLimits {
            max_lookahead_ticks: 50,
            ..CommandLimits::DEFAULT
        };
        let mut recorder = ReplayRecorder::new(TickRate::DEFAULT, 0x00C0_FFEE);
        recorder
            .runner_mut()
            .set_command_limits(limits)
            .expect("valid limits");
        setup_scenario(recorder.runner_mut());
        recorder.advance_ticks(2).expect("ticks");
        let recording = recorder.finish();
        assert_eq!(recording.command_limits(), &limits);
        let report =
            replay_recording(&recording, 2, &BTreeMap::new(), setup_scenario).expect("replay");
        assert!(!report.diverged());
    }

    #[test]
    fn malformed_recorded_commands_are_rejected_before_playback() {
        // Duplicate request identity (same actor+session+request id).
        let mut recording = record_scenario(4);
        let dup = recording.commands()[0].clone();
        recording.commands_mut()[1] = dup;
        assert_eq!(
            replay_recording(&recording, 4, &BTreeMap::new(), setup_scenario),
            Err(ReplayError::DuplicateRecordedRequest {
                index: 1,
                actor: ActorId(0),
                session: SessionId(0),
                request_id: RequestId(1)
            })
        );

        // Command IDs must be the ascending sequence from 0.
        let mut recording = record_scenario(4);
        let envelope = &recording.commands()[1];
        recording.commands_mut()[1] = CommandEnvelope::new(
            ENVELOPE_VERSION,
            envelope.request_id(),
            CommandId(5),
            envelope.actor(),
            envelope.session(),
            envelope.tick(),
            envelope.subject(),
            envelope.payload().clone(),
        )
        .expect("current version");
        assert_eq!(
            replay_recording(&recording, 4, &BTreeMap::new(), setup_scenario),
            Err(ReplayError::InvalidRecordedCommandIdSequence {
                index: 1,
                found: CommandId(5),
                expected: CommandId(1)
            })
        );

        // Tick 0 is never a valid target tick at submission.
        let mut recording = record_scenario(4);
        let envelope = &recording.commands()[0];
        recording.commands_mut()[0] = CommandEnvelope::new(
            ENVELOPE_VERSION,
            envelope.request_id(),
            envelope.command_id(),
            envelope.actor(),
            envelope.session(),
            0,
            envelope.subject(),
            envelope.payload().clone(),
        )
        .expect("current version");
        assert_eq!(
            replay_recording(&recording, 4, &BTreeMap::new(), setup_scenario),
            Err(ReplayError::InvalidRecordedCommandTick { index: 0, tick: 0 })
        );
    }

    #[test]
    fn incomplete_checkpoint_coverage_is_rejected() {
        // Overlong request: more ticks than recorded hashes.
        let recording = record_scenario(4);
        assert_eq!(
            replay_recording(&recording, 5, &BTreeMap::new(), setup_scenario),
            Err(ReplayError::IncompleteCheckpointCoverage {
                recorded: 4,
                requested: 5
            })
        );
        // The empty-hash edge case: zero recorded hashes cannot verify any
        // positive tick count (previously reported no divergence).
        let empty = record_scenario(0);
        assert!(empty.tick_hashes().is_empty());
        assert_eq!(
            replay_recording(&empty, 3, &BTreeMap::new(), setup_scenario),
            Err(ReplayError::IncompleteCheckpointCoverage {
                recorded: 0,
                requested: 3
            })
        );
        // A truncated replay (fewer ticks than recorded) still verifies fine.
        let report = replay_recording(&recording, 2, &BTreeMap::new(), setup_scenario)
            .expect("partial replay ok");
        assert!(!report.diverged());
        assert_eq!(report.ticks_executed(), 2);
    }

    #[test]
    fn incompatible_rng_derivation_version_is_rejected() {
        let mut recording = record_scenario(4);
        assert_eq!(
            recording.rng_derivation_version(),
            crate::rng::SEED_DERIVATION_VERSION
        );
        recording.rng_derivation_version = crate::rng::SEED_DERIVATION_VERSION + 1;
        assert_eq!(
            replay_recording(&recording, 4, &BTreeMap::new(), setup_scenario),
            Err(ReplayError::IncompatibleRngDerivationVersion {
                found: crate::rng::SEED_DERIVATION_VERSION + 1,
                expected: crate::rng::SEED_DERIVATION_VERSION
            })
        );
    }

    #[test]
    fn replay_reproduces_both_applied_and_rejected_commands() {
        let setup = |runner: &mut SimulationRunner<CommandPayload>| {
            runner.register_system(RejectingValidator).expect("id");
        };
        let mut recorder = ReplayRecorder::new(TickRate::DEFAULT, 0x5EED);
        setup(recorder.runner_mut());
        for (request_id, value) in [(1_u64, 1_u64), (2, 42), (3, 3)] {
            recorder
                .submit_command(CommandRequest {
                    request_id: RequestId(request_id),
                    actor: ActorId(0),
                    session: SessionId(0),
                    tick: 2,
                    subject: None,
                    payload: CommandPayload::Diagnostics { value },
                })
                .expect("structurally valid");
        }
        recorder.advance_ticks(3).expect("ticks");
        let recording = recorder.finish();

        let report = replay_recording(&recording, 3, &BTreeMap::new(), setup)
            .expect("replay should succeed");
        assert!(!report.diverged(), "rejected commands replay identically");
        assert_eq!(report.tick_hashes(), recording.tick_hashes());
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
            envelope.session(),
            envelope.tick(),
            envelope.subject(),
            CommandPayload::Diagnostics { value: 999_999 },
        )
        .expect("current version");

        // The pending command queue is authoritative input state and is part
        // of the canonical digest from submission, so the altered command
        // diverges the state already at tick 1.
        let report = replay_recording(&recording, SCENARIO_TICKS, &BTreeMap::new(), setup_scenario)
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
            replay_recording(&recording, 4, &BTreeMap::new(), setup_scenario),
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
        let report = replay_recording(&recording, SCENARIO_TICKS, &BTreeMap::new(), setup_scenario)
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
        let result = replay_recording(&recording, 4, &BTreeMap::new(), |runner| {
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
        let result = replay_recording(
            &recording,
            4,
            &BTreeMap::new(),
            |_: &mut SimulationRunner<CommandPayload>| {},
        );
        assert!(matches!(
            result,
            Err(ReplayError::IncompatibleSystemManifest { .. })
        ));
        // The matching setup is accepted (covered by the other tests).
        let report = replay_recording(&recording, 4, &BTreeMap::new(), setup_scenario)
            .expect("matching manifest");
        assert!(!report.diverged());
    }

    #[test]
    fn truncated_replay_verifies_the_executed_prefix() {
        let recording = record_scenario(SCENARIO_TICKS);
        // Replay only half the recorded ticks (issue #51): every executed
        // tick is verified against the recording; overlong requests are
        // rejected with a structured error instead (see
        // incomplete_checkpoint_coverage_is_rejected).
        let report = replay_recording(
            &recording,
            SCENARIO_TICKS / 2,
            &BTreeMap::new(),
            setup_scenario,
        )
        .expect("replay should succeed");
        assert_eq!(report.ticks_executed(), SCENARIO_TICKS / 2);
        assert_eq!(report.first_divergent_tick(), None);
        assert_eq!(
            report.tick_hashes(),
            &recording.tick_hashes()[..SCENARIO_TICKS as usize / 2]
        );
    }
    // ---- issue #37: config version in replay compatibility data; fatal
    // termination is canonical and replays reproduce it ----

    #[test]
    fn replay_rejects_incompatible_config_version() {
        let mut recording = record_scenario(SCENARIO_TICKS);
        recording.config_version = 0;
        assert_eq!(
            replay_recording(&recording, SCENARIO_TICKS, &BTreeMap::new(), setup_scenario),
            Err(ReplayError::IncompatibleConfigVersion {
                found: 0,
                expected: SIMULATION_CONFIG_VERSION,
            })
        );
    }

    #[test]
    fn recordings_carry_the_current_config_version() {
        let recording = record_scenario(SCENARIO_TICKS);
        assert_eq!(recording.config_version(), SIMULATION_CONFIG_VERSION);
    }

    struct FatalAtTick2;

    impl SimulationSystem<CommandPayload> for FatalAtTick2 {
        fn id(&self) -> SystemId {
            SystemId("fatal-at-2")
        }

        fn phase(&self) -> Phase {
            Phase::UpdateEconomy
        }

        fn order(&self) -> u64 {
            0
        }

        fn name(&self) -> &'static str {
            "fatal-at-2"
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }

        fn run(
            &mut self,
            ctx: &TickContext,
            _simulation: &mut Simulation<CommandPayload>,
            _phase: Phase,
        ) -> Result<SystemOutput<CommandPayload>, SystemError> {
            if ctx.tick() == 2 {
                Err(SystemError::Fatal {
                    system: self.id(),
                    code: 9,
                    message: "fatal probe failure",
                })
            } else {
                Ok(SystemOutput::default())
            }
        }
    }

    #[test]
    fn replay_reproduces_a_fatal_termination_without_divergence() {
        let setup = |runner: &mut SimulationRunner<CommandPayload>| {
            runner.register_system(FatalAtTick2).expect("id");
        };
        let mut recorder = ReplayRecorder::<CommandPayload>::new(TickRate::DEFAULT, 0xABCD);
        setup(recorder.runner_mut());
        // Tick 1 runs; tick 2 terminates fatally but still records its
        // checkpoint digest, so the recording covers both ticks.
        recorder.advance_ticks(2).expect("record two ticks");
        let recording = recorder.finish();
        assert_eq!(recording.tick_hashes().len(), 2);
        assert_eq!(recording.end_tick(), 2);

        let report =
            replay_recording(&recording, 2, &BTreeMap::new(), setup).expect("replay verifies");
        assert_eq!(report.ticks_executed(), 2);
        assert_eq!(report.first_divergent_tick(), None);
        assert_eq!(report.tick_hashes(), recording.tick_hashes());
    }

    #[test]
    fn stepping_a_terminated_recording_is_an_explicit_error() {
        let setup = |runner: &mut SimulationRunner<CommandPayload>| {
            runner.register_system(FatalAtTick2).expect("id");
        };
        let mut recorder = ReplayRecorder::<CommandPayload>::new(TickRate::DEFAULT, 0xABCD);
        setup(recorder.runner_mut());
        recorder.advance_ticks(2).expect("record two ticks");
        // The run terminated at tick 2; stepping again is an explicit error,
        // not a silent no-op.
        assert_eq!(
            recorder.step_tick(),
            Err(ReplayError::RunTerminated { tick: 2 })
        );
    }
}
