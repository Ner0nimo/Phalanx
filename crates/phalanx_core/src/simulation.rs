//! Fixed-tick authoritative simulation runner.
//!
//! This module implements the simulation schedule contract from ADR-0001 and
//! `RtsEngineDoc.md` §6.1: a CPU-only, deterministic, fixed-tick loop whose
//! phase order is explicit and versioned. The runner advances exclusively in
//! whole ticks, never reads wall-clock time, and has no dependency on
//! rendering, windowing, or GPU crates. A future application loop may call
//! [`SimulationRunner::advance_ticks`] from its own frame timing; simulation
//! progression is independent of the render frame rate.
//!
//! [`Simulation`] owns the authoritative primitives defined by issue #6:
//! stable entity IDs ([`crate::entity`]), deterministic RNG streams
//! ([`crate::rng`]), and the tick-indexed command buffer
//! ([`crate::command`]). The phase hooks are placeholders: they define *when*
//! systems run, not gameplay behavior. No gameplay systems exist yet.
//!
//! Every system hook receives the single authoritative [`TickContext`] and
//! returns a structured [`SystemOutput`] / [`SystemError`] (issue #37,
//! [`crate::context`]): systems read tick/time/configuration from one context,
//! emit typed events and intents through a deterministic observable path, and
//! report failures as structured errors instead of panicking.

use std::collections::BTreeMap;

use crate::command::{
    ActorId, CommandEnvelope, CommandId, CommandLimits, CommandLimitsError, CommandOutcome,
    CommandRequest, CommandSubmitError, CommandVerdict, ENVELOPE_VERSION, RequestId, SessionId,
};
use crate::context::{
    MAX_EVENT_TRANSCRIPT, SYSTEM_ERROR_INTENT_REJECTED, SystemError, SystemEvent, SystemIntent,
    SystemOutput, SystemTermination, TickContext,
};
use crate::entity::EntityAllocator;
use crate::hash::{
    CANONICAL_ENCODING_VERSION, CanonicalDecode, CanonicalError, CanonicalReader,
    CanonicalSerialize, CanonicalSink, Digest, DigestAlgorithm, HashSink, SnapshotSink,
    canonical_digest,
};
use crate::rng::RngStreams;

pub use crate::TickOverflow;

/// The default fixed simulation rate: 20 ticks per second (initial proposal in
/// `RtsEngineDoc.md` §3.2; the final tick rate remains an open question in
/// ADR-0001).
pub const DEFAULT_TICKS_PER_SECOND: u32 = 20;

/// The version of the authoritative simulation state layout. Recorded in
/// replays and savegames; bump it on any change that alters canonical state.
/// The simulation schema version. Bumped when the canonical simulation state
/// layout changes incompatibly (schema 2: bounded command bookkeeping, issue
/// #39; schema 3: request identity scoped to (actor, session), issue #41;
/// schema 4: canonical per-command application outcomes, issue #43; schema 5:
/// RNG registry finalization flag, issue #47; schema 6: versioned simulation
/// config plus the canonical system-event transcript and termination record,
/// issue #37; schema 7: same-tick application order is the stable arbitration
/// key, not command-ID/arrival order, issue #42).
/// Older-schema replays are rejected at the boundary (issue #30).
pub const SIMULATION_SCHEMA_VERSION: u32 = 7;

/// The fixed simulation rate. This configures simulation time, not wall-clock
/// scheduling: the runner itself never reads the clock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TickRate {
    ticks_per_second: u32,
}

impl TickRate {
    /// The default rate: 20 ticks per second.
    pub const DEFAULT: Self = Self {
        ticks_per_second: DEFAULT_TICKS_PER_SECOND,
    };

    /// Creates a tick rate, rejecting zero.
    pub const fn new(ticks_per_second: u32) -> Result<Self, InvalidTickRate> {
        if ticks_per_second == 0 {
            Err(InvalidTickRate)
        } else {
            Ok(Self { ticks_per_second })
        }
    }

    /// Returns the configured number of simulation ticks per second.
    #[must_use]
    pub const fn ticks_per_second(self) -> u32 {
        self.ticks_per_second
    }
}

impl Default for TickRate {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A tick rate of zero is not a valid simulation rate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidTickRate;

impl std::fmt::Display for InvalidTickRate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("tick rate must be at least one tick per second")
    }
}

impl std::error::Error for InvalidTickRate {}

/// The version of the gameplay-affecting simulation configuration layout
/// (issue #37). Bumped when the set or meaning of configuration fields changes
/// incompatibly; written into the canonical simulation state and recorded in
/// replay compatibility data, and rejected on mismatch at the boundary.
pub const SIMULATION_CONFIG_VERSION: u32 = 1;

/// Gameplay-affecting simulation configuration (issue #37).
///
/// This is the single authoritative configuration object: it lives in
/// [`Simulation`] (so it is part of the canonical state and of checkpoint
/// digests), is exposed to systems through [`TickContext`], and is recorded in
/// replay compatibility data. The tick rate configures simulation time only;
/// the runner never reads the wall clock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SimulationConfig {
    tick_rate: TickRate,
}

impl SimulationConfig {
    /// The default configuration: 20 ticks per second.
    pub const DEFAULT: Self = Self {
        tick_rate: TickRate::DEFAULT,
    };

    /// Creates a configuration with the given fixed tick rate.
    #[must_use]
    pub const fn new(tick_rate: TickRate) -> Self {
        Self { tick_rate }
    }

    /// The configured fixed tick rate.
    #[must_use]
    pub const fn tick_rate(&self) -> TickRate {
        self.tick_rate
    }

    /// The configuration layout version (see [`SIMULATION_CONFIG_VERSION`]).
    #[must_use]
    pub const fn version(&self) -> u32 {
        SIMULATION_CONFIG_VERSION
    }
}

impl CanonicalSerialize for SimulationConfig {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u32(self.tick_rate.ticks_per_second());
    }
}

impl CanonicalDecode for SimulationConfig {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        let ticks_per_second = reader.read_u32()?;
        let tick_rate = TickRate::new(ticks_per_second)
            .map_err(|_| CanonicalError::InvalidState("tick rate of zero"))?;
        Ok(Self { tick_rate })
    }
}

/// The explicit, versioned simulation phase schedule (`RtsEngineDoc.md` §6.1,
/// ADR-0001 Decision §3).
///
/// Execution order is the declaration order of [`Phase::SCHEDULE`], never the
/// order in which systems are registered. Changing the schedule requires an
/// ADR or an explicit simulation-version change because it can alter replay
/// results and game balance.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Phase {
    /// Ingest validated commands assigned to this tick.
    IngestCommands = 1,
    /// Apply command changes and resolve command lifecycle transitions.
    ApplyCommands,
    /// Update strategic/group intent and formation targets.
    UpdateStrategicIntent,
    /// Update economy, resource extraction, and supply production.
    UpdateEconomy,
    /// Update construction, production, and research progress.
    UpdateProduction,
    /// Update navigation requests and scheduled path/flow-field work.
    UpdateNavigation,
    /// Compute unit movement intent and local constraints.
    UpdateMovementIntent,
    /// Resolve authoritative movement and collision constraints.
    ResolveMovement,
    /// Acquire/validate targets and resolve combat decisions.
    ResolveCombat,
    /// Advance authoritative projectiles and resolve impacts.
    AdvanceProjectiles,
    /// Apply damage, deaths, capture progress, and destruction effects.
    ApplyDamageAndDeath,
    /// Update territory, supply connectivity, and visibility as scheduled.
    UpdateTerritorySupplyVisibility,
    /// Emit deterministic gameplay events and update derived caches.
    EmitEvents,
    /// Validate critical invariants in debug/test builds.
    ValidateInvariants,
    /// Produce canonical state hash and replay checkpoint as configured.
    ProduceStateHash,
}

impl Phase {
    /// The authoritative execution order of every tick.
    pub const SCHEDULE: [Phase; 15] = [
        Phase::IngestCommands,
        Phase::ApplyCommands,
        Phase::UpdateStrategicIntent,
        Phase::UpdateEconomy,
        Phase::UpdateProduction,
        Phase::UpdateNavigation,
        Phase::UpdateMovementIntent,
        Phase::ResolveMovement,
        Phase::ResolveCombat,
        Phase::AdvanceProjectiles,
        Phase::ApplyDamageAndDeath,
        Phase::UpdateTerritorySupplyVisibility,
        Phase::EmitEvents,
        Phase::ValidateInvariants,
        Phase::ProduceStateHash,
    ];

    /// Returns the zero-based position of this phase in [`Phase::SCHEDULE`].
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize - 1
    }

    /// Returns a stable machine-readable name for diagnostics and tests.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Phase::IngestCommands => "ingest_commands",
            Phase::ApplyCommands => "apply_commands",
            Phase::UpdateStrategicIntent => "update_strategic_intent",
            Phase::UpdateEconomy => "update_economy",
            Phase::UpdateProduction => "update_production",
            Phase::UpdateNavigation => "update_navigation",
            Phase::UpdateMovementIntent => "update_movement_intent",
            Phase::ResolveMovement => "resolve_movement",
            Phase::ResolveCombat => "resolve_combat",
            Phase::AdvanceProjectiles => "advance_projectiles",
            Phase::ApplyDamageAndDeath => "apply_damage_and_death",
            Phase::UpdateTerritorySupplyVisibility => "update_territory_supply_visibility",
            Phase::EmitEvents => "emit_events",
            Phase::ValidateInvariants => "validate_invariants",
            Phase::ProduceStateHash => "produce_state_hash",
        }
    }

    /// Returns the number of phases in the schedule.
    #[must_use]
    pub const fn count() -> usize {
        Self::SCHEDULE.len()
    }
}

impl CanonicalSerialize for Phase {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u32(*self as u32);
    }
}

impl CanonicalDecode for Phase {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        let tag = reader.read_u32()?;
        Self::SCHEDULE
            .get(tag.wrapping_sub(1) as usize)
            .filter(|phase| **phase as u32 == tag)
            .copied()
            .ok_or(CanonicalError::UnknownTag(tag))
    }
}

/// One entry of the finalized [`SystemManifest`]: a system's stable identity
/// and its exact position in the schedule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SystemManifestEntry {
    /// The system's stable unique ID.
    pub id: SystemId,
    /// The phase the system runs in.
    pub phase: Phase,
    /// The explicit in-phase ordering key.
    pub order: u64,
}

impl CanonicalSerialize for SystemManifestEntry {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        self.id.write_canonical(sink);
        self.phase.write_canonical(sink);
        sink.write_u64(self.order);
    }
}

/// The finalized, immutable system registry of a runner (issue #35): the
/// systems in deterministic execution order. Recorded in replay metadata;
/// its canonical digest is the manifest version compared during replay.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SystemManifest {
    entries: Vec<SystemManifestEntry>,
}

impl SystemManifest {
    /// The manifest entries in execution order (phase schedule, then the
    /// explicit per-phase ordering key).
    #[must_use]
    pub fn entries(&self) -> &[SystemManifestEntry] {
        &self.entries
    }

    /// Returns the number of registered systems.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether the manifest is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The canonical digest of the manifest: the stable manifest version,
    /// identical across independent runner construction with the same
    /// registrations in any registration order.
    #[must_use]
    pub fn digest(&self) -> Digest {
        canonical_digest(self)
    }
}

impl CanonicalSerialize for SystemManifest {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u64(self.entries.len() as u64);
        for entry in &self.entries {
            entry.write_canonical(sink);
        }
    }
}

/// Authoritative simulation state owned by the CPU simulation (ADR-0001
/// Decision §1).
///
/// The state is the versioned gameplay-affecting configuration (issue #37),
/// the simulation tick, the entity allocator, the deterministic RNG streams,
/// the tick-indexed command bookkeeping, and the canonical system-event
/// transcript with its termination record (issue #37). Gameplay world state
/// lands in later milestones; nothing here pretends gameplay systems exist.
///
/// `E` is the typed system-event payload; it defaults to `()` for
/// event-less runs.
pub struct Simulation<P, E = ()> {
    tick: u64,
    rng: RngStreams,
    entities: EntityAllocator,
    pending_commands: BTreeMap<u64, Vec<CommandEnvelope<P>>>,
    /// Durable transcript of commands applied at tick boundaries, in
    /// application order (the stable arbitration key within each tick, issue
    /// #42). Part of the
    /// canonical state, bounded to the runner's `max_applied_history` window
    /// (issue #39); the full stream remains available in replay recordings.
    applied_commands: Vec<CommandEnvelope<P>>,
    /// Durable per-command application outcomes, parallel to
    /// `applied_commands` (issue #43, lifecycle stage 4). Rejected-at-
    /// application commands stay in the transcript, so replays reproduce
    /// them. Bounded to the same window as `applied_commands`.
    applied_outcomes: Vec<(CommandId, CommandOutcome)>,
    /// Total number of commands applied so far (the transcript keeps only a
    /// bounded window, so the count preserves the total).
    applied_command_count: u64,
    /// Transient view of the commands being applied on the tick currently
    /// executing: a range into `applied_commands`. Rebuilt from the pending
    /// queue at each tick boundary and never part of the canonical encoding,
    /// so a recorded per-tick checkpoint digest equals the public digest once
    /// `step_tick` returns (issue #38).
    current_tick_commands: std::ops::Range<usize>,
    /// Per-(actor, session) high-water mark of accepted request IDs (issue
    /// #41). A retry at or below the mark of the same actor+session is
    /// rejected as a duplicate (never applied twice), while the dedup state
    /// stays bounded to one entry per actor+session instead of growing with
    /// every accepted request (issue #39). A new session of the same actor
    /// starts a fresh local request-ID sequence.
    last_request_id_per_session: BTreeMap<(ActorId, SessionId), RequestId>,
    next_command_id: u64,
    /// Gameplay-affecting configuration (issue #37). Part of the canonical
    /// state; versioned via [`SIMULATION_CONFIG_VERSION`] and recorded in
    /// replay compatibility data.
    config: SimulationConfig,
    /// Canonical transcript of system-emitted events and failures, in
    /// emission order (issue #37). Bounded to [`MAX_EVENT_TRANSCRIPT`]
    /// entries; `emitted_event_count` preserves the full total.
    events: Vec<SystemEvent<E>>,
    /// Total number of system events emitted since tick 0 (the transcript
    /// keeps only a bounded window of the most recent ones).
    emitted_event_count: u64,
    /// Set when a fatal system error terminated the run (issue #37).
    /// Canonical: snapshots and replays reproduce the termination.
    terminated: Option<SystemTermination>,
}

