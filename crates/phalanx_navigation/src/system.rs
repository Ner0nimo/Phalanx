//! `SimulationSystem` glue: steps the [`MovementWorld`] in
//! `Phase::ResolveMovement` and exposes its canonical state through the
//! issue #34 state hooks, so movement is covered by checkpoint digests,
//! snapshots, and replays.

use phalanx_core::command::CommandPayload;
use phalanx_core::hash::{CanonicalError, CanonicalReader, CanonicalSink};
use phalanx_core::simulation::{Phase, Simulation, SimulationSystem, SystemId};

use crate::movement::MovementWorld;

/// The movement system. Runs once per tick in [`Phase::ResolveMovement`]:
/// path-request budget first, then route following, separation, and obstacle
/// constraints (all inside [`MovementWorld::step`]).
pub struct MovementSystem {
    world: MovementWorld,
}

impl MovementSystem {
    /// Creates the system owning the given world.
    #[must_use]
    pub fn new(world: MovementWorld) -> Self {
        Self { world }
    }

    /// Shared access to the world (spawn units, issue orders, inspect).
    #[must_use]
    pub const fn world(&self) -> &MovementWorld {
        &self.world
    }

    /// Mutable access to the world (spawn units, issue orders before ticking).
    #[must_use]
    pub fn world_mut(&mut self) -> &mut MovementWorld {
        &mut self.world
    }
}

impl SimulationSystem<CommandPayload> for MovementSystem {
    fn id(&self) -> SystemId {
        SystemId("movement")
    }

    fn phase(&self) -> Phase {
        Phase::ResolveMovement
    }

    fn order(&self) -> u64 {
        0
    }

    fn name(&self) -> &'static str {
        "movement"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn write_canonical_state(&self, sink: &mut dyn CanonicalSink) {
        self.world.write_state(sink);
    }

    fn restore_canonical_state(
        &mut self,
        reader: &mut CanonicalReader<'_>,
    ) -> Result<(), CanonicalError> {
        self.world.restore_state(reader)
    }

    fn run(&mut self, _simulation: &mut Simulation<CommandPayload>, _phase: Phase) {
        self.world.step();
    }
}
