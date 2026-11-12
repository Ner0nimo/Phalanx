//! End-to-end "Crossing assault" acceptance test (issue #13;
//! `docs/vertical-slice-scenario.md` §8–§9): the scripted command stream on
//! the Crossing map ends with `MatchOver { winner: Team A }` within 2000
//! ticks, first contact near the rule-derived tick, the recorded run replays
//! with identical per-tick hashes, an altered command diverges at tick 1,
//! two independent runs are bit-identical, and a snapshot/restore round trip
//! continues the run identically (canonical system-state hooks, issue #34).

use phalanx_core::command::{CommandEnvelope, CommandPayload, ENVELOPE_VERSION};
use phalanx_core::entity::EntityId;
use phalanx_core::replay::{ReplayRecorder, ReplayRecording, replay_recording};
use phalanx_core::simulation::{SimulationRunner, TickRate};
use phalanx_game::events::BattleEvent;
use phalanx_game::scenario::{
    SCENARIO_SEED, TEAM_A, TEAM_B, crossing_assault_commands, setup_battle, spawn_team,
};
use phalanx_game::{SharedBattleWorld, slice_roster};

/// Acceptance budget from the spec §8: the match ends no later than tick 2000.
const MAX_TICKS: u64 = 2000;
const PAD_A: (f32, f32) = (20.0, 100.0);
const PAD_B: (f32, f32) = (180.0, 100.0);

struct Assault {
    world: SharedBattleWorld,
    recording: ReplayRecording<CommandPayload>,
    ticks: u64,
    ids_a: Vec<EntityId>,
    ids_b: Vec<EntityId>,
}

/// Records the scripted scenario: spawn both teams, submit all twelve
/// commands for tick 1, then advance until `MatchOver` (bounded by
/// `MAX_TICKS`).
fn record_assault() -> Assault {
    let mut recorder = ReplayRecorder::new(TickRate::DEFAULT, SCENARIO_SEED);
    let world = setup_battle(recorder.runner_mut());
    let ids_a = spawn_team(
        recorder.runner_mut().simulation_mut(),
        &world,
        TEAM_A,
        PAD_A,
    );
    let ids_b = spawn_team(
        recorder.runner_mut().simulation_mut(),
        &world,
        TEAM_B,
        PAD_B,
    );
    for request in crossing_assault_commands(&ids_a, &ids_b) {
        recorder.submit_command(request).expect("command accepted");
    }
    let mut ticks = 0_u64;
    while world.borrow().match_over.is_none() && ticks < MAX_TICKS {
        recorder.step_tick().expect("tick");
        ticks += 1;
    }
    Assault {
        world: world.clone(),
        recording: recorder.finish(),
        ticks,
        ids_a,
        ids_b,
    }
}

/// The replay setup: identical registration and spawn order, so entity IDs
/// match the recorded commands.
fn replay_setup(runner: &mut SimulationRunner<CommandPayload>) {
    let world = setup_battle(runner);
    spawn_team(runner.simulation_mut(), &world, TEAM_A, PAD_A);
    spawn_team(runner.simulation_mut(), &world, TEAM_B, PAD_B);
}

#[test]
fn crossing_assault_team_a_wins_within_2000_ticks() {
    let assault = record_assault();
    let world = assault.world.borrow();

    assert!(
        assault.ticks <= MAX_TICKS,
        "match ended at tick {} (budget {MAX_TICKS})",
        assault.ticks
    );
    assert_eq!(world.match_over, Some(TEAM_A));

    let destroyed: Vec<EntityId> = world
        .events
        .iter()
        .filter_map(|event| match event {
            BattleEvent::UnitDestroyed { unit, .. } => Some(*unit),
            _ => None,
        })
        .collect();
    for id in &assault.ids_b {
        assert!(
            destroyed.contains(id),
            "every Team B unit is destroyed (spec §9.3)"
        );
    }
    assert_eq!(
        world.events.iter().find_map(|event| match event {
            BattleEvent::MatchOver { winner, tick } => Some((*winner, *tick)),
            _ => None,
        }),
        Some((TEAM_A, assault.ticks))
    );
    assert!(
        world.combat.keys().any(|id| assault.ids_a.contains(id)),
        "the winning team still fields living units"
    );

    // First contact: derived from the rules, not hardcoded. The spawn pads
    // are 160 m apart and both striker groups close at 0.2 m/tick each, so
    // the first striker pair is within weapon range (10 m) after
    // (160 - 10) / 0.4 = 375 ticks (spec §8). The fixed spawn layout offsets
    // individual units by up to 2 m per axis (earlier contact) and the
    // chokepoint rocks/separation only delay it, so the recorded first
    // `DamageDealt` must sit in a window around the derived value.
    let roster = slice_roster();
    let striker = roster.get("unit.striker").expect("striker def");
    let meters_per_tick = striker.speed_m_per_s / TickRate::DEFAULT.ticks_per_second() as f32;
    let expected_contact = (160.0 - striker.weapon.range_m) / (2.0 * meters_per_tick);
    assert_eq!(expected_contact, 375.0, "worked example from spec §8");
    let first_damage = world
        .events
        .iter()
        .find_map(|event| match event {
            BattleEvent::DamageDealt { tick, .. } => Some(*tick),
            _ => None,
        })
        .expect("the armies do fight");
    assert!(
        (300..=450).contains(&first_damage),
        "first DamageDealt at tick {first_damage}, rule-derived estimate {expected_contact} \
         (spawn-layout offsets and deterministic chokepoint congestion shift it)"
    );
}

