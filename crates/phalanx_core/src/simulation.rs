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

use std::collections::BTreeMap;

use crate::command::{
    CommandEnvelope, CommandId, CommandRequest, CommandSubmitError, ENVELOPE_VERSION,
};
use crate::entity::EntityAllocator;
use crate::hash::{CanonicalHasher, CanonicalSerialize};
use crate::rng::RngStreams;

pub use crate::TickOverflow;

/// The default fixed simulation rate: 20 ticks per second (initial proposal in
/// `RtsEngineDoc.md` §3.2; the final tick rate remains an open question in
/// ADR-0001).
pub const DEFAULT_TICKS_PER_SECOND: u32 = 20;

/// The version of the authoritative simulation state layout. Recorded in
/// replays and savegames; bump it on any change that alters canonical state.
pub const SIMULATION_SCHEMA_VERSION: u32 = 1;

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

/// Authoritative simulation state owned by the CPU simulation (ADR-0001
/// Decision §1).
///
/// The state is the simulation tick, the entity allocator, the deterministic
/// RNG streams, and the tick-indexed command bookkeeping. Gameplay world state
/// lands in later milestones; nothing here pretends gameplay systems exist.
pub struct Simulation<P> {
    tick: u64,
    rng: RngStreams,
    entities: EntityAllocator,
    pending_commands: BTreeMap<u64, Vec<CommandEnvelope<P>>>,
    current_tick_commands: Vec<CommandEnvelope<P>>,
    seen_requests: BTreeMap<crate::command::RequestId, CommandId>,
    next_command_id: u64,
}

impl<P> Simulation<P> {
    /// Creates the initial authoritative state at tick 0 with the given
    /// explicit master RNG seed.
    #[must_use]
    pub fn new(rng_seed: u64) -> Self {
        Self {
            tick: 0,
            rng: RngStreams::new(rng_seed),
            entities: EntityAllocator::new(),
            pending_commands: BTreeMap::new(),
            current_tick_commands: Vec::new(),
            seen_requests: BTreeMap::new(),
            next_command_id: 0,
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
    /// ascending command-ID order. They are drained from the pending buffer at
    /// the tick boundary and are only visible while that tick runs.
    #[must_use]
    pub fn commands_for_current_tick(&self) -> &[CommandEnvelope<P>] {
        &self.current_tick_commands
    }

    /// Returns the ticks that still have queued commands, in ascending order.
    pub fn pending_command_ticks(
        &self,
    ) -> std::collections::btree_map::Keys<'_, u64, Vec<CommandEnvelope<P>>> {
        self.pending_commands.keys()
    }
}

impl<P: CanonicalSerialize> Simulation<P> {
    /// Produces the canonical state hash of the authoritative simulation
    /// state (ADR-0001 Decision §8, `RtsEngineDoc.md` §13.3).
    ///
    /// The hash covers the schema version, tick, entity allocator, RNG stream
    /// states, the accepted-request bookkeeping, and the commands applied at
    /// the current tick. It uses stable logical ordering (slot index, stream
    /// name, command ID) and never depends on storage layout. The pending
    /// input queue is deliberately not hashed: it is the recorded command
    /// stream (see [`crate::replay`]) and is compared through execution, so an
    /// altered input is reported at the tick where it is applied rather than
    /// at tick 1. Renderer/GPU state and wall-clock data cannot enter the hash
    /// because the authoritative types do not contain them.
    #[must_use]
    pub fn canonical_hash(&self) -> u64 {
        let mut hasher = CanonicalHasher::new();
        hasher.write_u32(SIMULATION_SCHEMA_VERSION);
        hasher.write_u64(self.tick);
        self.entities.write_canonical(&mut hasher);
        self.rng.write_canonical(&mut hasher);
        self.current_tick_commands.write_canonical(&mut hasher);
        self.seen_requests.write_canonical(&mut hasher);
        hasher.write_u64(self.next_command_id);
        hasher.finish()
    }
}

/// A hook that runs inside one [`Phase`] of every tick.
///
/// Systems declare their phase and an explicit in-phase ordering key. The
/// runner executes phases in [`Phase::SCHEDULE`] order; registration order
/// only breaks exact (phase, order) ties and is therefore never what defines
/// the schedule.
pub trait SimulationSystem<P> {
    /// The phase this system runs in.
    fn phase(&self) -> Phase;

    /// Explicit ordering key within the phase. Defaults to 0.
    fn order(&self) -> u64 {
        0
    }

    /// Stable diagnostic name used by tests and replay diagnostics.
    fn name(&self) -> &'static str;

