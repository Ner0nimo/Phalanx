//! The scripted "Crossing assault" acceptance scenario
//! (`docs/vertical-slice-scenario.md` §2–§3, §8): map, teams, spawn layout,
//! and the fixed command stream.

use phalanx_core::command::{ActorId, CommandPayload, CommandRequest, RequestId};
use phalanx_core::entity::EntityId;
use phalanx_core::simulation::{Simulation, SimulationRunner, TickRate};
use phalanx_navigation::crossing_map;

use crate::defs::slice_roster;
use crate::system::{SharedBattleWorld, register_battle_systems};
use crate::world::BattleWorld;

/// Team A (player side) id; `ActorId(1)` issues its commands.
pub const TEAM_A: u64 = 1;
/// Team B (scripted side) id; `ActorId(2)` issues its commands.
pub const TEAM_B: u64 = 2;

/// The master RNG seed of the deterministic slice scenario (slice rules are
/// RNG-free; the seed exists for replay/hash state).
pub const SCENARIO_SEED: u64 = 0xBA771E;

/// Planner budget (path requests per tick) for the slice scenario.
pub const SCENARIO_PLANNER_BUDGET: u32 = 64;

/// Units per team: 3 strikers + 3 skirmishers (spec §3).
pub const UNITS_PER_TEAM: usize = 6;

/// Builds the battle world over the Crossing map with the slice roster.
#[must_use]
pub fn battle_world() -> BattleWorld {
    BattleWorld::new(
        &crossing_map(),
        1.0,
        0.5,
        TickRate::DEFAULT.ticks_per_second(),
        SCENARIO_PLANNER_BUDGET,
        slice_roster(),
    )
}

/// Registers the battle systems on a runner and returns the shared world.
pub fn setup_battle(runner: &mut SimulationRunner<CommandPayload>) -> SharedBattleWorld {
    register_battle_systems(runner, battle_world())
}

/// Spawns one team's 6 units (3 strikers + 3 skirmishers) on its spawn pad in
/// a fixed deterministic layout. Returns the unit IDs in spawn order
/// (strikers first — Team A's units therefore have the lowest IDs).
pub fn spawn_team(
    simulation: &mut Simulation<CommandPayload>,
    world: &SharedBattleWorld,
    team: u64,
    pad_center: (f32, f32),
) -> Vec<EntityId> {
    let defs = [
        "unit.striker",
        "unit.striker",
        "unit.striker",
        "unit.skirmisher",
        "unit.skirmisher",
        "unit.skirmisher",
    ];
    let mut ids = Vec::with_capacity(UNITS_PER_TEAM);
    for (index, def_id) in defs.iter().enumerate() {
        let x = pad_center.0 - 2.0 + 2.0 * (index % 3) as f32;
        let z = pad_center.1 - 1.0 + 2.0 * (index / 3) as f32;
        let id = world
            .borrow_mut()
            .spawn(simulation.entities_mut(), (x, z), def_id, team)
            .expect("spawn on the free pad");
        ids.push(id);
    }
    ids
}

/// The scripted command stream of the Crossing assault (spec §8): all
/// submitted before tick 1, applied at tick 1. Team A attack-moves all six
/// units to the chokepoint; Team B's strikers attack-move there too, its
/// skirmishers hold at (110, 100).
#[must_use]
pub fn crossing_assault_commands(
    ids_a: &[EntityId],
    ids_b: &[EntityId],
) -> Vec<CommandRequest<CommandPayload>> {
    let mut requests = Vec::with_capacity(12);
    let mut next = 1_u64;
    for &subject in ids_a {
        requests.push(CommandRequest {
            request_id: RequestId(next),
            actor: ActorId(TEAM_A),
            tick: 1,
            subject: Some(subject),
            payload: CommandPayload::AttackMove {
                destination: (100.0, 100.0),
            },
        });
        next += 1;
    }
    for (index, &subject) in ids_b.iter().enumerate() {
        let payload = if index < 3 {
            CommandPayload::AttackMove {
                destination: (100.0, 100.0),
            }
        } else {
            CommandPayload::Move {
                destination: (110.0, 100.0),
            }
        };
        requests.push(CommandRequest {
            request_id: RequestId(next),
            actor: ActorId(TEAM_B),
            tick: 1,
            subject: Some(subject),
            payload,
        });
        next += 1;
    }
    requests
}