impl<P, E> Simulation<P, E> {
    /// Creates the initial authoritative state at tick 0 with the given
    /// explicit master RNG seed and the default configuration.
    #[must_use]
    pub fn new(rng_seed: u64) -> Self {
        Self::with_config(SimulationConfig::DEFAULT, rng_seed)
    }

    /// Creates the initial authoritative state at tick 0 with the given
    /// explicit master RNG seed and the given gameplay-affecting
    /// configuration (issue #37).
    #[must_use]
    pub fn with_config(config: SimulationConfig, rng_seed: u64) -> Self {
        Self {
            tick: 0,
            rng: RngStreams::new(rng_seed),
            entities: EntityAllocator::new(),
            pending_commands: BTreeMap::new(),
            applied_commands: Vec::new(),
            applied_outcomes: Vec::new(),
            applied_command_count: 0,
            current_tick_commands: 0..0,
            last_request_id_per_session: BTreeMap::new(),
            next_command_id: 0,
            config,
            events: Vec::new(),
            emitted_event_count: 0,
            terminated: None,
        }
    }

    /// Returns the current authoritative tick. The tick counter is part of
    /// authoritative simulation state and is included in saves, replays, and
    /// state hashes.
    #[must_use]
    pub const fn tick(&self) -> u64 {
        self.tick
    }

    /// Returns the deterministic RNG stream registry.
    #[must_use]
    pub const fn rng(&self) -> &RngStreams {
        &self.rng
    }

    /// Returns mutable access to the RNG stream registry for systems.
    #[must_use]
    pub const fn rng_mut(&mut self) -> &mut RngStreams {
        &mut self.rng
    }

    /// Returns the entity ID allocator.
    #[must_use]
    pub const fn entities(&self) -> &EntityAllocator {
        &self.entities
    }

    /// Returns mutable access to the entity ID allocator for systems.
    #[must_use]
    pub const fn entities_mut(&mut self) -> &mut EntityAllocator {
        &mut self.entities
    }

    /// Returns the commands assigned to the tick currently being executed, in
    /// arbitration-key order (issue #42). They are drained from the pending buffer at
    /// the tick boundary and are only visible while that tick runs.
    #[must_use]
    pub fn commands_for_current_tick(&self) -> &[CommandEnvelope<P>] {
        &self.applied_commands[self.current_tick_commands.clone()]
    }

    /// Returns the durable transcript of commands applied at tick boundaries,
    /// in application order. This is authoritative input history and is part
    /// of the canonical state (bounded to the runner's configured window,
    /// issue #39).
    #[must_use]
    pub fn applied_commands(&self) -> &[CommandEnvelope<P>] {
        &self.applied_commands
    }

    /// Returns the durable per-command application outcomes in application
    /// order (issue #43). Parallel to [`Simulation::applied_commands`].
    #[must_use]
    pub fn applied_outcomes(&self) -> &[(CommandId, CommandOutcome)] {
        &self.applied_outcomes
    }

    /// Returns the recorded outcome of an applied command, if it is still in
    /// the bounded transcript window.
    #[must_use]
    pub fn command_outcome(&self, command_id: CommandId) -> Option<CommandOutcome> {
        self.applied_outcomes
            .iter()
            .find(|(id, _)| *id == command_id)
            .map(|(_, outcome)| *outcome)
    }

    /// Returns the total number of commands applied since tick 0 (the
    /// transcript keeps only a bounded window of the most recent ones).
    #[must_use]
    pub const fn applied_command_count(&self) -> u64 {
        self.applied_command_count
    }

    /// Returns the ticks that still have queued commands, in ascending order.
    pub fn pending_command_ticks(
        &self,
    ) -> std::collections::btree_map::Keys<'_, u64, Vec<CommandEnvelope<P>>> {
        self.pending_commands.keys()
    }

    /// Returns the gameplay-affecting configuration (issue #37). Also
    /// exposed to systems through [`TickContext`].
    #[must_use]
    pub const fn config(&self) -> SimulationConfig {
        self.config
    }

    /// Returns the canonical system-event transcript (bounded window), in
    /// emission order (issue #37).
    #[must_use]
    pub fn events(&self) -> &[SystemEvent<E>] {
        &self.events
    }

    /// Returns the total number of system events emitted since tick 0 (the
    /// transcript keeps only a bounded window of the most recent ones).
    #[must_use]
    pub const fn emitted_event_count(&self) -> u64 {
        self.emitted_event_count
    }

    /// Returns the fatal termination record, if a fatal system error
    /// terminated the run (issue #37).
    #[must_use]
    pub const fn terminated(&self) -> Option<SystemTermination> {
        self.terminated
    }

    /// Records typed events emitted by the system at `system_index` (its
    /// position in the runner's execution-ordered system list) during `tick`
    /// into the canonical transcript (issue #37).
    pub fn record_events(&mut self, tick: u64, system_index: u32, events: Vec<E>) {
        self.emitted_event_count = self.emitted_event_count.saturating_add(events.len() as u64);
        for event in events {
            self.events
                .push(SystemEvent::output(tick, system_index, event));
        }
        self.trim_events();
    }

    /// Records a system failure (non-fatal, or the fatal terminating one) in
    /// the canonical transcript (issue #37).
    pub fn record_failure(&mut self, tick: u64, system_index: u32, code: u32) {
        self.emitted_event_count = self.emitted_event_count.saturating_add(1);
        self.events
            .push(SystemEvent::failure(tick, system_index, code));
        self.trim_events();
    }

    /// Records a fatal failure and marks the run terminated (issue #37).
    pub fn terminate(&mut self, tick: u64, system_index: u32, code: u32) {
        self.record_failure(tick, system_index, code);
        self.terminated = Some(SystemTermination::new(tick, system_index, code));
    }

    /// Keeps only the most recent [`MAX_EVENT_TRANSCRIPT`] transcript entries.
    fn trim_events(&mut self) {
        if self.events.len() > MAX_EVENT_TRANSCRIPT {
            let drop = self.events.len() - MAX_EVENT_TRANSCRIPT;
            self.events.drain(..drop);
        }
    }

    /// Validates that every transcript entry references a system index below
    /// `system_count`. Called by the runner after decoding a snapshot
    /// (issue #37).
    pub fn validate_system_indices(&self, system_count: usize) -> Result<(), CanonicalError> {
        let count = u32::try_from(system_count)
            .map_err(|_| CanonicalError::InvalidLength(system_count as u64))?;
        let valid = self.events.iter().all(|event| event.system_index() < count)
            && self.terminated.is_none_or(|t| t.system_index() < count);
        if valid {
            Ok(())
        } else {
            Err(CanonicalError::InvalidState(
                "system event transcript references an unknown system index",
            ))
        }
    }

    /// The standard command ingress, shared by external submissions and
    /// system-emitted intents (issue #37). See
    /// [`SimulationRunner::submit_command`] for the contract.
    ///
    /// Returns the assigned [`CommandId`]. Commands targeting the current or a
    /// past tick, commands beyond the given look-ahead limit, duplicate
    /// request IDs (at or below the (actor, session) high-water mark),
    /// saturated pending queues, and unsupported envelope versions are
    /// rejected with structured errors; commands are never silently
    /// retargeted.
    pub fn submit_command_with(
        &mut self,
        limits: &CommandLimits,
        request: CommandRequest<P>,
    ) -> Result<CommandId, CommandSubmitError> {
        if request.tick <= self.tick {
            return Err(CommandSubmitError::LateTick {
                requested: request.tick,
                current: self.tick,
            });
        }
        let max_target = self.tick.saturating_add(limits.max_lookahead_ticks);
        if request.tick > max_target {
            return Err(CommandSubmitError::LookAheadExceeded {
                requested: request.tick,
                current: self.tick,
                max_lookahead_ticks: limits.max_lookahead_ticks,
            });
        }
        // Per-(actor, session) monotonic high-water mark (issues #39/#41): a
        // request ID at or below the accepted mark of the same actor+session
        // is a retry and rejected, so an old retry can never be applied twice,
        // while the dedup state stays bounded to one entry per actor+session.
        let identity = (request.actor, request.session);
        if let Some(&last) = self.last_request_id_per_session.get(&identity) {
            if request.request_id <= last {
                return Err(CommandSubmitError::DuplicateRequest {
                    request_id: request.request_id,
                });
            }
        }
        // Pending queue limits (issue #39). The queue is already bounded by
        // max_pending_total, so counting it is O(limit).
        let pending_total = self.pending_commands.values().map(Vec::len).sum::<usize>();
        let pending_actor = self
            .pending_commands
            .values()
            .flatten()
            .filter(|envelope| envelope.actor() == request.actor)
            .count();
        let to_u32 = |count: usize| u32::try_from(count).unwrap_or(u32::MAX);
        if pending_actor >= limits.max_pending_per_actor as usize {
            return Err(CommandSubmitError::ActorPendingFull {
                actor: request.actor,
                count: to_u32(pending_actor),
                limit: limits.max_pending_per_actor,
            });
        }
        if pending_total >= limits.max_pending_total as usize {
            return Err(CommandSubmitError::PendingQueueFull {
                count: to_u32(pending_total),
                limit: limits.max_pending_total,
            });
        }
        let raw_id = self.next_command_id;
        self.next_command_id = raw_id
            .checked_add(1)
            .ok_or(CommandSubmitError::IdSpaceExhausted)?;
        let command_id = CommandId(raw_id);

        let envelope = CommandEnvelope::new(
            ENVELOPE_VERSION,
            request.request_id,
            command_id,
            request.actor,
            request.session,
            request.tick,
            request.subject,
            request.payload,
        )?;
        self.last_request_id_per_session
            .insert(identity, request.request_id);
        self.pending_commands
            .entry(request.tick)
            .or_default()
            .push(envelope);
        Ok(command_id)
    }
}

/// Canonical encoding of the authoritative simulation state (ADR-0001
/// Decision §8, `RtsEngineDoc.md` §13.3, encoding contract in
/// [`crate::hash`]). Covers the encoding, schema, and configuration versions,
/// the simulation config, tick, entity
/// allocator, RNG stream states, the command bookkeeping (pending queue,
/// applied-command count and bounded transcript, per-actor request
/// high-water marks, command-ID sequence) — the pending queue is
/// authoritative input state and is hashed from submission, while the dedup
/// and transcript state stay bounded so hash cost does not grow with match
/// duration (issue #39). The transient current-tick command view is
/// excluded: it is rebuilt from the pending queue at each tick boundary, so
/// a recorded per-tick checkpoint digest equals the public digest after the
/// tick (issue #38). Ordering is stable and logical (slot index, stream
/// name, tick, command ID) and never depends on storage layout.
/// Renderer/GPU state and wall-clock data cannot enter because the
/// authoritative types do not contain them. The canonical system-event
/// transcript and termination record (issue #37) are covered too, so event
/// and failure ordering is checkpointed like every other state.
impl<P: CanonicalSerialize, E: CanonicalSerialize> CanonicalSerialize for Simulation<P, E> {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u32(CANONICAL_ENCODING_VERSION);
        sink.write_u32(SIMULATION_SCHEMA_VERSION);
        sink.write_u32(SIMULATION_CONFIG_VERSION);
        self.config.write_canonical(sink);
        sink.write_u64(self.tick);
        self.entities.write_canonical(sink);
        self.rng.write_canonical(sink);
        self.pending_commands.write_canonical(sink);
        sink.write_u64(self.applied_command_count);
        self.applied_commands.write_canonical(sink);
        self.applied_outcomes.write_canonical(sink);
        self.last_request_id_per_session.write_canonical(sink);
        sink.write_u64(self.next_command_id);
        sink.write_u64(self.emitted_event_count);
        self.events.write_canonical(sink);
        self.terminated.write_canonical(sink);
    }
}

impl<P: CanonicalDecode, E: CanonicalDecode> CanonicalDecode for Simulation<P, E> {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        let encoding_version = reader.read_u32()?;
        if encoding_version != CANONICAL_ENCODING_VERSION {
            return Err(CanonicalError::UnsupportedVersion {
                found: encoding_version,
                expected: CANONICAL_ENCODING_VERSION,
            });
        }
        let schema_version = reader.read_u32()?;
        if schema_version != SIMULATION_SCHEMA_VERSION {
            return Err(CanonicalError::UnsupportedVersion {
                found: schema_version,
                expected: SIMULATION_SCHEMA_VERSION,
            });
        }
        let config_version = reader.read_u32()?;
        if config_version != SIMULATION_CONFIG_VERSION {
            return Err(CanonicalError::UnsupportedVersion {
                found: config_version,
                expected: SIMULATION_CONFIG_VERSION,
            });
        }
        let config = SimulationConfig::read_canonical(reader)?;
        Ok(Self {
            config,
            tick: reader.read_u64()?,
            entities: CanonicalDecode::read_canonical(reader)?,
            rng: CanonicalDecode::read_canonical(reader)?,
            pending_commands: CanonicalDecode::read_canonical(reader)?,
            applied_command_count: reader.read_u64()?,
            applied_commands: CanonicalDecode::read_canonical(reader)?,
            applied_outcomes: CanonicalDecode::read_canonical(reader)?,
            current_tick_commands: 0..0,
            last_request_id_per_session: CanonicalDecode::read_canonical(reader)?,
            next_command_id: reader.read_u64()?,
            emitted_event_count: reader.read_u64()?,
            events: CanonicalDecode::read_canonical(reader)?,
            terminated: CanonicalDecode::read_canonical(reader)?,
        })
    }
}