    /// Executes the system for the current tick. Placeholder hooks must not
    /// pretend gameplay systems exist.
    fn run(&mut self, simulation: &mut Simulation<P>, phase: Phase);
}

struct RegisteredSystem<P> {
    phase: Phase,
    order: u64,
    system: Box<dyn SimulationSystem<P>>,
}

/// The fixed-tick, CPU-only, deterministic simulation loop.
///
/// Advancing the simulation is separated from any real-time application loop:
/// callers step exactly one tick with [`SimulationRunner::step_tick`] or an
/// exact count with [`SimulationRunner::advance_ticks`]. No sleeping, window,
/// or wall-clock read is involved.
pub struct SimulationRunner<P> {
    simulation: Simulation<P>,
    tick_rate: TickRate,
    systems: Vec<RegisteredSystem<P>>,
    hashes: Vec<u64>,
    record_hashes: bool,
}

impl<P: CanonicalSerialize> SimulationRunner<P> {
    /// Creates a runner at the given fixed tick rate with the given explicit
    /// master RNG seed and no systems registered. Per-tick state hash
    /// recording is enabled by default.
    #[must_use]
    pub fn new(tick_rate: TickRate, rng_seed: u64) -> Self {
        Self {
            simulation: Simulation::new(rng_seed),
            tick_rate,
            systems: Vec::new(),
            hashes: Vec::new(),
            record_hashes: true,
        }
    }

    /// Returns the canonical state hash recorded for each executed tick, in
    /// tick order. Empty when hash recording is disabled.
    #[must_use]
    pub fn tick_hashes(&self) -> &[u64] {
        &self.hashes
    }

    /// Returns whether per-tick state hash recording is enabled.
    #[must_use]
    pub const fn record_hashes(&self) -> bool {
        self.record_hashes
    }

    /// Enables or disables per-tick state hash recording. Hashing is always
    /// available via [`Simulation::canonical_hash`]; recording may be disabled
    /// in release builds (`RtsEngineDoc.md` §13.3) but must stay enabled in
    /// tests and diagnostic builds.
    pub fn set_record_hashes(&mut self, enabled: bool) {
        self.record_hashes = enabled;
    }

    /// Returns the configured fixed tick rate.
    #[must_use]
    pub const fn tick_rate(&self) -> TickRate {
        self.tick_rate
    }

    /// Returns the authoritative simulation state.
    #[must_use]
    pub const fn simulation(&self) -> &Simulation<P> {
        &self.simulation
    }

    /// Registers a system. The phase schedule comes from [`Phase::SCHEDULE`]
    /// and each system's declared `(phase, order)`, not from registration
    /// order.
    pub fn register_system<S>(&mut self, system: S) -> &mut Self
    where
        S: SimulationSystem<P> + 'static,
    {
        let registered = RegisteredSystem {
            phase: system.phase(),
            order: system.order(),
            system: Box::new(system),
        };
        self.systems.push(registered);
        // Stable sort: the phase schedule dominates, so phase execution order
        // never depends on registration order.
        self.systems
            .sort_by_key(|registered| (registered.phase.index(), registered.order));
        self
    }

    /// Queues a command to be applied at the boundary of its target tick.
    ///
    /// Returns the assigned [`CommandId`]. Commands targeting the current or a
    /// past tick, duplicate request IDs, and unsupported envelope versions are
    /// rejected with structured errors; commands are never silently retargeted.
    pub fn submit_command(
        &mut self,
        request: CommandRequest<P>,
    ) -> Result<CommandId, CommandSubmitError> {
        if request.tick <= self.simulation.tick {
            return Err(CommandSubmitError::LateTick {
                requested: request.tick,
                current: self.simulation.tick,
            });
        }
        if self
            .simulation
            .seen_requests
            .contains_key(&request.request_id)
        {
            return Err(CommandSubmitError::DuplicateRequest {
                request_id: request.request_id,
            });
        }
        let raw_id = self.simulation.next_command_id;
        self.simulation.next_command_id = raw_id
            .checked_add(1)
            .ok_or(CommandSubmitError::IdSpaceExhausted)?;
        let command_id = CommandId(raw_id);

        let envelope = CommandEnvelope::new(
            ENVELOPE_VERSION,
            request.request_id,
            command_id,
            request.actor,
            request.tick,
            request.payload,
        )?;
        self.simulation
            .seen_requests
            .insert(request.request_id, command_id);
        self.simulation
            .pending_commands
            .entry(request.tick)
            .or_default()
            .push(envelope);
        Ok(command_id)
    }

