//! Integration tests for the movement/nav prototype on the "Crossing" map
//! (issue #12 acceptance criteria; `docs/vertical-slice-scenario.md` §2/§5/§9).

use phalanx_core::command::CommandPayload;
use phalanx_core::entity::EntityId;
use phalanx_core::replay::{ReplayRecorder, replay_recording};
use phalanx_core::simulation::{SimulationRunner, SystemId, TickRate};
use phalanx_navigation::{
    GROUND, Map, MovementSystem, MovementWorld, Obstacle, OrderStatus, RouteError, crossing_map,
};

const SEED: u64 = 0xC205_1A55;
const TICKS_PER_SECOND: u32 = 20;
const MOVEMENT_SYSTEM_ID: SystemId = SystemId("movement");

fn world(budget: u32) -> MovementWorld {
    MovementWorld::new(&crossing_map(), 1.0, 0.5, TICKS_PER_SECOND, budget)
}

/// Builds a runner with a movement system, spawning units and issuing move
/// orders before registration.
fn build_runner(
    budget: u32,
    spawns: &[(f32, f32)],
    orders: &[(usize, f32, f32)],
) -> (SimulationRunner<CommandPayload>, Vec<EntityId>) {
    let mut runner: SimulationRunner<CommandPayload> =
        SimulationRunner::new(TickRate::DEFAULT, SEED);
    let mut world = world(budget);
    let mut ids = Vec::new();
    for &position in spawns {
        let id = world
            .spawn(runner.simulation_mut().entities_mut(), position, GROUND)
            .expect("spawn on a free cell");
        ids.push(id);
    }
    for &(index, x, z) in orders {
        world.order_move(ids[index], (x, z)).expect("known unit");
    }
    runner
        .register_system(MovementSystem::new(world))
        .expect("unique system id");
    (runner, ids)
}

fn world_of(runner: &mut SimulationRunner<CommandPayload>) -> &MovementWorld {
    runner
        .system_mut(MOVEMENT_SYSTEM_ID)
        .expect("movement system registered")
        .as_any()
        .downcast_ref::<MovementSystem>()
        .expect("movement system type")
        .world()
}

fn world_of_mut(runner: &mut SimulationRunner<CommandPayload>) -> &mut MovementWorld {
    runner
        .system_mut(MOVEMENT_SYSTEM_ID)
        .expect("movement system registered")
        .as_any_mut()
        .downcast_mut::<MovementSystem>()
        .expect("movement system type")
        .world_mut()
}

/// Invariants checked every tick: units stay in bounds, never on a blocked
/// cell, and never closer than the separation distance.
fn assert_movement_invariants(runner: &mut SimulationRunner<CommandPayload>) {
    let world = world_of(runner);
    let grid = world.grid();
    let units: Vec<_> = world.units().values().collect();
    for unit in &units {
        assert!(
            grid.cell_of_checked(world.map(), unit.position).is_some(),
            "unit {:?} outside the map at {:?}",
            unit.id,
            unit.position
        );
        assert!(
            !grid.is_blocked(grid.cell_of(unit.position)),
            "unit {:?} inside a blocked cell at {:?}",
            unit.id,
            unit.position
        );
    }
    for i in 0..units.len() {
        for j in (i + 1)..units.len() {
            let dx = units[i].position.0 - units[j].position.0;
            let dz = units[i].position.1 - units[j].position.1;
            let distance = (dx * dx + dz * dz).sqrt();
            let separation = (units[i].profile.separation_m + units[j].profile.separation_m) / 2.0;
            // Separation is iterative float resolution: under congestion the
            // residual overlap stays within a few millimeters and never
            // accumulates (a unit is never pushed through another unit).
            if distance < separation - 5e-3 {
                panic!(
                    "units {:?} at {:?} and {:?} at {:?} overlap: {distance} < {separation}; statuses {:?}/{:?}; path lens {:?}/{:?}",
                    units[i].id,
                    units[i].position,
                    units[j].id,
                    units[j].position,
                    units[i].status,
                    units[j].status,
                    units[i].path.as_ref().map(|p| p.cells.len()),
                    units[j].path.as_ref().map(|p| p.cells.len()),
                );
            }
        }
    }
}

