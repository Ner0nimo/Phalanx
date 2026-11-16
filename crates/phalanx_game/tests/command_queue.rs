//! Command queue tests (issue #94): appending commands to the queue,
//! advancing to the next queued command when the current one completes,
//! and cancelling all commands.

use phalanx_core::command::CommandPayload;
use phalanx_core::entity::EntityId;
use phalanx_core::simulation::{SimulationRunner, TickRate};
use phalanx_navigation::Map;

use phalanx_game::{BattleCommand, BattleWorld, SharedBattleWorld, slice_roster};

const SEED: u64 = 0x9EED_0001;
const TICK_RATE: u32 = 20;

fn empty_map(size_m: f32) -> Map {
    Map {
        width_m: size_m,
        depth_m: size_m,
        obstacles: Vec::new(),
    }
}

fn setup() -> (
    SimulationRunner<CommandPayload>,
    SharedBattleWorld,
    EntityId,
) {
    let mut runner = SimulationRunner::new(TickRate::DEFAULT, SEED);
    let world = BattleWorld::new(&empty_map(200.0), 1.0, 0.5, TICK_RATE, 64, slice_roster());
    let shared = phalanx_game::register_battle_systems(&mut runner, world);
    let id = shared
        .borrow_mut()
        .spawn(
            runner.simulation_mut().entities_mut(),
            (10.0, 10.0),
            "unit.striker",
            1,
        )
        .expect("spawn");
    (runner, shared, id)
}

#[test]
fn queued_command_activates_after_current_completes() {
    let (mut runner, world, id) = setup();
    // Issue a Move to a nearby destination (will complete quickly).
    world.borrow_mut().queue_command(
        id,
        BattleCommand::Move {
            destination: (15.0, 10.0),
        },
    );
    world.borrow_mut().queue_command(
        id,
        BattleCommand::Move {
            destination: (50.0, 50.0),
        },
    );
    // Activate the first queued command manually (simulating apply_commands).
    {
        let mut world = world.borrow_mut();
        world.advance_command_queue(id);
    }
    {
        let world_ref = world.borrow();
        let state = world_ref.combat.get(&id).unwrap();
        assert!(
            matches!(
                state.command,
                Some(BattleCommand::Move { destination }) if destination == (15.0, 10.0)
            ),
            "first queued command should be active"
        );
        assert_eq!(
            state.command_queue.len(),
            1,
            "one command should remain queued"
        );
    }

    // Step until the first move completes (5 m at 0.2 m/tick = 25 ticks + planning).
    runner.advance_ticks(60).expect("advance");
    // After completion, the second queued command should be active.
    let world_ref = world.borrow();
    let state = &world_ref.combat.get(&id).unwrap();
    assert!(
        matches!(
            state.command,
            Some(BattleCommand::Move { destination }) if destination == (50.0, 50.0)
        ),
        "second queued command should activate after first completes, got {:?}",
        state.command
    );
    assert!(
        state.command_queue.is_empty(),
        "queue should be empty after draining"
    );
}

#[test]
fn cancel_commands_clears_active_and_queue() {
    let (_runner, world, id) = setup();
    {
        let mut world = world.borrow_mut();
        world.queue_command(
            id,
            BattleCommand::Move {
                destination: (15.0, 10.0),
            },
        );
        world.queue_command(
            id,
            BattleCommand::Move {
                destination: (50.0, 50.0),
            },
        );
        world.advance_command_queue(id);
    }
    assert!(world.borrow().combat.get(&id).unwrap().command.is_some());
    assert_eq!(
        world.borrow().combat.get(&id).unwrap().command_queue.len(),
        1
    );

    world.borrow_mut().cancel_commands(id);
    let world_ref = world.borrow();
    let state = &world_ref.combat.get(&id).unwrap();
    assert!(state.command.is_none(), "active command cleared");
    assert!(state.command_queue.is_empty(), "queue cleared");
    assert!(state.target.is_none(), "target cleared");
}