impl<P: CanonicalSerialize, E: CanonicalSerialize> Simulation<P, E> {
    /// Produces the canonical state digest of the authoritative simulation
    /// state (ADR-0001 Decision §8). The concrete algorithm is hidden behind
    /// [`Digest`]; see [`crate::hash`] for the versioned encoding contract.
    ///
    /// This covers simulation state only; the runner's checkpoint digest
    /// additionally covers every registered system's canonical state.
    #[must_use]
    pub fn canonical_digest(&self) -> Digest {
        canonical_digest(self)
    }
}

/// Stable identity of a registered system (issue #34: system-state ownership;
/// issue #35: registration freeze and manifest). Unique within a runner;
/// used to key the system's canonical state in checkpoints and snapshots, and
/// in the replay system manifest.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SystemId(pub &'static str);

impl CanonicalSerialize for SystemId {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_bytes(self.0.as_bytes());
    }
}

/// A hook that runs inside one [`Phase`] of every tick.
///
/// # State ownership contract (issue #34, ADR-0001 Decision §8)
///
/// The preferred model is stateless systems: all authoritative mutable state
/// lives in [`Simulation`], which is what the canonical checkpoint digest and
/// snapshots cover. A system that keeps mutable state in `self` must treat it
/// as authoritative: it MUST be written by [`SimulationSystem::write_canonical_state`]
/// and restored by [`SimulationSystem::restore_canonical_state`], and any
/// configuration that can affect behavior belongs in that state too. State
/// missing from the canonical encoding is untracked: it cannot be checkpointed
/// or restored, and any behavior depending on it will diverge after a restore.
///
/// Rebuildable caches are the exception: they are allowed only when rebuilt
/// from authoritative state, excluded from the canonical encoding, and never
/// consulted for an outcome before being validated against that state.
///
/// Systems declare their phase and an explicit in-phase ordering key. The
/// runner executes phases in [`Phase::SCHEDULE`] order; registration order
/// only breaks exact (phase, order) ties (by [`SystemId`]) and is therefore
/// never what defines the schedule.
pub trait SimulationSystem<P, E = ()> {
    /// The stable, unique identity of this system.
    fn id(&self) -> SystemId;

    /// The phase this system runs in.
    fn phase(&self) -> Phase;

    /// Explicit ordering key within the phase. Required and unique per
    /// phase: together with the phase schedule this fully determines
    /// execution order, so registration order can never affect it.
    fn order(&self) -> u64;

    /// Stable diagnostic name used by tests and replay diagnostics.
    fn name(&self) -> &'static str;

    /// Downcast access for inspection/diagnostics.
    fn as_any(&self) -> &dyn std::any::Any;

    /// Mutable downcast access for setup/diagnostics.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;

    /// Writes this system's authoritative state (including behavior-relevant
    /// configuration) into the canonical stream. Stateless systems use the
    /// default, which writes nothing. This is the ONLY system state included
    /// in checkpoint digests and snapshots.
    fn write_canonical_state(&self, _sink: &mut dyn CanonicalSink) {}

    /// Restores authoritative state previously written by
    /// [`SimulationSystem::write_canonical_state`]. The default suits
    /// stateless systems: it succeeds only when no state bytes are present.
    fn restore_canonical_state(
        &mut self,
        reader: &mut CanonicalReader<'_>,
    ) -> Result<(), CanonicalError> {
        reader.finish()
    }

    /// Application-time command validation (lifecycle stage 3, issue #43).
    /// Called once per queued command at its target tick's boundary, in
    /// deterministic execution order across `Phase::ApplyCommands` systems,
    /// before that phase runs — i.e. against the authoritative world state at
    /// application time. The first [`CommandVerdict::Reject`] wins and becomes
    /// the command's canonical [`CommandOutcome`]. The default accepts.
    ///
    /// The hook also receives the authoritative [`TickContext`] (issue #37).
    fn validate_command(
        &mut self,
        _ctx: &TickContext,
        _simulation: &Simulation<P, E>,
        _envelope: &CommandEnvelope<P>,
    ) -> CommandVerdict {
        CommandVerdict::Accept
    }

    /// Executes the system for the current tick with the authoritative
    /// [`TickContext`] (issue #37): tick, tick rate, and time step come from
    /// the single context, never from values captured by the system itself.
    ///
    /// The hook returns a structured [`SystemOutput`] (typed events and
    /// internal intents, which the runner applies deterministically right
    /// after the hook) or a structured [`SystemError`]. A non-fatal
    /// [`SystemError::Event`] is recorded in the canonical system-event
    /// transcript and the tick continues; [`SystemError::Fatal`] terminates
    /// the run deterministically. Panics are not the normal error path.
    fn run(
        &mut self,
        ctx: &TickContext,
        simulation: &mut Simulation<P, E>,
        phase: Phase,
    ) -> Result<SystemOutput<P, E>, SystemError>;
}

/// Structured reasons system registration is rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SystemRegistrationError {
    /// A system with this ID is already registered; IDs must be unique so
    /// canonical state can be keyed unambiguously.
    DuplicateSystemId { id: SystemId },
    /// Another system already declared this ordering key in the same phase;
    /// ordering keys must be unique per phase so execution order is fully
    /// determined and never depends on registration order.
    DuplicateOrderKey { phase: Phase, order: u64 },
    /// The registry is frozen: registration is closed after
    /// `finalize_registry` or after the first tick has executed, so a run's
    /// behavior can never change mid-run without a versioned transition.
    RegistryFrozen,
}

impl std::fmt::Display for SystemRegistrationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateSystemId { id } => {
                write!(formatter, "duplicate system id: {}", id.0)
            }
            Self::DuplicateOrderKey { phase, order } => write!(
                formatter,
                "duplicate ordering key {order} in phase {}",
                phase.name()
            ),
            Self::RegistryFrozen => {
                formatter.write_str("system registry is frozen (finalized or past the first tick)")
            }
        }
    }
}

impl std::error::Error for SystemRegistrationError {}

/// Errors produced while restoring a runner snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnapshotError {
    /// The canonical byte stream was invalid, or a version field mismatched.
    Canonical(CanonicalError),
    /// The snapshot names a system ID that is not registered on this runner.
    UnknownSystemId(String),
    /// A registered system has no state in the snapshot.
    MissingSystemState(SystemId),
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Canonical(error) => write!(formatter, "invalid snapshot: {error}"),
            Self::UnknownSystemId(name) => {
                write!(formatter, "snapshot contains unknown system id: {name}")
            }
            Self::MissingSystemState(id) => {
                write!(formatter, "snapshot has no state for system: {}", id.0)
            }
        }
    }
}

impl std::error::Error for SnapshotError {}

/// Errors returned when advancing the simulation by one tick (issue #37).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StepError {
    /// The authoritative tick counter cannot represent another tick.
    TickOverflow(TickOverflow),
    /// A fatal system error terminated the run. The termination is recorded
    /// in canonical state, so snapshots and replays reproduce it; further
    /// ticks are refused.
    Terminated {
        /// The canonical termination record.
        termination: SystemTermination,
    },
}

impl From<TickOverflow> for StepError {
    fn from(error: TickOverflow) -> Self {
        Self::TickOverflow(error)
    }
}

impl std::fmt::Display for StepError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TickOverflow(error) => write!(formatter, "{error}"),
            Self::Terminated { termination } => write!(
                formatter,
                "run terminated by fatal system error at tick {} (system index {}, code {})",
                termination.tick(),
                termination.system_index(),
                termination.code()
            ),
        }
    }
}

impl std::error::Error for StepError {}

/// The canonical checkpoint digest over the simulation and every registered
/// system's canonical state, keyed by system ID in execution order.
fn checkpoint_digest<P: CanonicalSerialize, E: CanonicalSerialize>(
    simulation: &Simulation<P, E>,
    systems: &[RegisteredSystem<P, E>],
) -> Digest {
    let mut sink = HashSink::new(DigestAlgorithm::current());
    simulation.write_canonical(&mut sink);
    sink.write_u64(systems.len() as u64);
    for registered in systems {
        registered.system.id().write_canonical(&mut sink);
        let mut state = SnapshotSink::new();
        registered.system.write_canonical_state(&mut state);
        sink.write_bytes(&state.bytes);
    }
    sink.finish()
}

struct RegisteredSystem<P, E> {
    id: SystemId,
    phase: Phase,
    order: u64,
    system: Box<dyn SimulationSystem<P, E>>,
}

/// The fixed-tick, CPU-only, deterministic simulation loop.
///
/// Advancing the simulation is separated from any real-time application loop:
/// callers step exactly one tick with [`SimulationRunner::step_tick`] or an
/// exact count with [`SimulationRunner::advance_ticks`]. No sleeping, window,
/// or wall-clock read is involved.
///
/// The gameplay-affecting configuration (tick rate) lives in the
/// [`Simulation`]'s canonical [`SimulationConfig`] (issue #37), so it is part
/// of every checkpoint digest and is exposed to systems through
/// [`TickContext`]. `E` is the typed system-event payload (`()` for
/// event-less runs).
pub struct SimulationRunner<P, E = ()> {
    simulation: Simulation<P, E>,
    systems: Vec<RegisteredSystem<P, E>>,
    manifest: Option<SystemManifest>,
    hashes: Vec<Digest>,
    record_hashes: bool,
    command_limits: CommandLimits,
}

impl<P: CanonicalSerialize, E: CanonicalSerialize> SimulationRunner<P, E> {
    /// Creates a runner at the given fixed tick rate with the given explicit
    /// master RNG seed and no systems registered. The tick rate becomes part
    /// of the simulation's canonical configuration (issue #37). Per-tick
    /// state hash recording is enabled by default.
    #[must_use]
    pub fn new(tick_rate: TickRate, rng_seed: u64) -> Self {
        Self {
            simulation: Simulation::with_config(SimulationConfig::new(tick_rate), rng_seed),
            systems: Vec::new(),
            manifest: None,
            hashes: Vec::new(),
            record_hashes: true,
            command_limits: CommandLimits::DEFAULT,
        }
    }

    /// Returns the active command ingress limits (issue #39).
    #[must_use]
    pub const fn command_limits(&self) -> &CommandLimits {
        &self.command_limits
    }

    /// Installs new validated command ingress limits. Limits may only change
    /// before the first tick; afterwards the running configuration is locked
    /// so behavior stays deterministic.
    pub fn set_command_limits(&mut self, limits: CommandLimits) -> Result<(), CommandLimitsError> {
        limits.validate().map_err(CommandLimitsError::Invalid)?;
        if self.simulation.tick > 0 {
            return Err(CommandLimitsError::LockedAfterFirstTick);
        }
        self.command_limits = limits;
        Ok(())
    }

    /// Returns the canonical state digest recorded for each executed tick, in
    /// tick order. Empty when digest recording is disabled.
    #[must_use]
    pub fn tick_hashes(&self) -> &[Digest] {
        &self.hashes
    }

    /// Returns whether per-tick state hash recording is enabled.
    #[must_use]
    pub const fn record_hashes(&self) -> bool {
        self.record_hashes
    }

    /// Enables or disables per-tick state digest recording. Digesting is
    /// always available via [`Simulation::canonical_digest`]; recording may be
    /// disabled in release builds (`RtsEngineDoc.md` §13.3) but must stay
    /// enabled in tests and diagnostic builds.
    pub fn set_record_hashes(&mut self, enabled: bool) {
        self.record_hashes = enabled;
    }

    /// Returns the configured fixed tick rate, from the simulation's canonical
    /// configuration (issue #37).
    #[must_use]
    pub fn tick_rate(&self) -> TickRate {
        self.simulation.config().tick_rate()
    }

    /// Returns the authoritative simulation state.
    #[must_use]
    pub const fn simulation(&self) -> &Simulation<P, E> {
        &self.simulation
    }

    /// Returns mutable access to the authoritative simulation state, for
    /// setup (world construction, spawning) outside system hooks.
    #[must_use]
    pub fn simulation_mut(&mut self) -> &mut Simulation<P, E> {
        &mut self.simulation
    }

    /// Returns the registered system with the given ID, for inspection and
    /// downcast access (via [`SimulationSystem::as_any`]).
    pub fn system_mut(&mut self, id: SystemId) -> Option<&mut dyn SimulationSystem<P, E>> {
        for registered in &mut self.systems {
            if registered.id == id {
                return Some(&mut *registered.system);
            }
        }
        None
    }