#[test]
fn single_unit_crosses_the_map_through_the_chokepoint_lane() {
    let (mut runner, ids) = build_runner(64, &[(20.0, 100.0)], &[(0, 180.0, 100.0)]);

    runner.advance_ticks(1).expect("planner tick");
    // The planned route crosses the wall band only inside the lane.
    {
        let world = world_of(&mut runner);
        let path = world
            .unit(ids[0])
            .and_then(|unit| unit.path.clone())
            .expect("path planned on the first tick");
        assert!(path.len() > 2);
        for cell in &path.cells {
            assert!(
                !world.grid().is_blocked(*cell),
                "path cell {cell:?} blocked"
            );
            if (55..=145).contains(&cell.0) {
                assert!(cell.1 > 62 && cell.1 < 138, "path leaves lane at {cell:?}");
            }
        }
    }

    // The unit arrives: 160 m at 0.2 m/tick plus the deterministic detour.
    let mut arrived_at = None;
    for tick in 2..=1600_u64 {
        runner.advance_ticks(1).expect("tick");
        assert_movement_invariants(&mut runner);
        if world_of(&mut runner).order_status(ids[0]) == Some(OrderStatus::Completed) {
            arrived_at = Some(tick);
            break;
        }
    }
    let arrived_at = arrived_at.expect("reachable destination must complete");
    assert!(arrived_at >= 800, "cannot beat 0.2 m/tick: {arrived_at}");
    let position = world_of(&mut runner).unit(ids[0]).unwrap().position;
    assert!((position.0 - 180.0).abs() <= 0.5 && (position.1 - 100.0).abs() <= 0.5);
}

#[test]
fn unreachable_destination_fails_with_an_explicit_observable_outcome() {
    // A walled pocket makes the goal cell unreachable.
    let map = Map {
        width_m: 20.0,
        depth_m: 20.0,
        obstacles: vec![
            Obstacle::new(10.0, 4.0, 12.0, 2.0),
            Obstacle::new(10.0, 16.0, 12.0, 2.0),
            Obstacle::new(4.0, 10.0, 2.0, 12.0),
            Obstacle::new(16.0, 10.0, 2.0, 12.0),
        ],
    };
    let mut runner: SimulationRunner<CommandPayload> =
        SimulationRunner::new(TickRate::DEFAULT, SEED);
    let mut world = MovementWorld::new(&map, 1.0, 0.5, TICKS_PER_SECOND, 8);
    let id = world
        .spawn(runner.simulation_mut().entities_mut(), (2.0, 2.0), GROUND)
        .unwrap();
    world.order_move(id, (10.0, 10.0)).unwrap();
    runner
        .register_system(MovementSystem::new(world))
        .expect("id");

    runner.advance_ticks(3).expect("planner runs within budget");
    assert_eq!(
        world_of(&mut runner).order_status(id),
        Some(OrderStatus::Failed(RouteError::Unreachable))
    );
    // The failure is counted in the planner stats (recorded in the hash).
    assert_eq!(world_of(&mut runner).planner().stats().unreachable, 1);
}

#[test]
fn chokepoint_scenario_does_not_deadlock() {
    // 12 units spawned in a block on the west pad, all ordered across the map
    // through the single lane between the walls.
    let spawns: Vec<(f32, f32)> = (0..4)
        .flat_map(|i| (0..3).map(move |j| (17.0 + 2.0 * i as f32, 97.0 + 3.0 * j as f32)))
        .collect();
    // Distinct destinations spread over the east pad: all 12 routes cross the
    // single lane between the walls.
    let orders: Vec<(usize, f32, f32)> = (0..spawns.len())
        .map(|i| (i, 172.0 + 3.0 * (i % 4) as f32, 94.0 + 4.0 * (i / 4) as f32))
        .collect();
    let (mut runner, ids) = build_runner(64, &spawns, &orders);

    let mut all_completed_at = None;
    for tick in 1..=2500_u64 {
        runner.advance_ticks(1).expect("tick");
        assert_movement_invariants(&mut runner);
        if ids
            .iter()
            .all(|id| world_of(&mut runner).order_status(*id) == Some(OrderStatus::Completed))
        {
            all_completed_at = Some(tick);
            break;
        }
    }
    assert!(
        all_completed_at.is_some(),
        "12 units must all cross the chokepoint without deadlock"
    );
    let stats = world_of(&mut runner).planner().stats();
    assert_eq!(stats.unreachable, 0);
    assert!(stats.processed >= 12, "every route was planned");
}

#[test]
fn identical_inputs_produce_identical_simulation_hashes() {
    let spawns = [(20.0, 100.0), (20.0, 103.0), (23.0, 100.0)];
    let orders = [(0, 100.0, 100.0), (1, 180.0, 100.0), (2, 100.0, 100.0)];
    let (mut a, _) = build_runner(64, &spawns, &orders);
    let (mut b, _) = build_runner(64, &spawns, &orders);
    a.advance_ticks(120).expect("advance");
    b.advance_ticks(120).expect("advance");
    assert_eq!(a.tick_hashes(), b.tick_hashes());
    assert_eq!(a.canonical_digest(), b.canonical_digest());
}