#[test]
fn replay_reproduces_identical_per_tick_hashes() {
    let assault = record_assault();
    let report =
        replay_recording(&assault.recording, assault.ticks, replay_setup).expect("replay succeeds");
    assert_eq!(report.ticks_executed(), assault.ticks);
    assert_eq!(
        report.first_divergent_tick(),
        None,
        "same-build replay is hash-identical"
    );
    assert_eq!(report.tick_hashes(), assault.recording.tick_hashes());
}

#[test]
fn an_altered_command_diverges_at_its_target_tick() {
    let assault = record_assault();
    let mut altered = assault.recording.clone();
    let envelope = &altered.commands()[0];
    assert_eq!(envelope.tick(), 1);
    altered.commands_mut()[0] = CommandEnvelope::new(
        ENVELOPE_VERSION,
        envelope.request_id(),
        envelope.command_id(),
        envelope.actor(),
        envelope.tick(),
        envelope.subject(),
        CommandPayload::AttackMove {
            destination: (100.0, 120.0),
        },
    )
    .expect("current envelope version");

    let report = replay_recording(&altered, assault.ticks, replay_setup).expect("replay succeeds");
    assert!(report.diverged(), "altering one command must be detected");
    assert_eq!(
        report.first_divergent_tick(),
        Some(1),
        "the pending command queue is authoritative state from submission"
    );
}

#[test]
fn two_independent_runs_are_bit_identical() {
    let first = record_assault();
    let second = record_assault();
    assert_eq!(first.ticks, second.ticks);
    assert_eq!(
        first.recording.tick_hashes(),
        second.recording.tick_hashes()
    );
    assert_eq!(first.world.borrow().events, second.world.borrow().events);
    assert_eq!(first.world.borrow().combat, second.world.borrow().combat);
}

#[test]
fn snapshot_restore_continues_the_run_identically() {
    let reference = record_assault();

    let mut runner = SimulationRunner::new(TickRate::DEFAULT, SCENARIO_SEED);
    let world = setup_battle(&mut runner);
    let ids_a = spawn_team(runner.simulation_mut(), &world, TEAM_A, PAD_A);
    let ids_b = spawn_team(runner.simulation_mut(), &world, TEAM_B, PAD_B);
    for request in crossing_assault_commands(&ids_a, &ids_b) {
        runner.submit_command(request).expect("command accepted");
    }
    runner.advance_ticks(100).expect("100 ticks");
    let snapshot = runner.snapshot();
    let digest_at_100 = runner.canonical_digest();

    runner.advance_ticks(50).expect("50 more ticks");
    runner.restore_snapshot(&snapshot).expect("restore");
    assert_eq!(
        runner.canonical_digest(),
        digest_at_100,
        "the battle world's canonical state round-trips exactly"
    );
    assert_eq!(runner.tick_hashes().len(), 100);

    let mut ticks = 100_u64;
    while world.borrow().match_over.is_none() && ticks < MAX_TICKS {
        runner.step_tick().expect("tick");
        ticks += 1;
    }
    assert_eq!(ticks, reference.ticks, "same deterministic outcome");
    assert_eq!(
        runner.tick_hashes(),
        &reference.recording.tick_hashes()[..ticks as usize],
        "hashes after the restore match the uninterrupted run"
    );
}