    /// Registers a system. The phase schedule comes from [`Phase::SCHEDULE`]
    /// and each system's declared `(phase, order)`, not from registration
    /// order. System IDs must be unique: they key canonical system state in
    /// checkpoints and snapshots.
    pub fn register_system<S>(&mut self, system: S) -> Result<&mut Self, SystemRegistrationError>
    where
        S: SimulationSystem<P, E> + 'static,
    {
        if self.manifest.is_some() || self.simulation.tick > 0 {
            return Err(SystemRegistrationError::RegistryFrozen);
        }
        let id = system.id();
        if self.systems.iter().any(|registered| registered.id == id) {
            return Err(SystemRegistrationError::DuplicateSystemId { id });
        }
        let phase = system.phase();
        let order = system.order();
        if self
            .systems
            .iter()
            .any(|registered| registered.phase == phase && registered.order == order)
        {
            return Err(SystemRegistrationError::DuplicateOrderKey { phase, order });
        }
        let registered = RegisteredSystem {
            id,
            phase,
            order,
            system: Box::new(system),
        };
        self.systems.push(registered);
        // Stable sort: the phase schedule and the explicit ordering key
        // dominate, so execution order never depends on registration order;
        // the system ID is the final deterministic tie-break.
        self.systems
            .sort_by_key(|registered| (registered.phase.index(), registered.order, registered.id));
        Ok(self)
    }

    /// Finalizes the registry: freezes it into a [`SystemManifest`] in
    /// execution order. Registration already rejected duplicates, so this is
    /// infallible and idempotent. Also called automatically on the first
    /// tick, so registration is implicitly closed after the first tick.
    pub fn finalize_registry(&mut self) -> &SystemManifest {
        if self.manifest.is_none() {
            let entries = self
                .systems
                .iter()
                .map(|registered| SystemManifestEntry {
                    id: registered.id,
                    phase: registered.phase,
                    order: registered.order,
                })
                .collect();
            self.manifest = Some(SystemManifest { entries });
        }
        self.manifest.as_ref().expect("manifest was just set")
    }

    /// Returns the finalized system manifest, or `None` while the registry
    /// is still open (before the first tick and before
    /// [`SimulationRunner::finalize_registry`]).
    #[must_use]
    pub fn system_manifest(&self) -> Option<&SystemManifest> {
        self.manifest.as_ref()
    }

    /// Returns whether the registry has been finalized.
    #[must_use]
    pub const fn is_finalized(&self) -> bool {
        self.manifest.is_some()
    }

    /// The registered systems in deterministic execution order.
    fn ordered_systems(&self) -> &[RegisteredSystem<P, E>] {
        &self.systems
    }

    /// Produces the canonical checkpoint digest of the full authoritative
    /// state: the [`Simulation`] plus every registered system's canonical
    /// state, keyed by [`SystemId`] in execution order (issue #34). This is
    /// the digest recorded per tick at [`Phase::ProduceStateHash`].
    #[must_use]
    pub fn canonical_digest(&self) -> Digest {
        checkpoint_digest(&self.simulation, self.ordered_systems())
    }

    /// Encodes a full snapshot of the authoritative state: the simulation
    /// (encoding and schema versions included) plus each registered system's
    /// canonical state keyed by [`SystemId`]. Stateless systems contribute an
    /// empty state. Rebuildable caches are excluded by contract.
    #[must_use]
    pub fn snapshot(&self) -> Vec<u8> {
        let mut sink = SnapshotSink::new();
        self.simulation.write_canonical(&mut sink);
        sink.write_u64(self.systems.len() as u64);
        for registered in self.ordered_systems() {
            registered.system.id().write_canonical(&mut sink);
            let mut state = SnapshotSink::new();
            registered.system.write_canonical_state(&mut state);
            sink.write_bytes(&state.bytes);
        }
        sink.bytes
    }

    /// Restores a snapshot produced by [`SimulationRunner::snapshot`] into
    /// this runner. The snapshot's system IDs must match the registered
    /// systems exactly; each system's state is fed through its
    /// [`SimulationSystem::restore_canonical_state`]. The encoding and schema
    /// versions are validated and rejected on mismatch.
    pub fn restore_snapshot(&mut self, bytes: &[u8]) -> Result<(), SnapshotError>
    where
        P: CanonicalDecode,
        E: CanonicalDecode,
    {
        let mut reader = CanonicalReader::new(bytes);
        self.simulation =
            Simulation::read_canonical(&mut reader).map_err(SnapshotError::Canonical)?;
        // The system-event transcript references systems by execution index
        // (issue #37): validate the indices against this runner's registry.
        self.simulation
            .validate_system_indices(self.systems.len())
            .map_err(SnapshotError::Canonical)?;
        // Recorded per-tick digests beyond the restored tick are stale.
        self.hashes
            .truncate(usize::try_from(self.simulation.tick).unwrap_or(usize::MAX));
        let count = reader.read_u64().map_err(SnapshotError::Canonical)?;
        let count = usize::try_from(count)
            .map_err(|_| SnapshotError::Canonical(CanonicalError::InvalidLength(count)))?;
        let mut restored = vec![false; self.systems.len()];
        for _ in 0..count {
            let name_bytes = reader.read_bytes().map_err(SnapshotError::Canonical)?;
            let name = String::from_utf8(name_bytes.to_vec())
                .map_err(|_| SnapshotError::Canonical(CanonicalError::InvalidUtf8))?;
            let position = self
                .systems
                .iter()
                .position(|registered| registered.id.0 == name)
                .ok_or_else(|| SnapshotError::UnknownSystemId(name.clone()))?;
            let state_bytes = reader.read_bytes().map_err(SnapshotError::Canonical)?;
            let mut state_reader = CanonicalReader::new(state_bytes);
            self.systems[position]
                .system
                .restore_canonical_state(&mut state_reader)
                .map_err(SnapshotError::Canonical)?;
            state_reader.finish().map_err(SnapshotError::Canonical)?;
            restored[position] = true;
        }
        reader.finish().map_err(SnapshotError::Canonical)?;
        for (registered, restored) in self.systems.iter().zip(restored) {
            if !restored {
                return Err(SnapshotError::MissingSystemState(registered.id));
            }
        }
        Ok(())
    }

    /// Queues a command to be applied at the boundary of its target tick.
    ///
    /// Returns the assigned [`CommandId`]. Commands targeting the current or a
    /// past tick, commands beyond the configured look-ahead limit, duplicate
    /// request IDs (at or below the actor's high-water mark), saturated
    /// pending queues, and unsupported envelope versions are rejected with
    /// structured errors; commands are never silently retargeted.
    ///
    /// This is the standard ingress: system-emitted intents
    /// ([`SystemIntent::Command`], issue #37) go through the exact same path,
    /// with the same explicit target-tick semantics; same-tick application
    /// order is the arbitration key (issue #42), not submission order.
    pub fn submit_command(
        &mut self,
        request: CommandRequest<P>,
    ) -> Result<CommandId, CommandSubmitError> {
        self.simulation
            .submit_command_with(&self.command_limits, request)
    }

    /// Advances the simulation by exactly one tick: increments the
    /// authoritative tick counter, drains the commands assigned to that tick
    /// and arbitrates them into the stable key order (issue #42), then executes every phase in
    /// [`Phase::SCHEDULE`] order. Every hook receives the single authoritative
    /// [`TickContext`] for this tick (issue #37). Never reads wall-clock time.
    ///
    /// System outputs are applied deterministically right after each hook:
    /// events land in the canonical system-event transcript, intents go
    /// through the standard command ingress. A non-fatal
    /// [`SystemError::Event`] is recorded as a transcript failure and the tick
    /// continues; a [`SystemError::Fatal`] terminates the run (recorded
    /// canonically) and this returns [`StepError::Terminated`]. The
    /// terminating tick's checkpoint digest is still recorded, so replays
    /// compare it like any other tick.
    pub fn step_tick(&mut self) -> Result<(), StepError> {
        if let Some(termination) = self.simulation.terminated {
            return Err(StepError::Terminated { termination });
        }
        // One-way lifecycle: the first tick implicitly finalizes the system
        // registry and the RNG stream registry (issue #47), so neither can
        // change mid-run.
        self.finalize_registry();
        self.simulation.rng.finalize();
        let next_tick = self
            .simulation
            .tick
            .checked_add(1)
            .ok_or(StepError::TickOverflow(TickOverflow))?;
        self.simulation.tick = next_tick;
        // The single authoritative context handed to every system hook this
        // tick (issue #37): tick + time-step configuration from the canonical
        // simulation config.
        let ctx = TickContext::new(next_tick, self.simulation.config().tick_rate());
        let mut commands = self
            .simulation
            .pending_commands
            .remove(&next_tick)
            .unwrap_or_default();
        // Deterministic same-tick arbitration (issue #42): order by the
        // stable arbitration key (actor, session, request ID, command ID),
        // never by arrival order, so the same command set arbitrates
        // identically regardless of which producer reached the runner first.
        // Validation, application, and the transcript all follow this order.
        // The drained commands join the durable applied transcript; the
        // current-tick view is just a range into it.
        commands.sort_by_key(|envelope| envelope.arbitration_key());
        // Lifecycle stages 3-4 (issue #43): application-time validation against
        // the authoritative state at the tick boundary, in deterministic
        // execution order across ApplyCommands systems; the first rejection
        // wins and is recorded as the command's canonical outcome.
        let mut outcomes: Vec<(CommandId, CommandOutcome)> = Vec::with_capacity(commands.len());
        for envelope in &commands {
            let mut outcome = CommandOutcome::Applied;
            for registered in &mut self.systems {
                if registered.phase == Phase::ApplyCommands {
                    if let CommandVerdict::Reject { reason } =
                        registered
                            .system
                            .validate_command(&ctx, &self.simulation, envelope)
                    {
                        outcome = CommandOutcome::Rejected { reason };
                        break;
                    }
                }
            }
            outcomes.push((envelope.command_id(), outcome));
        }
        let start = self.simulation.applied_commands.len();
        self.simulation.applied_commands.append(&mut commands);
        self.simulation.applied_outcomes.extend(outcomes);
        self.simulation.applied_command_count +=
            (self.simulation.applied_commands.len() - start) as u64;
        self.simulation.current_tick_commands = start..self.simulation.applied_commands.len();

        let SimulationRunner {
            simulation,
            systems,
            hashes,
            record_hashes,
            command_limits,
            ..
        } = self;
        let mut termination: Option<SystemTermination> = None;
        'phases: for phase in Phase::SCHEDULE {
            for (system_index, registered) in systems.iter_mut().enumerate() {
                if registered.phase != phase {
                    continue;
                }
                let system_index = u32::try_from(system_index).unwrap_or(u32::MAX);
                match registered.system.run(&ctx, simulation, phase) {
                    Ok(output) => {
                        // Structured outputs are applied right after the hook,
                        // in emission order (issue #37): events into the
                        // canonical transcript, intents through the standard
                        // ingress.
                        let (events, intents) = output.into_parts();
                        if !events.is_empty() {
                            simulation.record_events(ctx.tick(), system_index, events);
                        }
                        for intent in intents {
                            let SystemIntent::Command(request) = intent;
                            if simulation
                                .submit_command_with(command_limits, request)
                                .is_err()
                            {
                                // An intent that ingress rejects violates
                                // the ingress contract (e.g. a target tick
                                // at or before the current tick):
                                // terminate deterministically instead of
                                // silently dropping the intent.
                                simulation.terminate(
                                    ctx.tick(),
                                    system_index,
                                    SYSTEM_ERROR_INTENT_REJECTED,
                                );
                                termination = simulation.terminated;
                                break 'phases;
                            }
                        }
                    }
                    Err(error) => {
                        simulation.record_failure(ctx.tick(), system_index, error.code());
                        if error.is_fatal() {
                            simulation.terminated = Some(SystemTermination::new(
                                ctx.tick(),
                                system_index,
                                error.code(),
                            ));
                            termination = simulation.terminated;
                            break 'phases;
                        }
                    }
                }
            }
        }

