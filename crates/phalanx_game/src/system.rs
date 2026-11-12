//! `SimulationSystem` glue for the battle world. Three systems share the
//! world (the runner is single-threaded, so `Rc<RefCell<_>>` stays
//! deterministic); only `BattleSystem` carries canonical state, which covers
//! the whole world (movement + combat + events + match outcome).

use std::cell::RefCell;
use std::rc::Rc;

use phalanx_core::command::CommandPayload;
use phalanx_core::hash::{CanonicalError, CanonicalReader, CanonicalSink};
use phalanx_core::simulation::{Phase, Simulation, SimulationRunner, SimulationSystem, SystemId};

use crate::world::BattleWorld;

/// Shared battle world handle.
pub type SharedBattleWorld = Rc<RefCell<BattleWorld>>;

/// Applies tick commands (runs in `Phase::ApplyCommands`).
pub struct BattleCommandSystem {
    world: SharedBattleWorld,
}

/// Steps movement (runs in `Phase::ResolveMovement`).
pub struct BattleMovementSystem {
    world: SharedBattleWorld,
}

/// Resolves combat and owns the canonical state (runs in
/// `Phase::ResolveCombat`).
pub struct BattleSystem {
    world: SharedBattleWorld,
}

impl SimulationSystem<CommandPayload> for BattleCommandSystem {
    fn id(&self) -> SystemId {
        SystemId("battle-commands")
    }

    fn phase(&self) -> Phase {
        Phase::ApplyCommands
    }

    fn order(&self) -> u64 {
        0
    }

    fn name(&self) -> &'static str {
        "battle-commands"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn run(&mut self, simulation: &mut Simulation<CommandPayload>, _phase: Phase) {
        self.world.borrow_mut().apply_commands(simulation);
    }
}

impl SimulationSystem<CommandPayload> for BattleMovementSystem {
    fn id(&self) -> SystemId {
        SystemId("battle-movement")
    }

    fn phase(&self) -> Phase {
        Phase::ResolveMovement
    }

    fn order(&self) -> u64 {
        0
    }

    fn name(&self) -> &'static str {
        "battle-movement"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn run(&mut self, _simulation: &mut Simulation<CommandPayload>, _phase: Phase) {
        self.world.borrow_mut().step_movement();
    }
}

impl SimulationSystem<CommandPayload> for BattleSystem {
    fn id(&self) -> SystemId {
        SystemId("battle-combat")
    }

    fn phase(&self) -> Phase {
        Phase::ResolveCombat
    }

    fn order(&self) -> u64 {
        0
    }

    fn name(&self) -> &'static str {
        "battle-combat"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn write_canonical_state(&self, sink: &mut dyn CanonicalSink) {
        self.world.borrow().write_state(sink);
    }

    fn restore_canonical_state(
        &mut self,
        reader: &mut CanonicalReader<'_>,
    ) -> Result<(), CanonicalError> {
        self.world.borrow_mut().restore_state(reader)
    }

    fn run(&mut self, simulation: &mut Simulation<CommandPayload>, _phase: Phase) {
        self.world.borrow_mut().step_combat(simulation);
    }
}

/// Registers the three battle systems on a runner and returns the shared
/// world handle for setup (spawning) and inspection.
pub fn register_battle_systems(
    runner: &mut SimulationRunner<CommandPayload>,
    world: BattleWorld,
) -> SharedBattleWorld {
    let world = Rc::new(RefCell::new(world));
    runner
        .register_system(BattleCommandSystem {
            world: world.clone(),
        })
        .expect("unique system id");
    runner
        .register_system(BattleMovementSystem {
            world: world.clone(),
        })
        .expect("unique system id");
    runner
        .register_system(BattleSystem {
            world: world.clone(),
        })
        .expect("unique system id");
    world
}