#[test]
fn replay_reproduces_identical_per_tick_hashes() {
    let spawns = [(20.0, 100.0), (23.0, 100.0), (20.0, 103.0)];
    let orders = [(0, 100.0, 100.0), (1, 180.0, 100.0), (2, 100.0, 100.0)];

    let mut recorder = ReplayRecorder::<CommandPayload>::new(TickRate::DEFAULT, SEED);
    {
        let runner = recorder.runner_mut();
        let mut world = world(64);
        let mut ids = Vec::new();
        for &position in &spawns {
            let id = world
                .spawn(runner.simulation_mut().entities_mut(), position, GROUND)
                .unwrap();
            ids.push(id);
        }
        for &(index, x, z) in &orders {
            world.order_move(ids[index], (x, z)).unwrap();
        }
        runner
            .register_system(MovementSystem::new(world))
            .expect("id");
    }
    recorder.advance_ticks(120).expect("record");
    let recording = recorder.finish();

    let setup = |runner: &mut SimulationRunner<CommandPayload>| {
        let mut world = world(64);
        let mut ids = Vec::new();
        for &position in &spawns {
            let id = world
                .spawn(runner.simulation_mut().entities_mut(), position, GROUND)
                .unwrap();
            ids.push(id);
        }
        for &(index, x, z) in &orders {
            world.order_move(ids[index], (x, z)).unwrap();
        }
        runner
            .register_system(MovementSystem::new(world))
            .expect("id");
    };
    let report = replay_recording(&recording, 120, setup).expect("replay");
    assert!(
        !report.diverged(),
        "first divergent tick: {:?}",
        report.first_divergent_tick()
    );
    assert_eq!(report.tick_hashes(), recording.tick_hashes());
}

#[test]
fn snapshot_restore_reproduces_subsequent_hashes() {
    let spawns = [(20.0, 100.0), (23.0, 100.0)];
    let orders = [(0, 100.0, 100.0), (1, 180.0, 100.0)];

    let (mut a, _) = build_runner(64, &spawns, &orders);
    a.advance_ticks(50).expect("advance to tick 50");
    let snapshot = a.snapshot();
    a.advance_ticks(50).expect("advance to tick 100");
    let hashes_a = a.tick_hashes()[50..].to_vec();

    let (mut b, _) = build_runner(64, &spawns, &orders);
    b.advance_ticks(80).expect("diverge to tick 80");
    b.restore_snapshot(&snapshot).expect("restore");
    assert_eq!(b.simulation().tick(), 50);
    assert_eq!(b.tick_hashes().len(), 50);
    b.advance_ticks(50).expect("advance to tick 100");
    assert_eq!(b.tick_hashes()[50..], hashes_a);
}

#[test]
fn planner_budget_limits_path_requests_per_tick() {
    let spawns = [(20.0, 100.0), (23.0, 100.0), (20.0, 103.0)];
    let orders = [(0, 100.0, 100.0), (1, 180.0, 100.0), (2, 90.0, 90.0)];
    let (mut runner, _) = build_runner(1, &spawns, &orders);
    runner.advance_ticks(1).expect("tick");
    let world = world_of(&mut runner);
    assert_eq!(world.planner().stats().processed, 1, "budget of 1/tick");
    assert_eq!(world.planner().pending(), 2);
    runner.advance_ticks(1).expect("tick");
    assert_eq!(world_of(&mut runner).planner().stats().processed, 2);
    assert_eq!(world_of(&mut runner).planner().pending(), 1);
}

#[test]
fn a_new_move_order_replaces_the_current_one() {
    let (mut runner, ids) = build_runner(64, &[(20.0, 100.0)], &[(0, 100.0, 100.0)]);
    world_of_mut(&mut runner)
        .order_move(ids[0], (150.0, 100.0))
        .expect("known unit");
    let world = world_of(&mut runner);
    assert_eq!(
        world.unit(ids[0]).unwrap().destination,
        Some((150.0, 100.0))
    );
    assert_eq!(world.planner().pending(), 1, "old request was replaced");
}

#[test]
fn navigation_diagnostics_are_machine_readable() {
    let (mut runner, ids) = build_runner(64, &[(20.0, 100.0)], &[(0, 180.0, 100.0)]);
    runner.advance_ticks(1).expect("tick");
    let world = world_of(&mut runner);
    let diagnostics = world.diagnostics();
    assert!(
        diagnostics.blocked_cells.contains(&(100, 60)),
        "wall N blocked"
    );
    assert_eq!(diagnostics.path_budget_per_tick, 64);
    assert_eq!(diagnostics.path_stats.processed, 1);
    assert_eq!(diagnostics.units.len(), 1);
    assert!(!diagnostics.units[0].path.is_empty(), "path is reported");
    assert!(!diagnostics.units[0].stuck);
    let bytes = diagnostics.to_bytes();
    assert!(!bytes.is_empty(), "canonical encoding is machine-readable");
    // The unit id is recoverable from the diagnostics.
    assert_eq!(diagnostics.units[0].id, ids[0]);
}
