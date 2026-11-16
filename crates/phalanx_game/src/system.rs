//! `SimulationSystem` glue for the battle world. Three systems share the
//! world (the runner is single-threaded, so `Rc<RefCell<_>>` stays
//! deterministic); only `BattleSystem` carries canonical state, which covers
//! the whole world (movement + combat + events + match outcome).

use std::cell::RefCell;
use std::rc::Rc;

use phalanx_core::command::{CommandEnvelope, CommandPayload, CommandVerdict};
use phalanx_core::context::{SystemError, SystemOutput, TickContext};
use phalanx_core::hash::{CanonicalError, CanonicalReader, CanonicalSink};
use phalanx_core::simulation::{Phase, Simulation, SimulationRunner, SimulationSystem, SystemId};

use crate::events::CommandFailReason;
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

    fn validate_command(
        &mut self,
        _ctx: &TickContext,
        _simulation: &Simulation<CommandPayload>,
        envelope: &CommandEnvelope<CommandPayload>,
    ) -> CommandVerdict {
        // Application-time validation (lifecycle stage 3, issue #43) against
        // the authoritative world at the command's target tick: unknown
        // subjects and invalid attack targets are rejected here, with typed
        // reason codes, before any command is applied this tick.
        let Some(subject) = envelope.subject() else {
            return CommandVerdict::Accept;
        };
        let world = self.world.borrow();
        let Some(state) = world.combat.get(&subject) else {
            return CommandVerdict::Reject {
                reason: CommandFailReason::UnknownSubject.code(),
            };
        };
        if let CommandPayload::Attack { target } = envelope.payload() {
            let valid = world
                .combat
                .get(target)
                .is_some_and(|other| other.team != state.team);
            if !valid {
                return CommandVerdict::Reject {
                    reason: CommandFailReason::InvalidTarget.code(),
                };
            }
        }
        CommandVerdict::Accept
    }

    fn run(
        &mut self,
        _ctx: &TickContext,
        simulation: &mut Simulation<CommandPayload>,
        _phase: Phase,
    ) -> Result<SystemOutput<CommandPayload>, SystemError> {
        self.world.borrow_mut().apply_commands(simulation);
        Ok(SystemOutput::default())
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

    fn run(
        &mut self,
        _ctx: &TickContext,
        _simulation: &mut Simulation<CommandPayload>,
        _phase: Phase,
    ) -> Result<SystemOutput<CommandPayload>, SystemError> {
        self.world.borrow_mut().step_movement();
        Ok(SystemOutput::default())
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

    fn run(
        &mut self,
        _ctx: &TickContext,
        simulation: &mut Simulation<CommandPayload>,
        _phase: Phase,
    ) -> Result<SystemOutput<CommandPayload>, SystemError> {
        self.world.borrow_mut().step_combat(simulation);
        Ok(SystemOutput::default())
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
