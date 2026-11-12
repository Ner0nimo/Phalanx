//! Issue #9 spike: bevy_ecs vs hecs on representative RTS workloads (input to
//! ADR-0003).
//!
//! This binary is deliberately NOT part of the production workspace (see the
//! `[workspace]` table in Cargo.toml): CI gates do not build it. Wall-clock
//! timing and a counting global allocator are fine here because this measures
//! the ECS libraries, not the authoritative simulation. Both candidates run
//! the same seed, entity counts, workload, and release profile.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bevy_ecs::prelude::{Component, Entity as BevyEntity, Without, World as BevyWorld};

#[derive(Component, Clone, Copy)]
struct Position {
    x: f32,
    y: f32,
    z: f32,
}

#[derive(Component, Clone, Copy)]
struct Velocity {
    x: f32,
    y: f32,
    z: f32,
}

#[derive(Component, Clone, Copy)]
struct Health {
    current: f32,
    max: f32,
}

#[derive(Component, Clone, Copy)]
struct Attack {
    damage: f32,
    range: f32,
}

#[derive(Component, Clone, Copy)]
struct Building;

#[derive(Component, Clone, Copy)]
struct Armor;

// --- counting global allocator (allocation count + live/peak bytes) ---

static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
static CUR_BYTES: AtomicU64 = AtomicU64::new(0);
static PEAK_BYTES: AtomicU64 = AtomicU64::new(0);

struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let size = layout.size() as u64;
            let current = CUR_BYTES.fetch_add(size, Ordering::Relaxed) + size;
            PEAK_BYTES.fetch_max(current, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        CUR_BYTES.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

// --- deterministic RNG for workload data (SplitMix64) ---

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn f32(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
}

const DT: f32 = 0.05;

type UnitBundle = (Position, Velocity, Health, Attack);
type ArmoredBundle = (Position, Velocity, Health, Attack, Armor);
type BuildingBundle = (Position, Health, Building);

fn make_bundles(n: usize, rng: &mut Rng) -> (Vec<UnitBundle>, Vec<ArmoredBundle>, Vec<BuildingBundle>) {
    let mut units = Vec::new();
    let mut armored = Vec::new();
    let mut buildings = Vec::new();
    for i in 0..n {
        let pos = Position {
            x: rng.f32() * 4000.0,
            y: rng.f32() * 100.0,
            z: rng.f32() * 4000.0,
        };
        let health = Health {
            current: rng.f32() * 100.0,
            max: 100.0,
        };
        match i % 10 {
            0 => buildings.push((pos, health, Building)),
            1 => armored.push((
                pos,
                Velocity {
                    x: rng.f32() * 4.0 - 2.0,
                    y: 0.0,
                    z: rng.f32() * 4.0 - 2.0,
                },
                health,
                Attack {
                    damage: rng.f32() * 20.0,
                    range: 10.0 + rng.f32() * 90.0,
                },
                Armor,
            )),
            _ => units.push((
                pos,
                Velocity {
                    x: rng.f32() * 4.0 - 2.0,
                    y: 0.0,
                    z: rng.f32() * 4.0 - 2.0,
                },
                health,
                Attack {
                    damage: rng.f32() * 20.0,
                    range: 10.0 + rng.f32() * 90.0,
                },
            )),
        }
    }
    (units, armored, buildings)
}

fn unit_bundle(rng: &mut Rng) -> UnitBundle {
    (
        Position {
            x: rng.f32() * 4000.0,
            y: rng.f32() * 100.0,
            z: rng.f32() * 4000.0,
        },
        Velocity {
            x: rng.f32() * 4.0 - 2.0,
            y: 0.0,
            z: rng.f32() * 4.0 - 2.0,
        },
        Health {
            current: rng.f32() * 100.0,
            max: 100.0,
        },
        Attack {
            damage: rng.f32() * 20.0,
            range: 10.0 + rng.f32() * 90.0,
        },
    )
}

fn step(pos: &mut Position, vel: &Velocity) {
    pos.x += vel.x * DT;
    pos.y += vel.y * DT;
    pos.z += vel.z * DT;
}

fn report(bench: &str, candidate: &str, entities: usize, rounds: usize, elapsed: Duration, checksum: f64) {
    let secs = elapsed.as_secs_f64();
    let per_round = if rounds > 0 { secs / rounds as f64 } else { 0.0 };
    let throughput = if secs > 0.0 {
        entities as f64 * rounds as f64 / secs
    } else {
        0.0
    };
    println!(
        "RESULT candidate={candidate} entities={entities} bench={bench} rounds={rounds} time_ms={:.3} per_round_ms={:.4} throughput_eps={:.0} checksum={checksum:.4}",
        secs * 1000.0,
        per_round * 1000.0,
        throughput,
    );
}

fn mem(phase: &str, candidate: &str) {
    println!(
        "MEM candidate={candidate} phase={phase} alloc_calls={} current_bytes={} peak_bytes={}",
        ALLOC_CALLS.load(Ordering::Relaxed),
        CUR_BYTES.load(Ordering::Relaxed),
        PEAK_BYTES.load(Ordering::Relaxed),
    );
}

// --- bevy_ecs path ---

fn run_bevy(n: usize, rounds: usize, seed: u64) {
    let candidate = "bevy_ecs-0.20.1";
    let mut rng = Rng(seed);
    let (units, armored, buildings) = make_bundles(n, &mut rng);
    let mut world = BevyWorld::new();

    let t = Instant::now();
    world.spawn_batch(units);
    world.spawn_batch(armored);
    world.spawn_batch(buildings);
    report("spawn", candidate, n, 1, t.elapsed(), n as f64);
    mem("after_spawn", candidate);

    // Deterministic batch update: integrate positions from velocities.
    for _ in 0..2 {
        let mut q = world.query::<(&mut Position, &Velocity)>();
        for (mut pos, vel) in q.iter_mut(&mut world) {
            step(&mut pos, vel);
        }
    }
    let t = Instant::now();
    let mut checksum = 0.0f64;
    for _ in 0..rounds {
        let mut q = world.query::<(&mut Position, &Velocity)>();
        for (mut pos, vel) in q.iter_mut(&mut world) {
            step(&mut pos, vel);
            checksum += f64::from(pos.x + pos.z);
        }
    }
    report("iterate_update", candidate, n, rounds, t.elapsed(), checksum);

    // Read-only movement query.
    let mut q = world.query::<(&Position, &Velocity)>();
    let t = Instant::now();
    let mut checksum = 0.0f64;
    for _ in 0..rounds {
        for (pos, vel) in q.iter(&world) {
            checksum += f64::from(pos.x + vel.x);
        }
    }
    report("query_movement_read", candidate, n, rounds, t.elapsed(), checksum);

    // Combat query: wounded attackers.
    let mut q = world.query::<(&Position, &Health, &Attack)>();
    let t = Instant::now();
    let mut checksum = 0.0f64;
    for _ in 0..rounds {
        for (_pos, health, attack) in q.iter(&world) {
            if health.current < health.max * 0.5 {
                checksum += f64::from(attack.damage + attack.range);
            }
        }
    }
    report("query_combat_wounded", candidate, n, rounds, t.elapsed(), checksum);

    // Mixed filtered query: mobile non-buildings.
    let mut q = world.query_filtered::<(&Position, &Velocity), Without<Building>>();
    let t = Instant::now();
    let mut checksum = 0.0f64;
    for _ in 0..rounds {
        for (pos, vel) in q.iter(&world) {
            checksum += f64::from(pos.z - vel.z);
        }
    }
    report("query_movement_no_building", candidate, n, rounds, t.elapsed(), checksum);

    // Churn: despawn + respawn ~1% per round.
    let t = Instant::now();
    for _ in 0..rounds {
        let mut ids: Vec<BevyEntity> = Vec::new();
        {
            let mut q = world.query::<(BevyEntity, &Position)>();
            for (entity, _pos) in q.iter(&world) {
                if rng.f32() < 0.01 {
                    ids.push(entity);
                }
            }
        }
        for entity in &ids {
            world.despawn(*entity);
        }
        let fresh: Vec<UnitBundle> = (0..ids.len()).map(|_| unit_bundle(&mut rng)).collect();
        world.spawn_batch(fresh);
    }
    report("churn_spawn_despawn", candidate, n, rounds, t.elapsed(), 0.0);
    mem("after_churn", candidate);
}

// --- hecs path ---

fn run_hecs(n: usize, rounds: usize, seed: u64) {
    let candidate = "hecs-0.11.2";
    let mut rng = Rng(seed);
    let (units, armored, buildings) = make_bundles(n, &mut rng);
    let mut world = hecs::World::new();

    let t = Instant::now();
    world.spawn_batch(units);
    world.spawn_batch(armored);
    world.spawn_batch(buildings);
    report("spawn", candidate, n, 1, t.elapsed(), n as f64);
    mem("after_spawn", candidate);

    // Deterministic batch update.
    for _ in 0..2 {
        for (pos, vel) in world.query_mut::<(&mut Position, &Velocity)>() {
            step(pos, vel);
        }
    }
    let t = Instant::now();
    let mut checksum = 0.0f64;
    for _ in 0..rounds {
        for (pos, vel) in world.query_mut::<(&mut Position, &Velocity)>() {
            step(pos, vel);
            checksum += f64::from(pos.x + pos.z);
        }
    }
    report("iterate_update", candidate, n, rounds, t.elapsed(), checksum);

    // Read-only movement query.
    let t = Instant::now();
    let mut checksum = 0.0f64;
    for _ in 0..rounds {
        let mut q = world.query::<(&Position, &Velocity)>();
        for (pos, vel) in q.iter() {
            checksum += f64::from(pos.x + vel.x);
        }
    }
    report("query_movement_read", candidate, n, rounds, t.elapsed(), checksum);

    // Combat query: wounded attackers.
    let t = Instant::now();
    let mut checksum = 0.0f64;
    for _ in 0..rounds {
        let mut q = world.query::<(&Position, &Health, &Attack)>();
        for (_pos, health, attack) in q.iter() {
            if health.current < health.max * 0.5 {
                checksum += f64::from(attack.damage + attack.range);
            }
        }
    }
    report("query_combat_wounded", candidate, n, rounds, t.elapsed(), checksum);

    // Mixed filtered query: mobile non-buildings.
    let t = Instant::now();
    let mut checksum = 0.0f64;
    for _ in 0..rounds {
        let mut q = world.query::<(&Position, &Velocity)>().without::<&Building>();
        for (pos, vel) in q.iter() {
            checksum += f64::from(pos.z - vel.z);
        }
    }
    report("query_movement_no_building", candidate, n, rounds, t.elapsed(), checksum);

    // Churn: despawn + respawn ~1% per round.
    let t = Instant::now();
    for _ in 0..rounds {
        let ids: Vec<hecs::Entity> = {
            let mut q = world.query::<(hecs::Entity, &Position)>();
            q.iter()
                .filter(|_| rng.f32() < 0.01)
                .map(|(entity, _pos)| entity)
                .collect()
        };

        for entity in &ids {
            let _ = world.despawn(*entity);
        }
        let fresh: Vec<UnitBundle> = (0..ids.len()).map(|_| unit_bundle(&mut rng)).collect();
        world.spawn_batch(fresh);
    }
    report("churn_spawn_despawn", candidate, n, rounds, t.elapsed(), 0.0);
    mem("after_churn", candidate);
}

fn main() {
    let mut candidate = String::from("bevy");
    let mut entities = String::from("10000,50000,100000");
    let mut rounds = 20usize;
    let mut seed = 42u64;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--candidate" => {
                i += 1;
                candidate = args[i].clone();
            }
            "--entities" => {
                i += 1;
                entities = args[i].clone();
            }
            "--rounds" => {
                i += 1;
                rounds = args[i].parse().expect("rounds must be an integer");
            }
            "--seed" => {
                i += 1;
                seed = args[i].parse().expect("seed must be an integer");
            }
            other => panic!("unknown argument: {other}"),
        }
        i += 1;
    }

    let rustc = std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned());
    println!(
        "ENV rustc=\"{rustc}\" os={} arch={} cores={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        available_parallelism(),
    );

    for part in entities.split(',') {
        let n: usize = part.trim().parse().expect("entity count must be an integer");
        match candidate.as_str() {
            "bevy" => run_bevy(n, rounds, seed),
            "hecs" => run_hecs(n, rounds, seed),
            other => panic!("unknown candidate: {other} (expected bevy|hecs)"),
        }
    }
}

fn available_parallelism() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}