    /// Advances the simulation by exactly one tick: increments the
    /// authoritative tick counter, drains the commands assigned to that tick
    /// in ascending command-ID order, then executes every phase in
    /// [`Phase::SCHEDULE`] order. Never reads wall-clock time.
    pub fn step_tick(&mut self) -> Result<(), TickOverflow> {
        let next_tick = self.simulation.tick.checked_add(1).ok_or(TickOverflow)?;
        self.simulation.tick = next_tick;
        let mut commands = self
            .simulation
            .pending_commands
            .remove(&next_tick)
            .unwrap_or_default();
        // Deterministic same-tick ordering: ascending command ID.
        commands.sort_by_key(|envelope| envelope.command_id());
        self.simulation.current_tick_commands = commands;

        let SimulationRunner {
            simulation,
            systems,
            hashes,
            record_hashes,
            ..
        } = self;
        for phase in Phase::SCHEDULE {
            for registered in systems.iter_mut() {
                if registered.phase == phase {
                    registered.system.run(simulation, phase);
                }
            }
        }

        // Phase 15 (produce state hash): hash after all phases ran, while the
        // tick's applied commands are still visible.
        if *record_hashes {
            hashes.push(simulation.canonical_hash());
        }
        simulation.current_tick_commands.clear();
        Ok(())
    }

    /// Advances the simulation by exactly `count` ticks. This is the
    /// deterministic stepping API used by tests and replay tools.
    pub fn advance_ticks(&mut self, count: u64) -> Result<(), TickOverflow> {
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
    use crate::command::{ActorId, RequestId};

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
        fn phase(&self) -> Phase {
            self.phase
        }

        fn order(&self) -> u64 {
            self.order
        }

        fn name(&self) -> &'static str {
            self.name
        }

        fn run(&mut self, simulation: &mut Simulation<P>, phase: Phase) {
            self.trace
                .borrow_mut()
                .push((simulation.tick(), phase, self.name));
            if phase == Phase::IngestCommands {
                let count = simulation.commands_for_current_tick().len();
                self.ingest_counts
                    .borrow_mut()
                    .push((simulation.tick(), count));
            }
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
            .register_system(Probe {
                phase: Phase::ResolveCombat,
                order: 0,
                name: "combat",
                trace: trace.clone(),
                ingest_counts: ingest_counts.clone(),
            })
            .register_system(Probe {
                phase: Phase::IngestCommands,
                order: 1,
                name: "ingest-b",
                trace: trace.clone(),
                ingest_counts: ingest_counts.clone(),
            })
            .register_system(Probe {
                phase: Phase::IngestCommands,
                order: 0,
                name: "ingest-a",
                trace: trace.clone(),
                ingest_counts: ingest_counts.clone(),
            });
        runner
    }

    fn probe_runner(trace: &Trace, ingest_counts: &IngestCounts) -> SimulationRunner<u32> {
        probe_runner_at(TickRate::DEFAULT, trace, ingest_counts)
    }

    fn request(request_id: u64, tick: u64, payload: u32) -> CommandRequest<u32> {
        CommandRequest {
            request_id: RequestId(request_id),
            actor: ActorId(0),
            tick,
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
        runner.register_system(Probe {
            phase: Phase::IngestCommands,
            order: 0,
            name: "ingest",
            trace: Trace::default(),
            ingest_counts: counts.clone(),
        });

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
    fn same_tick_commands_apply_in_ascending_command_id_order() {
        struct CommandProbe {
            log: CommandLog,
        }

        impl SimulationSystem<u32> for CommandProbe {
            fn phase(&self) -> Phase {
                Phase::IngestCommands
            }

            fn name(&self) -> &'static str {
                "command-probe"
            }

            fn run(&mut self, simulation: &mut Simulation<u32>, _phase: Phase) {
                self.log.borrow_mut().push((
                    simulation.tick(),
                    simulation
                        .commands_for_current_tick()
                        .iter()
                        .map(|envelope| envelope.payload())
                        .copied()
                        .collect(),
                ));
            }
        }

        let log = CommandLog::default();
        let mut runner: SimulationRunner<u32> = SimulationRunner::new(TickRate::DEFAULT, SEED);
        runner.register_system(CommandProbe { log: log.clone() });

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

        assert_eq!(runner.step_tick(), Err(TickOverflow));
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
            fn phase(&self) -> Phase {
                Phase::ApplyCommands
            }

            fn name(&self) -> &'static str {
                "world-probe"
            }

            fn run(&mut self, simulation: &mut Simulation<()>, _phase: Phase) {
                let entity = simulation
                    .entities_mut()
                    .allocate()
                    .expect("allocate entity");
                let draw = simulation.rng_mut().stream("combat").next_u64();
                self.log.borrow_mut().push((
                    simulation.tick(),
                    entity.index(),
                    entity.generation(),
                    draw,
                ));
            }
        }

        let log_a = Rc::new(RefCell::new(Vec::new()));
        let log_b = Rc::new(RefCell::new(Vec::new()));
        let mut a: SimulationRunner<()> = SimulationRunner::new(TickRate::DEFAULT, 42);
        let mut b: SimulationRunner<()> = SimulationRunner::new(TickRate::DEFAULT, 42);
        a.register_system(WorldProbe { log: log_a.clone() });
        b.register_system(WorldProbe { log: log_b.clone() });

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
}