        // Phase 15 (produce state hash): checkpoint digest after all phases
        // ran. The transient current-tick command view is not canonical state,
        // so this digest equals the public checkpoint digest once step_tick
        // returns, even on ticks that applied commands. It covers the
        // simulation and every registered system's canonical state.
        if *record_hashes {
            hashes.push(checkpoint_digest(simulation, systems));
        }
        simulation.current_tick_commands = 0..0;
        // Bound the in-state applied transcript (issue #39): keep only the
        // most recent `max_applied_history` entries; the total count keeps
        // the full number. This keeps canonical hash cost independent of
        // match duration. The system-event transcript is bounded the same way
        // (issue #37).
        let cap = usize::try_from(command_limits.max_applied_history).unwrap_or(usize::MAX);
        if simulation.applied_commands.len() > cap {
            let drop = simulation.applied_commands.len() - cap;
            simulation.applied_commands.drain(..drop);
            simulation.applied_outcomes.drain(..drop);
        }
        simulation.trim_events();
        if let Some(termination) = termination {
            return Err(StepError::Terminated { termination });
        }
        Ok(())
    }

    /// Advances the simulation by exactly `count` ticks. This is the
    /// deterministic stepping API used by tests and replay tools.
    pub fn advance_ticks(&mut self, count: u64) -> Result<(), StepError> {
        for _ in 0..count {
            self.step_tick()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::command::{
        ActorId, CommandId, CommandLimits, CommandLimitsError, CommandOutcome, CommandVerdict,
        InvalidCommandLimits, RequestId, SessionId,
    };
    use crate::context::SystemEventKind;
    use crate::hash::canonical_encode;

    type Trace = Rc<RefCell<Vec<(u64, Phase, &'static str)>>>;
    type IngestCounts = Rc<RefCell<Vec<(u64, usize)>>>;
    type CommandLog = Rc<RefCell<Vec<(u64, Vec<u32>)>>>;
    type WorldLog = Rc<RefCell<Vec<(u64, u32, u64, u64)>>>;

    const SEED: u64 = 0x5EED;

    struct Probe {
        phase: Phase,
        order: u64,
        name: &'static str,
        trace: Trace,
        ingest_counts: IngestCounts,
    }

    impl<P> SimulationSystem<P> for Probe {
        fn id(&self) -> SystemId {
            SystemId(self.name)
        }

        fn phase(&self) -> Phase {
            self.phase
        }

        fn order(&self) -> u64 {
            self.order
        }

        fn name(&self) -> &'static str {
            self.name
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }

        fn run(
            &mut self,
            _ctx: &TickContext,
            simulation: &mut Simulation<P>,
            phase: Phase,
        ) -> Result<SystemOutput<P>, SystemError> {
            self.trace
                .borrow_mut()
                .push((simulation.tick(), phase, self.name));
            if phase == Phase::IngestCommands {
                let count = simulation.commands_for_current_tick().len();
                self.ingest_counts
                    .borrow_mut()
                    .push((simulation.tick(), count));
            }
            Ok(SystemOutput::default())
        }
    }

    /// Registers probes deliberately out of schedule order to prove that
    /// registration order does not define execution order.
    fn probe_runner_at(
        tick_rate: TickRate,
        trace: &Trace,
        ingest_counts: &IngestCounts,
    ) -> SimulationRunner<u32> {
        let mut runner = SimulationRunner::new(tick_rate, SEED);
        runner
            .register_system(Probe {
                phase: Phase::ProduceStateHash,
                order: 0,
                name: "hash",
                trace: trace.clone(),
                ingest_counts: ingest_counts.clone(),
            })
            .expect("unique system id");
        runner
            .register_system(Probe {
                phase: Phase::ResolveCombat,
                order: 0,
                name: "combat",
                trace: trace.clone(),
                ingest_counts: ingest_counts.clone(),
            })
            .expect("unique system id");
        runner
            .register_system(Probe {
                phase: Phase::IngestCommands,
                order: 1,
                name: "ingest-b",
                trace: trace.clone(),
                ingest_counts: ingest_counts.clone(),
            })
            .expect("unique system id");
        runner
            .register_system(Probe {
                phase: Phase::IngestCommands,
                order: 0,
                name: "ingest-a",
                trace: trace.clone(),
                ingest_counts: ingest_counts.clone(),
            })
            .expect("unique system id");
        runner
    }

    fn probe_runner(trace: &Trace, ingest_counts: &IngestCounts) -> SimulationRunner<u32> {
        probe_runner_at(TickRate::DEFAULT, trace, ingest_counts)
    }

    fn request(request_id: u64, tick: u64, payload: u32) -> CommandRequest<u32> {
        request_from(0, request_id, tick, payload)
    }

    fn request_from(actor: u64, request_id: u64, tick: u64, payload: u32) -> CommandRequest<u32> {
        request_from_session(actor, 0, request_id, tick, payload)
    }

    fn request_from_session(
        actor: u64,
        session: u64,
        request_id: u64,
        tick: u64,
        payload: u32,
    ) -> CommandRequest<u32> {
        CommandRequest {
            request_id: RequestId(request_id),
            actor: ActorId(actor),
            session: SessionId(session),
            tick,
            subject: None,
            payload,
        }
    }

    #[test]
    fn advances_an_exact_tick_count_without_sleeping_or_a_window() {
        let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);

        runner.advance_ticks(0).expect("zero ticks is valid");
        assert_eq!(runner.simulation().tick(), 0);

        runner.advance_ticks(10).expect("advance should succeed");
        assert_eq!(runner.simulation().tick(), 10);

        runner.step_tick().expect("single step should succeed");
        assert_eq!(runner.simulation().tick(), 11);
    }

    #[test]
    fn phase_schedule_is_explicit_complete_and_stable() {
        assert_eq!(Phase::count(), 15);
        for (position, phase) in Phase::SCHEDULE.iter().enumerate() {
            assert_eq!(phase.index(), position);
        }

        let mut names: Vec<&str> = Phase::SCHEDULE.iter().map(|phase| phase.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Phase::count());

        assert_eq!(Phase::IngestCommands.name(), "ingest_commands");
        assert_eq!(Phase::ProduceStateHash.name(), "produce_state_hash");
    }

    #[test]
    fn same_state_and_inputs_execute_the_same_phase_sequence() {
        let trace_a = Trace::default();
        let trace_b = Trace::default();
        let counts_a = IngestCounts::default();
        let counts_b = IngestCounts::default();
        let mut first = probe_runner(&trace_a, &counts_a);
        let mut second = probe_runner(&trace_b, &counts_b);

        for runner in [&mut first, &mut second] {
            runner
                .submit_command(request(1, 2, 7))
                .expect("tick 2 is in the future");
            runner
                .submit_command(request(2, 3, 8))
                .expect("tick 3 is in the future");
        }
        first.advance_ticks(4).expect("advance should succeed");
        second.advance_ticks(4).expect("advance should succeed");

        assert_eq!(first.simulation().tick(), second.simulation().tick());
        assert_eq!(*trace_a.borrow(), *trace_b.borrow());
        assert_eq!(*counts_a.borrow(), *counts_b.borrow());

        let trace = trace_a.borrow();
        for tick in 1..=4_u64 {
            let executed: Vec<Phase> = trace
                .iter()
                .filter(|(t, _, _)| *t == tick)
                .map(|(_, phase, _)| *phase)
                .collect();
            // Two probes are registered in IngestCommands, one each in
            // ResolveCombat and ProduceStateHash; schedule order dominates.
            let expected: Vec<Phase> = vec![
                Phase::IngestCommands,
                Phase::IngestCommands,
                Phase::ResolveCombat,
                Phase::ProduceStateHash,
            ];
            assert_eq!(executed, expected, "tick {tick} must follow the schedule");
        }

        let ingest_names: Vec<&str> = trace
            .iter()
            .filter(|(tick, phase, _)| *tick == 1 && *phase == Phase::IngestCommands)
            .map(|(_, _, name)| *name)
            .collect();
        assert_eq!(ingest_names, ["ingest-a", "ingest-b"]);
    }

    #[test]
    fn commands_are_applied_only_at_their_tick_boundary() {
        let counts = IngestCounts::default();
        let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        runner
            .register_system(Probe {
                phase: Phase::IngestCommands,
                order: 0,
                name: "ingest",
                trace: Trace::default(),
                ingest_counts: counts.clone(),
            })
            .expect("unique system id");

        runner
            .submit_command(request(1, 3, 42))
            .expect("tick 3 is in the future");
        runner.advance_ticks(2).expect("advance to tick 2");
        assert_eq!(*counts.borrow(), vec![(1, 0), (2, 0)]);
        assert_eq!(
            runner
                .simulation()
                .pending_command_ticks()
                .copied()
                .collect::<Vec<_>>(),
            vec![3]
        );

        runner.advance_ticks(1).expect("advance to tick 3");
        assert_eq!(*counts.borrow(), vec![(1, 0), (2, 0), (3, 1)]);
        assert!(runner.simulation().commands_for_current_tick().is_empty());
        assert_eq!(runner.simulation().pending_command_ticks().count(), 0);
    }

    #[test]
    fn same_tick_commands_apply_in_arbitration_key_order() {
        struct CommandProbe {
            log: CommandLog,
        }

        impl SimulationSystem<u32> for CommandProbe {
            fn id(&self) -> SystemId {
                SystemId("command-probe")
            }

            fn phase(&self) -> Phase {
                Phase::IngestCommands
            }

            fn order(&self) -> u64 {
                0
            }

            fn name(&self) -> &'static str {
                "command-probe"
            }

            fn as_any(&self) -> &dyn std::any::Any {
                self
            }

            fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
                self
            }

            fn run(
                &mut self,
                _ctx: &TickContext,
                simulation: &mut Simulation<u32>,
                _phase: Phase,
            ) -> Result<SystemOutput<u32>, SystemError> {
                self.log.borrow_mut().push((
                    simulation.tick(),
                    simulation
                        .commands_for_current_tick()
                        .iter()
                        .map(|envelope| envelope.payload())
                        .copied()
                        .collect(),
                ));
                Ok(SystemOutput::default())
            }
        }

        let log = CommandLog::default();
        let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        runner
            .register_system(CommandProbe { log: log.clone() })
            .expect("unique system id");

        // Assigned command IDs follow submission order: 0, 1, 2, 3.
        let id0 = runner
            .submit_command(request(1, 2, 10))
            .expect("future tick");
        let id1 = runner.submit_command(request(2, 2, 20)).expect("same tick");
        let id2 = runner
            .submit_command(request(3, 4, 30))
            .expect("later tick");
        let id3 = runner.submit_command(request(4, 2, 40)).expect("same tick");
        assert_eq!(
            (id0, id1, id2, id3),
            (CommandId(0), CommandId(1), CommandId(2), CommandId(3))
        );

        runner.advance_ticks(4).expect("advance should succeed");

        assert_eq!(
            *log.borrow(),
            vec![
                (1, vec![]),
                (2, vec![10, 20, 40]),
                (3, vec![]),
                (4, vec![30])
            ]
        );
    }

    #[test]
    fn late_commands_are_rejected_with_structured_errors() {
        let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);

        runner
            .submit_command(request(1, 1, 1))
            .expect("tick 1 is future at tick 0");
        runner.advance_ticks(1).expect("now at tick 1");

        assert_eq!(
            runner.submit_command(request(2, 1, 2)),
            Err(CommandSubmitError::LateTick {
                requested: 1,
                current: 1
            })
        );
        assert_eq!(
            runner.submit_command(request(3, 0, 3)),
            Err(CommandSubmitError::LateTick {
                requested: 0,
                current: 1
            })
        );
        runner
            .submit_command(request(4, 2, 4))
            .expect("future tick accepted");
    }

    #[test]
    fn duplicate_requests_are_rejected_not_applied_twice() {
        let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);

        runner
            .submit_command(request(7, 2, 1))
            .expect("first submission accepted");
        assert_eq!(
            runner.submit_command(request(7, 3, 2)),
            Err(CommandSubmitError::DuplicateRequest {
                request_id: RequestId(7)
            })
        );
        // The rejected duplicate did not add another queued command.
        assert_eq!(
            runner
                .simulation()
                .pending_command_ticks()
                .copied()
                .collect::<Vec<_>>(),
            vec![2]
        );

        // Even after the original command was applied, the request ID stays
        // reserved.
        runner.advance_ticks(2).expect("advance to tick 2");
        assert_eq!(
            runner.submit_command(request(7, 3, 2)),
            Err(CommandSubmitError::DuplicateRequest {
                request_id: RequestId(7)
            })
        );
        assert_eq!(runner.simulation().pending_command_ticks().count(), 0);
    }

    #[test]
    fn command_id_space_exhaustion_is_an_error() {
        let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        runner.simulation.next_command_id = u64::MAX;
        assert_eq!(
            runner.submit_command(request(1, 1, 1)),
            Err(CommandSubmitError::IdSpaceExhausted)
        );
    }

    #[test]
    fn tick_overflow_is_reported_not_wrapped() {
        let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        runner.simulation.tick = u64::MAX;

        assert_eq!(
            runner.step_tick(),
            Err(StepError::TickOverflow(TickOverflow))
        );
        assert_eq!(runner.simulation().tick(), u64::MAX);
    }

    #[test]
    fn tick_rate_is_configurable_and_decoupled_from_deterministic_stepping() {
        assert_eq!(TickRate::DEFAULT.ticks_per_second(), 20);
        assert_eq!(TickRate::new(0), Err(InvalidTickRate));
        assert_eq!(
            TickRate::new(10)
                .expect("10 Hz is valid")
                .ticks_per_second(),
            10
        );

        let trace_slow = Trace::default();
        let trace_fast = Trace::default();
        let counts_slow = IngestCounts::default();
        let counts_fast = IngestCounts::default();
        let mut slow = probe_runner_at(TickRate::new(10).unwrap(), &trace_slow, &counts_slow);
        let mut fast = probe_runner_at(TickRate::new(60).unwrap(), &trace_fast, &counts_fast);

        slow.advance_ticks(40).expect("advance should succeed");
        fast.advance_ticks(40).expect("advance should succeed");

        assert_eq!(slow.simulation().tick(), 40);
        assert_eq!(fast.simulation().tick(), 40);
        assert_eq!(*trace_slow.borrow(), *trace_fast.borrow());
        assert_eq!(*counts_slow.borrow(), *counts_fast.borrow());
    }

    #[test]
    fn simulation_exposes_authoritative_entity_and_rng_primitives() {
        struct WorldProbe {
            log: WorldLog,
        }

        impl SimulationSystem<()> for WorldProbe {
            fn id(&self) -> SystemId {
                SystemId("world-probe")
            }

            fn phase(&self) -> Phase {
                Phase::ApplyCommands
            }

            fn order(&self) -> u64 {
                0
            }

            fn name(&self) -> &'static str {
                "world-probe"
            }

            fn as_any(&self) -> &dyn std::any::Any {
                self
            }

            fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
                self
            }

            fn run(
                &mut self,
                _ctx: &TickContext,
                simulation: &mut Simulation<()>,
                _phase: Phase,
            ) -> Result<SystemOutput<()>, SystemError> {
                let entity = simulation
                    .entities_mut()
                    .allocate()
                    .expect("allocate entity");
                let draw = simulation
                    .rng_mut()
                    .stream("combat")
                    .expect("declared at setup")
                    .next_u64();
                self.log.borrow_mut().push((
                    simulation.tick(),
                    entity.index(),
                    entity.generation(),
                    draw,
                ));
                Ok(SystemOutput::default())
            }
        }

        let log_a = Rc::new(RefCell::new(Vec::new()));
        let log_b = Rc::new(RefCell::new(Vec::new()));
        let mut a: SimulationRunner<()> = SimulationRunner::new(TickRate::DEFAULT, 42);
        let mut b: SimulationRunner<()> = SimulationRunner::new(TickRate::DEFAULT, 42);
        a.register_system(WorldProbe { log: log_a.clone() })
            .expect("unique system id");
        b.register_system(WorldProbe { log: log_b.clone() })
            .expect("unique system id");
        a.simulation_mut()
            .rng_mut()
            .declare("combat")
            .expect("declare stream at setup");
        b.simulation_mut()
            .rng_mut()
            .declare("combat")
            .expect("declare stream at setup");

        a.advance_ticks(3).expect("advance");
        b.advance_ticks(3).expect("advance");

        // Same seed and inputs: identical entity IDs and RNG draws.
        assert_eq!(*log_a.borrow(), *log_b.borrow());
        assert_eq!(log_a.borrow().len(), 3);
        let first = log_a.borrow()[0];
        assert!(
            a.simulation()
                .entities()
                .contains(crate::entity::EntityId::from_raw_parts(first.1, first.2))
        );
    }

    #[test]
    fn defaults_match_the_documented_contract() {
        assert_eq!(TickRate::default(), TickRate::DEFAULT);
        assert_eq!(
            SimulationRunner::<u32>::new(TickRate::DEFAULT, SEED).tick_rate(),
            TickRate::DEFAULT
        );

        let simulation = Simulation::<u32>::new(SEED);
        assert_eq!(simulation.tick(), 0);
        assert!(simulation.commands_for_current_tick().is_empty());
        assert_eq!(simulation.pending_command_ticks().count(), 0);
        assert_eq!(simulation.rng().master_seed(), SEED);
        assert_eq!(simulation.entities().alive_count(), 0);
    }

    /// A stateful system: keeps authoritative mutable state in `self`, which
    /// must flow through the canonical state hooks.
    struct CounterSystem {
        count: u64,
    }

    impl CounterSystem {
        fn new() -> Self {
            Self { count: 0 }
        }
    }

    impl SimulationSystem<u32> for CounterSystem {
        fn id(&self) -> SystemId {
            SystemId("counter")
        }

        fn phase(&self) -> Phase {
            Phase::ApplyCommands
        }

        fn order(&self) -> u64 {
            0
        }

        fn name(&self) -> &'static str {
            "counter"
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }

        fn write_canonical_state(&self, sink: &mut dyn CanonicalSink) {
            sink.write_u64(self.count);
        }

        fn restore_canonical_state(
            &mut self,
            reader: &mut CanonicalReader<'_>,
        ) -> Result<(), CanonicalError> {
            self.count = reader.read_u64()?;
            reader.finish()
        }

        fn run(
            &mut self,
            _ctx: &TickContext,
            simulation: &mut Simulation<u32>,
            _phase: Phase,
        ) -> Result<SystemOutput<u32>, SystemError> {
            self.count += simulation.commands_for_current_tick().len() as u64 + 1;
            Ok(SystemOutput::default())
        }
    }

    #[test]
    fn duplicate_system_ids_are_rejected() {
        let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        runner
            .register_system(CounterSystem::new())
            .expect("first registration");
        assert_eq!(
            runner.register_system(CounterSystem::new()).map(|_| ()),
            Err(SystemRegistrationError::DuplicateSystemId {
                id: SystemId("counter")
            })
        );
    }

    #[test]
    fn system_state_is_part_of_the_checkpoint_digest() {
        let mut a: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        let mut b: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        a.register_system(CounterSystem::new()).expect("id");
        b.register_system(CounterSystem::new()).expect("id");
        for runner in [&mut a, &mut b] {
            runner
                .submit_command(request(1, 2, 5))
                .expect("future tick");
        }
        a.advance_ticks(3).expect("advance");
        b.advance_ticks(3).expect("advance");

        // Same inputs: identical checkpoint digests, and the last recorded
        // per-tick digest is the checkpoint digest.
        assert_eq!(a.canonical_digest(), b.canonical_digest());
        assert_eq!(a.tick_hashes().last(), Some(&a.canonical_digest()));
        // The system state is included: the checkpoint digest differs from
        // the simulation-only digest once the system has state.
        assert_ne!(a.canonical_digest(), a.simulation().canonical_digest());
    }

    #[test]
    fn recorded_tick_hash_equals_public_checkpoint_digest_on_command_ticks() {
        let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        runner
            .register_system(Noop {
                id: "noop",
                phase: Phase::ApplyCommands,
                order: 0,
            })
            .expect("id");
        runner.submit_command(request(1, 1, 10)).expect("tick 1");
        runner.submit_command(request(2, 3, 30)).expect("tick 3");
        for expected_tick in 1..=3_u64 {
            runner.step_tick().expect("tick");
            assert_eq!(runner.simulation().tick(), expected_tick);
            assert_eq!(
                runner.tick_hashes().last(),
                Some(&runner.canonical_digest()),
                "recorded per-tick digest equals the public checkpoint digest after tick {expected_tick}"
            );
        }
        // The applied-command transcript is durable, explicit canonical state.
        let applied = runner.simulation().applied_commands();
        assert_eq!(applied.len(), 2);
        assert_eq!(applied[0].command_id(), CommandId(0));
        assert_eq!(applied[0].tick(), 1);
        assert_eq!(applied[1].command_id(), CommandId(1));
        assert_eq!(applied[1].tick(), 3);
    }

    #[test]
    fn transient_current_tick_view_is_not_part_of_the_canonical_digest() {
        let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        runner.submit_command(request(1, 1, 10)).expect("tick 1");
        runner.step_tick().expect("tick");
        let digest = runner.simulation().canonical_digest();
        // Populating or clearing the transient view must not change the
        // canonical digest.
        runner.simulation_mut().current_tick_commands = 0..1;
        assert_eq!(runner.simulation().commands_for_current_tick().len(), 1);
        assert_eq!(runner.simulation().canonical_digest(), digest);
        runner.simulation_mut().current_tick_commands = 0..0;
        assert_eq!(runner.simulation().canonical_digest(), digest);
    }

    #[test]
    fn lookahead_limit_rejects_distant_future_commands() {
        let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        let limits = CommandLimits {
            max_lookahead_ticks: 10,
            ..CommandLimits::DEFAULT
        };
        runner.set_command_limits(limits).expect("valid limits");
        assert_eq!(runner.command_limits(), &limits);

        runner
            .submit_command(request(1, 10, 1))
            .expect("within lookahead");
        assert_eq!(
            runner.submit_command(request(2, 11, 2)),
            Err(CommandSubmitError::LookAheadExceeded {
                requested: 11,
                current: 0,
                max_lookahead_ticks: 10
            })
        );
        runner.advance_ticks(5).expect("advance to tick 5");
        runner
            .submit_command(request(3, 15, 3))
            .expect("within lookahead at tick 5");
        assert_eq!(
            runner.submit_command(request(4, 16, 4)),
            Err(CommandSubmitError::LookAheadExceeded {
                requested: 16,
                current: 5,
                max_lookahead_ticks: 10
            })
        );
    }

    #[test]
    fn pending_queue_limits_are_enforced_per_actor_and_in_total() {
        let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        runner
            .set_command_limits(CommandLimits {
                max_lookahead_ticks: 100,
                max_pending_per_actor: 2,
                max_pending_total: 3,
                max_applied_history: 8,
            })
            .expect("valid limits");

        runner
            .submit_command(request_from(0, 1, 50, 1))
            .expect("actor 0 first");
        runner
            .submit_command(request_from(0, 2, 51, 2))
            .expect("actor 0 second");
        assert_eq!(
            runner.submit_command(request_from(0, 3, 52, 3)),
            Err(CommandSubmitError::ActorPendingFull {
                actor: ActorId(0),
                count: 2,
                limit: 2
            })
        );
        // A different actor may reuse the same local request ID (actor-scoped
        // dedup) and still has queue room until the total limit.
        runner
            .submit_command(request_from(1, 1, 50, 4))
            .expect("actor 1 reuses local request id 1");
        assert_eq!(
            runner.submit_command(request_from(1, 2, 51, 5)),
            Err(CommandSubmitError::PendingQueueFull { count: 3, limit: 3 })
        );
    }

    #[test]
    fn duplicate_retries_are_rejected_even_after_many_newer_requests() {
        let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        runner.submit_command(request(5, 10, 1)).expect("first");
        for id in 6..=8_u64 {
            runner
                .submit_command(request(id, 10, 1))
                .expect("newer request");
        }
        assert_eq!(
            runner.submit_command(request(5, 11, 2)),
            Err(CommandSubmitError::DuplicateRequest {
                request_id: RequestId(5)
            })
        );
        // The high-water mark is permanent: a retry is still rejected after
        // the original was applied — no retention boundary can re-admit it.
        runner.advance_ticks(10).expect("advance past tick 10");
        assert_eq!(
            runner.submit_command(request(5, 20, 3)),
            Err(CommandSubmitError::DuplicateRequest {
                request_id: RequestId(5)
            })
        );
        assert_eq!(runner.simulation().applied_command_count(), 4);
    }

    #[test]
    fn request_identity_is_scoped_to_actor_and_session() {
        let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        // Independent actors may reuse the same local request ID.
        runner
            .submit_command(request_from_session(0, 0, 1, 10, 1))
            .expect("actor 0 session 0");
        runner
            .submit_command(request_from_session(1, 0, 1, 10, 2))
            .expect("actor 1 session 0 reuses request id 1");
        // The same actor in a NEW session may also reuse it.
        runner
            .submit_command(request_from_session(0, 1, 1, 10, 3))
            .expect("actor 0 session 1 reuses request id 1");
        // A retry within the same actor+session is still a duplicate.
        assert_eq!(
            runner.submit_command(request_from_session(0, 0, 1, 11, 4)),
            Err(CommandSubmitError::DuplicateRequest {
                request_id: RequestId(1)
            })
        );
        assert_eq!(
            runner.submit_command(request_from_session(0, 1, 1, 11, 5)),
            Err(CommandSubmitError::DuplicateRequest {
                request_id: RequestId(1)
            })
        );
        assert_eq!(
            runner.submit_command(request_from_session(1, 0, 1, 11, 6)),
            Err(CommandSubmitError::DuplicateRequest {
                request_id: RequestId(1)
            })
        );
        // But a higher request ID in the same actor+session is accepted.
        runner
            .submit_command(request_from_session(0, 0, 2, 11, 7))
            .expect("next local request id in actor 0 session 0");
        assert_eq!(
            runner
                .simulation()
                .pending_commands
                .values()
                .flatten()
                .count(),
            4
        );
    }

    #[test]
    fn applied_transcript_is_bounded_but_counted_and_hash_input_stays_constant() {
        let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        runner
            .set_command_limits(CommandLimits {
                max_lookahead_ticks: 100,
                max_pending_per_actor: 16,
                max_pending_total: 16,
                max_applied_history: 4,
            })
            .expect("valid limits");
        for tick in 1..=6_u64 {
            runner
                .submit_command(request(tick, tick, tick as u32))
                .expect("submit");
        }
        runner.advance_ticks(6).expect("6 ticks");
        let encoded_at_6 = canonical_encode(runner.simulation()).len();
        assert_eq!(runner.simulation().applied_command_count(), 6);
        assert_eq!(
            runner.simulation().applied_commands().len(),
            4,
            "transcript window is capped"
        );
        assert_eq!(
            runner.simulation().applied_commands()[0].tick(),
            3,
            "oldest entries are dropped first"
        );
        for tick in 7..=12_u64 {
            runner
                .submit_command(request(tick, tick, tick as u32))
                .expect("submit");
        }
        runner.advance_ticks(6).expect("6 more ticks");
        assert_eq!(
            canonical_encode(runner.simulation()).len(),
            encoded_at_6,
            "canonical input size does not grow with match duration"
        );
        assert_eq!(runner.simulation().applied_command_count(), 12);
        assert_eq!(runner.simulation().applied_commands().len(), 4);
    }

    struct RejectOn42;

    impl SimulationSystem<u32> for RejectOn42 {
        fn id(&self) -> SystemId {
            SystemId("reject-42")
        }

        fn phase(&self) -> Phase {
            Phase::ApplyCommands
        }

        fn order(&self) -> u64 {
            0
        }

        fn name(&self) -> &'static str {
            "reject-42"
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
            _simulation: &Simulation<u32>,
            envelope: &CommandEnvelope<u32>,
        ) -> CommandVerdict {
            if envelope.payload() == &42 {
                CommandVerdict::Reject { reason: 7 }
            } else {
                CommandVerdict::Accept
            }
        }

        fn run(
            &mut self,
            _ctx: &TickContext,
            _simulation: &mut Simulation<u32>,
            _phase: Phase,
        ) -> Result<SystemOutput<u32>, SystemError> {
            Ok(SystemOutput::default())
        }
    }

    #[test]
    fn application_time_rejection_is_recorded_in_the_canonical_transcript() {
        let make = || {
            let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
            runner.register_system(RejectOn42).expect("id");
            runner
                .submit_command(request(1, 2, 1))
                .expect("accepted payload");
            runner
                .submit_command(request(2, 2, 42))
                .expect("structurally valid");
            runner
                .submit_command(request(3, 2, 3))
                .expect("accepted payload");
            runner
        };
        let mut a = make();
        let mut b = make();
        a.advance_ticks(2).expect("advance");
        b.advance_ticks(2).expect("advance");

        let outcomes = a.simulation().applied_outcomes();
        assert_eq!(outcomes.len(), 3);
        assert_eq!(outcomes[0], (CommandId(0), CommandOutcome::Applied));
        assert_eq!(
            outcomes[1],
            (CommandId(1), CommandOutcome::Rejected { reason: 7 })
        );
        assert_eq!(outcomes[2], (CommandId(2), CommandOutcome::Applied));
        assert_eq!(
            a.simulation().command_outcome(CommandId(1)),
            Some(CommandOutcome::Rejected { reason: 7 })
        );
        // Outcomes are canonical state: identical runs produce identical
        // hashes and outcomes.
        assert_eq!(a.tick_hashes(), b.tick_hashes());
        assert_eq!(a.canonical_digest(), b.canonical_digest());

        // Structural (ingress) rejections never enter the transcript.
        assert_eq!(
            a.submit_command(request(4, 2, 9)),
            Err(CommandSubmitError::LateTick {
                requested: 2,
                current: 2
            })
        );
        a.step_tick().expect("tick 3");
        assert_eq!(a.simulation().applied_command_count(), 3);
        assert_eq!(a.simulation().applied_outcomes().len(), 3);
    }

    #[test]
    fn invalid_or_locked_command_limits_are_rejected() {
        let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        assert_eq!(
            runner.set_command_limits(CommandLimits {
                max_lookahead_ticks: 0,
                ..CommandLimits::DEFAULT
            }),
            Err(CommandLimitsError::Invalid(
                InvalidCommandLimits::LookAheadOutOfRange {
                    found: 0,
                    min: 1,
                    max: CommandLimits::MAX_LOOKAHEAD_TICKS
                }
            ))
        );
        assert_eq!(
            runner.set_command_limits(CommandLimits {
                max_pending_per_actor: 10,
                max_pending_total: 5,
                ..CommandLimits::DEFAULT
            }),
            Err(CommandLimitsError::Invalid(
                InvalidCommandLimits::PendingTotalBelowPerActor {
                    per_actor: 10,
                    total: 5
                }
            ))
        );
        runner
            .set_command_limits(CommandLimits::DEFAULT)
            .expect("defaults are valid");
        runner.step_tick().expect("first tick");
        assert_eq!(
            runner.set_command_limits(CommandLimits {
                max_lookahead_ticks: 5,
                ..CommandLimits::DEFAULT
            }),
            Err(CommandLimitsError::LockedAfterFirstTick)
        );
    }

    #[test]
    fn restored_snapshot_reproduces_subsequent_hashes() {
        let make_runner = || {
            let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
            runner.register_system(CounterSystem::new()).expect("id");
            runner
                .submit_command(request(1, 2, 5))
                .expect("future tick");
            runner
        };

        let mut a = make_runner();
        a.advance_ticks(4).expect("advance to tick 4");
        let snapshot = a.snapshot();
        a.advance_ticks(4).expect("advance to tick 8");
        let hashes_a = a.tick_hashes()[4..].to_vec();

        // A runner that diverged (tick 6) restores the tick-4 snapshot and
        // then reproduces the exact subsequent behavior and hashes.
        let mut b = make_runner();
        b.advance_ticks(6).expect("advance to tick 6");
        assert_ne!(b.simulation().tick(), 4);
        b.restore_snapshot(&snapshot).expect("restore");
        assert_eq!(b.simulation().tick(), 4);
        // Digests recorded beyond the restored tick were dropped.
        assert_eq!(b.tick_hashes().len(), 4);
        b.advance_ticks(4).expect("advance to tick 8");
        assert_eq!(b.tick_hashes()[4..], hashes_a);
        assert_eq!(b.canonical_digest(), a.canonical_digest());
    }

    #[test]
    fn restore_rejects_mismatched_system_sets() {
        struct Stateless;
        impl SimulationSystem<u32> for Stateless {
            fn id(&self) -> SystemId {
                SystemId("extra")
            }
            fn phase(&self) -> Phase {
                Phase::EmitEvents
            }

            fn order(&self) -> u64 {
                0
            }
            fn name(&self) -> &'static str {
                "extra"
            }

            fn as_any(&self) -> &dyn std::any::Any {
                self
            }

            fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
                self
            }
            fn run(
                &mut self,
                _ctx: &TickContext,
                _simulation: &mut Simulation<u32>,
                _phase: Phase,
            ) -> Result<SystemOutput<u32>, SystemError> {
                Ok(SystemOutput::default())
            }
        }

        let mut a: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        a.register_system(CounterSystem::new()).expect("id");
        a.advance_ticks(2).expect("advance");
        let snapshot = a.snapshot();

        // Snapshot names a system this runner does not have.
        let mut empty: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        assert_eq!(
            empty.restore_snapshot(&snapshot),
            Err(SnapshotError::UnknownSystemId("counter".to_owned()))
        );

        // This runner has a registered system missing from the snapshot.
        let mut with_extra: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        with_extra
            .register_system(CounterSystem::new())
            .expect("id");
        with_extra.register_system(Stateless).expect("id");
        assert_eq!(
            with_extra.restore_snapshot(&snapshot),
            Err(SnapshotError::MissingSystemState(SystemId("extra")))
        );
    }

    #[test]
    fn restore_rejects_wrong_encoding_version() {
        let mut a: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        a.register_system(CounterSystem::new()).expect("id");
        a.advance_ticks(1).expect("advance");
        let mut snapshot = a.snapshot();
        // Corrupt the encoding version (first u32).
        snapshot[0..4].copy_from_slice(&99_u32.to_le_bytes());
        let mut b: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        b.register_system(CounterSystem::new()).expect("id");
        assert_eq!(
            b.restore_snapshot(&snapshot),
            Err(SnapshotError::Canonical(
                CanonicalError::UnsupportedVersion {
                    found: 99,
                    expected: CANONICAL_ENCODING_VERSION
                }
            ))
        );
    }

    #[test]
    fn stateless_systems_round_trip_an_empty_state() {
        let mut a: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        a.register_system(Probe {
            phase: Phase::EmitEvents,
            order: 0,
            name: "probe",
            trace: Trace::default(),
            ingest_counts: IngestCounts::default(),
        })
        .expect("id");
        a.advance_ticks(2).expect("advance");
        let snapshot = a.snapshot();

        let mut b: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        b.register_system(Probe {
            phase: Phase::EmitEvents,
            order: 0,
            name: "probe",
            trace: Trace::default(),
            ingest_counts: IngestCounts::default(),
        })
        .expect("id");
        b.advance_ticks(3).expect("diverge to tick 3");
        b.restore_snapshot(&snapshot).expect("stateless restore");
        assert_eq!(b.simulation().tick(), 2);
        // Digests recorded beyond the restored tick were dropped.
        assert_eq!(b.tick_hashes().len(), 2);
        b.advance_ticks(2).expect("advance");
        a.advance_ticks(2).expect("advance");
        assert_eq!(a.tick_hashes()[2..], b.tick_hashes()[2..]);
    }

    /// A minimal stateless system parameterized by id/phase/order.
    struct Noop {
        id: &'static str,
        phase: Phase,
        order: u64,
    }

    impl<P> SimulationSystem<P> for Noop {
        fn id(&self) -> SystemId {
            SystemId(self.id)
        }

        fn phase(&self) -> Phase {
            self.phase
        }

        fn order(&self) -> u64 {
            self.order
        }

        fn name(&self) -> &'static str {
            self.id
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }

        fn run(
            &mut self,
            _ctx: &TickContext,
            _simulation: &mut Simulation<P>,
            _phase: Phase,
        ) -> Result<SystemOutput<P>, SystemError> {
            Ok(SystemOutput::default())
        }
    }

    fn noop(id: &'static str, phase: Phase, order: u64) -> Noop {
        Noop { id, phase, order }
    }

    #[test]
    fn registration_order_does_not_change_execution_or_manifest() {
        let build = |reverse: bool| {
            let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
            let systems = vec![
                noop("a", Phase::IngestCommands, 0),
                noop("b", Phase::IngestCommands, 1),
                noop("c", Phase::ResolveCombat, 0),
            ];
            let systems = if reverse {
                systems.into_iter().rev().collect::<Vec<_>>()
            } else {
                systems
            };
            for system in systems {
                runner.register_system(system).expect("unique keys");
            }
            runner
        };
        let mut a = build(false);
        let mut b = build(true);

        // The manifest is stable across independent construction and
        // independent of registration order.
        assert_eq!(
            a.finalize_registry().digest(),
            b.finalize_registry().digest()
        );
        assert_eq!(a.system_manifest(), b.system_manifest());
        let entries = a.system_manifest().expect("finalized").entries();
        let ids: Vec<&str> = entries.iter().map(|entry| entry.id.0).collect();
        assert_eq!(ids, ["a", "b", "c"], "manifest is in execution order");

        a.advance_ticks(2).expect("advance");
        b.advance_ticks(2).expect("advance");
        assert_eq!(a.tick_hashes(), b.tick_hashes());
    }

    #[test]
    fn duplicate_order_keys_in_a_phase_are_rejected() {
        let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        runner
            .register_system(noop("a", Phase::EmitEvents, 0))
            .expect("first key is fine");
        assert_eq!(
            runner
                .register_system(noop("b", Phase::EmitEvents, 0))
                .map(|_| ()),
            Err(SystemRegistrationError::DuplicateOrderKey {
                phase: Phase::EmitEvents,
                order: 0
            })
        );
        // The same ordering key in a different phase is fine.
        runner
            .register_system(noop("c", Phase::ValidateInvariants, 0))
            .expect("different phase allows the same key");
    }

    #[test]
    fn registration_is_closed_after_finalize_or_first_tick() {
        let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        assert!(!runner.is_finalized());
        assert!(runner.system_manifest().is_none());
        runner.finalize_registry();
        assert!(runner.is_finalized());
        assert_eq!(
            runner
                .register_system(noop("late", Phase::EmitEvents, 0))
                .map(|_| ()),
            Err(SystemRegistrationError::RegistryFrozen)
        );
        // Finalizing twice is idempotent.
        runner.finalize_registry();

        // The first tick implicitly finalizes an open registry.
        let mut other: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        other.advance_ticks(1).expect("tick");
        assert!(other.is_finalized());
        assert_eq!(
            other
                .register_system(noop("late", Phase::EmitEvents, 0))
                .map(|_| ()),
            Err(SystemRegistrationError::RegistryFrozen)
        );
    }
    // ---- issue #37: deterministic tick context and structured outputs ----

    struct ContextProbe {
        log: Rc<RefCell<Vec<(u64, u32, f32)>>>,
    }

    impl SimulationSystem<u32, u64> for ContextProbe {
        fn id(&self) -> SystemId {
            SystemId("context-probe")
        }

        fn phase(&self) -> Phase {
            Phase::UpdateEconomy
        }

        fn order(&self) -> u64 {
            0
        }

        fn name(&self) -> &'static str {
            "context-probe"
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
            _simulation: &mut Simulation<u32, u64>,
            _phase: Phase,
        ) -> Result<SystemOutput<u32, u64>, SystemError> {
            self.log.borrow_mut().push((
                ctx.tick(),
                ctx.tick_rate().ticks_per_second(),
                ctx.dt_seconds(),
            ));
            Ok(SystemOutput::default())
        }
    }

    #[test]
    fn systems_obtain_tick_and_config_from_the_single_authoritative_context() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let rate = TickRate::new(30).expect("valid rate");
        let mut runner: SimulationRunner<u32, u64> = SimulationRunner::new(rate, SEED);
        runner
            .register_system(ContextProbe { log: log.clone() })
            .expect("id");
        // The config is part of the simulation API and of canonical state.
        assert_eq!(runner.simulation().config().tick_rate(), rate);
        assert_eq!(runner.tick_rate(), rate);
        assert_eq!(
            runner.simulation().config().version(),
            SIMULATION_CONFIG_VERSION
        );
        runner.advance_ticks(3).expect("advance");
        assert_eq!(
            *log.borrow(),
            vec![
                (1, 30, 1.0 / 30.0),
                (2, 30, 1.0 / 30.0),
                (3, 30, 1.0 / 30.0)
            ]
        );
        // A different tick rate changes the context AND the canonical state.
        let mut other: SimulationRunner<u32, u64> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        other
            .register_system(ContextProbe {
                log: Rc::new(RefCell::new(Vec::new())),
            })
            .expect("id");
        other.advance_ticks(1).expect("advance");
        assert_ne!(
            runner.simulation().canonical_digest(),
            other.simulation().canonical_digest(),
            "the config is part of the canonical state"
        );
    }

    struct Emitter {
        events_per_tick: u64,
        intent_at_tick: Option<u64>,
    }

    impl SimulationSystem<u32, u64> for Emitter {
        fn id(&self) -> SystemId {
            SystemId("emitter")
        }

        fn phase(&self) -> Phase {
            Phase::UpdateEconomy
        }

        fn order(&self) -> u64 {
            0
        }

        fn name(&self) -> &'static str {
            "emitter"
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
            _simulation: &mut Simulation<u32, u64>,
            _phase: Phase,
        ) -> Result<SystemOutput<u32, u64>, SystemError> {
            let mut output = SystemOutput::default();
            for i in 0..self.events_per_tick {
                output.emit_event(ctx.tick() * 100 + i);
            }
            if Some(ctx.tick()) == self.intent_at_tick {
                output.emit_intent(SystemIntent::Command(CommandRequest {
                    request_id: RequestId(1),
                    actor: ActorId(9),
                    session: SessionId(0),
                    tick: ctx.tick() + 1,
                    subject: None,
                    payload: 777,
                }));
            }
            Ok(output)
        }
    }

    #[test]
    fn system_events_and_intents_are_recorded_deterministically_in_order() {
        let build = || {
            let mut runner: SimulationRunner<u32, u64> =
                SimulationRunner::new(TickRate::DEFAULT, SEED);
            runner
                .register_system(Emitter {
                    events_per_tick: 2,
                    intent_at_tick: Some(1),
                })
                .expect("id");
            // An external command for tick 2, submitted before the run starts.
            runner.submit_command(request(1, 2, 5)).expect("submit");
            runner
        };
        let mut a = build();
        let mut b = build();
        a.advance_ticks(3).expect("advance");
        b.advance_ticks(3).expect("advance");

        // Events: two per tick for ticks 1..=3, in emission order, attributed
        // to the emitter (the only system, so execution index 0).
        let events = a.simulation().events();
        assert_eq!(events.len(), 6);
        assert_eq!(a.simulation().emitted_event_count(), 6);
        let expected: Vec<(u64, u64)> = (1..=3_u64)
            .flat_map(|tick| [(tick * 100, tick), (tick * 100 + 1, tick)])
            .collect();
        for (event, (value, tick)) in events.iter().zip(expected) {
            assert_eq!(event.tick(), tick);
            assert_eq!(event.system_index(), 0);
            assert_eq!(event.kind(), &SystemEventKind::Output(value));
        }
        // The intent emitted at tick 1 targeted tick 2 and went through the
        // standard ingress: it got command ID 1 (after the external command
        // with ID 0) and both applied at tick 2 in arbitration-key order
        // (actor 0 before actor 9, issue #42).
        let applied = a.simulation().applied_commands();
        assert_eq!(applied.len(), 2);
        assert_eq!(applied[0].command_id(), CommandId(0));
        assert_eq!(applied[0].payload(), &5);
        assert_eq!(applied[1].command_id(), CommandId(1));
        assert_eq!(applied[1].payload(), &777);
        assert_eq!(applied[1].actor(), ActorId(9));
        assert_eq!(applied[1].tick(), 2);
        // Identical runs: identical transcripts, digests, and tick hashes.
        assert_eq!(a.simulation().events(), b.simulation().events());
        assert_eq!(a.canonical_digest(), b.canonical_digest());
        assert_eq!(a.tick_hashes(), b.tick_hashes());
    }

    struct RecoverableFailure {
        code: u32,
    }

    impl SimulationSystem<u32, u64> for RecoverableFailure {
        fn id(&self) -> SystemId {
            SystemId("recoverable")
        }

        fn phase(&self) -> Phase {
            Phase::UpdateEconomy
        }

        fn order(&self) -> u64 {
            0
        }

        fn name(&self) -> &'static str {
            "recoverable"
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }

        fn run(
            &mut self,
            _ctx: &TickContext,
            _simulation: &mut Simulation<u32, u64>,
            _phase: Phase,
        ) -> Result<SystemOutput<u32, u64>, SystemError> {
            Err(SystemError::Event {
                system: self.id(),
                code: self.code,
                message: "recoverable probe failure",
            })
        }
    }

    #[test]
    fn non_fatal_system_error_becomes_a_canonical_event_and_the_tick_continues() {
        let build = || {
            let mut runner: SimulationRunner<u32, u64> =
                SimulationRunner::new(TickRate::DEFAULT, SEED);
            runner
                .register_system(RecoverableFailure { code: 7 })
                .expect("id");
            runner
        };
        let mut a = build();
        let mut b = build();
        a.advance_ticks(3)
            .expect("the tick continues after a non-fatal error");
        b.advance_ticks(3).expect("advance");
        let events = a.simulation().events();
        assert_eq!(events.len(), 3);
        assert!(
            events
                .iter()
                .all(|e| e.system_index() == 0 && e.kind() == &SystemEventKind::Failure { code: 7 })
        );
        assert_eq!(a.simulation().terminated(), None);
        assert_eq!(a.simulation().tick(), 3);
        // Identical runs record identical failures and digests.
        assert_eq!(a.simulation().events(), b.simulation().events());
        assert_eq!(a.canonical_digest(), b.canonical_digest());
        assert_eq!(a.tick_hashes(), b.tick_hashes());
    }

    struct FatalAt {
        tick: u64,
        code: u32,
    }

    impl SimulationSystem<u32, u64> for FatalAt {
        fn id(&self) -> SystemId {
            SystemId("fatal-at")
        }

        fn phase(&self) -> Phase {
            Phase::UpdateEconomy
        }

        fn order(&self) -> u64 {
            0
        }

        fn name(&self) -> &'static str {
            "fatal-at"
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
            _simulation: &mut Simulation<u32, u64>,
            _phase: Phase,
        ) -> Result<SystemOutput<u32, u64>, SystemError> {
            if ctx.tick() == self.tick {
                Err(SystemError::Fatal {
                    system: self.id(),
                    code: self.code,
                    message: "fatal probe failure",
                })
            } else {
                Ok(SystemOutput::default())
            }
        }
    }

    #[test]
    fn fatal_system_error_terminates_the_run_deterministically() {
        let build = || {
            let mut runner: SimulationRunner<u32, u64> =
                SimulationRunner::new(TickRate::DEFAULT, SEED);
            runner
                .register_system(FatalAt { tick: 2, code: 9 })
                .expect("id");
            runner
        };
        let termination = SystemTermination::new(2, 0, 9);
        let mut a = build();
        let mut b = build();
        a.step_tick().expect("tick 1");
        assert_eq!(a.step_tick(), Err(StepError::Terminated { termination }));
        // Further ticks are refused.
        assert_eq!(a.step_tick(), Err(StepError::Terminated { termination }));
        // The termination and the failure event are canonical state.
        assert_eq!(a.simulation().terminated(), Some(termination));
        assert_eq!(a.simulation().events().len(), 1);
        assert_eq!(
            a.simulation().events()[0].kind(),
            &SystemEventKind::Failure { code: 9 }
        );
        assert_eq!(a.simulation().events()[0].tick(), 2);
        // The terminating tick's checkpoint digest was still recorded.
        assert_eq!(a.tick_hashes().len(), 2);
        // Identical runs terminate identically.
        b.step_tick().expect("tick 1");
        assert_eq!(b.step_tick(), Err(StepError::Terminated { termination }));
        assert_eq!(a.canonical_digest(), b.canonical_digest());
        assert_eq!(a.tick_hashes(), b.tick_hashes());
        // Snapshot/restore preserves the termination and the transcript.
        let mut c = build();
        c.restore_snapshot(&a.snapshot()).expect("restore");
        assert_eq!(c.simulation().terminated(), Some(termination));
        assert_eq!(c.simulation().events(), a.simulation().events());
        assert_eq!(c.canonical_digest(), a.canonical_digest());
    }

    struct BadIntent;

    impl SimulationSystem<u32, u64> for BadIntent {
        fn id(&self) -> SystemId {
            SystemId("bad-intent")
        }

        fn phase(&self) -> Phase {
            Phase::UpdateEconomy
        }

        fn order(&self) -> u64 {
            0
        }

        fn name(&self) -> &'static str {
            "bad-intent"
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
            _simulation: &mut Simulation<u32, u64>,
            _phase: Phase,
        ) -> Result<SystemOutput<u32, u64>, SystemError> {
            let mut output = SystemOutput::default();
            // Illegal: the target tick is the current tick, which ingress
            // rejects as LateTick.
            output.emit_intent(SystemIntent::Command(CommandRequest {
                request_id: RequestId(1),
                actor: ActorId(9),
                session: SessionId(0),
                tick: ctx.tick(),
                subject: None,
                payload: 1,
            }));
            Ok(output)
        }
    }

    #[test]
    fn intent_rejected_by_ingress_terminates_instead_of_being_dropped() {
        let mut runner: SimulationRunner<u32, u64> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        runner.register_system(BadIntent).expect("id");
        let termination = SystemTermination::new(1, 0, SYSTEM_ERROR_INTENT_REJECTED);
        assert_eq!(
            runner.step_tick(),
            Err(StepError::Terminated { termination })
        );
        assert_eq!(
            runner.simulation().events()[0].kind(),
            &SystemEventKind::Failure {
                code: SYSTEM_ERROR_INTENT_REJECTED
            }
        );
        assert!(
            runner.simulation().pending_command_ticks().next().is_none(),
            "the rejected intent was not queued"
        );
    }
    // ---- issue #42: deterministic multi-producer arbitration ----

    #[test]
    fn same_tick_arbitration_is_independent_of_producer_arrival_order() {
        // The same command set submitted in different arrival orders must
        // arbitrate identically at the tick boundary.
        let build = |reverse: bool| {
            let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
            // Request IDs are monotonic per (actor, session), but the
            // cross-producer arrival order is scrambled in the reversed run.
            let requests = if reverse {
                vec![
                    request_from(2, 1, 1, 20),
                    request_from(1, 1, 1, 10),
                    request_from(1, 2, 1, 11),
                    request_from(2, 2, 1, 21),
                ]
            } else {
                vec![
                    request_from(1, 1, 1, 10),
                    request_from(2, 1, 1, 20),
                    request_from(1, 2, 1, 11),
                    request_from(2, 2, 1, 21),
                ]
            };
            for request in requests {
                runner.submit_command(request).expect("submit");
            }
            runner
        };
        let mut first = build(false);
        let mut second = build(true);
        first.step_tick().expect("tick 1");
        second.step_tick().expect("tick 1");
        // Command IDs are arrival-order identities, so they differ between the
        // two runs...
        let ids = |runner: &SimulationRunner<u32>| {
            runner
                .simulation()
                .applied_commands()
                .iter()
                .map(|envelope| envelope.command_id())
                .collect::<Vec<_>>()
        };
        assert_ne!(ids(&first), ids(&second));
        // ...but the arbitration order (actor, session, request_id) is
        // identical, and the transcript records it.
        let order = |runner: &SimulationRunner<u32>| {
            runner
                .simulation()
                .applied_commands()
                .iter()
                .map(|envelope| {
                    (
                        envelope.actor(),
                        envelope.session(),
                        envelope.request_id(),
                        *envelope.payload(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let expected = vec![
            (ActorId(1), SessionId(0), RequestId(1), 10),
            (ActorId(1), SessionId(0), RequestId(2), 11),
            (ActorId(2), SessionId(0), RequestId(1), 20),
            (ActorId(2), SessionId(0), RequestId(2), 21),
        ];
        assert_eq!(order(&first), expected);
        assert_eq!(order(&first), order(&second));
        // Outcomes are recorded in arbitration order as well.
        assert_eq!(first.simulation().applied_outcomes().len(), 4);
        assert!(
            first
                .simulation()
                .applied_outcomes()
                .iter()
                .all(|(_, outcome)| *outcome == CommandOutcome::Applied)
        );
    }

    struct SubjectApplier {
        log: Rc<RefCell<Vec<(ActorId, u32)>>>,
        last: Rc<RefCell<Option<(ActorId, u32)>>>,
    }

    impl SimulationSystem<u32> for SubjectApplier {
        fn id(&self) -> SystemId {
            SystemId("applier")
        }

        fn phase(&self) -> Phase {
            Phase::ApplyCommands
        }

        fn order(&self) -> u64 {
            0
        }

        fn name(&self) -> &'static str {
            "applier"
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }

        fn run(
            &mut self,
            _ctx: &TickContext,
            simulation: &mut Simulation<u32>,
            _phase: Phase,
        ) -> Result<SystemOutput<u32>, SystemError> {
            // Applies commands in the order the runner arbitrated them;
            // for one subject the last applied command wins.
            for envelope in simulation.commands_for_current_tick() {
                self.log
                    .borrow_mut()
                    .push((envelope.actor(), *envelope.payload()));
                *self.last.borrow_mut() = Some((envelope.actor(), *envelope.payload()));
            }
            Ok(SystemOutput::default())
        }
    }

    #[test]
    fn conflicting_same_subject_commands_resolve_last_in_arbitration_order() {
        let build = |reverse: bool| {
            let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
            let log = Rc::new(RefCell::new(Vec::new()));
            let last = Rc::new(RefCell::new(None));
            runner
                .register_system(SubjectApplier {
                    log: log.clone(),
                    last: last.clone(),
                })
                .expect("id");
            let subject = runner
                .simulation_mut()
                .entities_mut()
                .allocate()
                .expect("entity");
            let from = |actor: u64, request_id: u64, payload: u32| CommandRequest {
                request_id: RequestId(request_id),
                actor: ActorId(actor),
                session: SessionId(0),
                tick: 1,
                subject: Some(subject),
                payload,
            };
            let requests = if reverse {
                vec![from(2, 1, 200), from(1, 1, 100)]
            } else {
                vec![from(1, 1, 100), from(2, 1, 200)]
            };
            for request in requests {
                runner.submit_command(request).expect("submit");
            }
            (runner, log, last)
        };
        let (mut a, log_a, last_a) = build(false);
        let (mut b, log_b, last_b) = build(true);
        a.step_tick().expect("tick 1");
        b.step_tick().expect("tick 1");
        // Arbitration order is (actor 1, req 1) then (actor 2, req 1) in both
        // runs, regardless of arrival order; the last in arbitration order
        // wins for the shared subject.
        assert_eq!(*log_a.borrow(), vec![(ActorId(1), 100), (ActorId(2), 200)]);
        assert_eq!(*log_a.borrow(), *log_b.borrow());
        assert_eq!(*last_a.borrow(), Some((ActorId(2), 200)));
        assert_eq!(*last_a.borrow(), *last_b.borrow());
    }
}
