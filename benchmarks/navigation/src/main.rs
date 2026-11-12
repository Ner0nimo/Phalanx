//! Movement/pathfinding baseline benchmark (issue #12 acceptance: a benchmark
//! baseline for 1k and 10k simple units).
//!
//! Runs a deterministic workload — N units on an open map with a few static
//! obstacles, all ordered a short move east — for a fixed number of ticks,
//! and reports wall-clock timings (median of repetitions) plus a determinism
//! check (identical final canonical digest across repetitions).
//!
//! Timings are reported for humans running this locally/on dedicated
//! hardware; CI intentionally does not run them on shared runners.

use std::time::Instant;

use phalanx_core::command::CommandPayload;
use phalanx_core::simulation::{SimulationRunner, TickRate};
use phalanx_navigation::{Map, MovementSystem, MovementWorld, Obstacle, GROUND};

const TICKS_PER_SECOND: u32 = 20;
const TICKS: u64 = 200;
const MOVE_DISTANCE_M: f32 = 40.0;
const PLANNER_BUDGET: u32 = 512;
const SEED: u64 = 0xAA11_CE5E;

fn bench_map() -> Map {
    // 800 m x 800 m open map; static obstacles are placed east of the
    // spawn/march area so the workload exercises movement + separation, not
    // obstacle detours.
    Map {
        width_m: 800.0,
        depth_m: 800.0,
        obstacles: vec![
            Obstacle::new(500.0, 200.0, 8.0, 120.0),
            Obstacle::new(500.0, 600.0, 8.0, 120.0),
            Obstacle::new(650.0, 400.0, 120.0, 8.0),
        ],
    }
}

/// Runs the workload once; returns (wall time, final checkpoint digest).
fn run_workload(units: u32) -> (std::time::Duration, phalanx_core::hash::Digest) {
    let map = bench_map();
    let mut runner: SimulationRunner<CommandPayload> =
        SimulationRunner::new(TickRate::DEFAULT, SEED);
    let mut world = MovementWorld::new(&map, 1.0, 0.5, TICKS_PER_SECOND, PLANNER_BUDGET);

    // Spawn a block of units on the west side (2 m spacing, 100 columns).
    let side = 100.min(units as usize);
    let mut ids = Vec::with_capacity(units as usize);
    for i in 0..units as usize {
        let x = 20.0 + 2.0 * (i % side) as f32;
        let z = 20.0 + 2.0 * (i / side) as f32;
        let id = world
            .spawn(runner.simulation_mut().entities_mut(), (x, z), GROUND)
            .expect("free spawn cell");
        ids.push(id);
    }
    // Order every unit a short move east (deterministic per-unit destination).
    for (i, id) in ids.iter().enumerate() {
        let x = 20.0 + 2.0 * (i % side) as f32 + MOVE_DISTANCE_M;
        let z = 20.0 + 2.0 * (i / side) as f32;
        world.order_move(*id, (x, z)).expect("known unit");
    }
    runner
        .register_system(MovementSystem::new(world))
        .expect("unique system id");

    let start = Instant::now();
    runner.advance_ticks(TICKS).expect("advance");
    let elapsed = start.elapsed();
    (elapsed, runner.canonical_digest())
}

fn median(mut values: Vec<std::time::Duration>) -> std::time::Duration {
    values.sort();
    values[values.len() / 2]
}

fn main() {
    println!("phalanx navigation baseline (issue #12)");
    println!("ticks={TICKS} tick_rate={TICKS_PER_SECOND}Hz move={MOVE_DISTANCE_M}m budget={PLANNER_BUDGET}/tick");
    for units in [1_000_u32, 10_000] {
        let mut runs = Vec::new();
        let mut digest = None;
        for rep in 0..3 {
            let (elapsed, d) = run_workload(units);
            if let Some(first) = &digest {
                assert_eq!(&d, first, "run {rep} diverged from run 0 (nondeterminism!)");
            } else {
                digest = Some(d);
            }
            runs.push(elapsed);
        }
        let med = median(runs);
        println!(
            "units={units:>5}: median {:?} total, {:.3} ms/tick, digest={}",
            med,
            med.as_secs_f64() * 1000.0 / TICKS as f64,
            digest.expect("digest")
        );
    }
    println!("determinism check: identical final digest across repetitions for each workload");
}
