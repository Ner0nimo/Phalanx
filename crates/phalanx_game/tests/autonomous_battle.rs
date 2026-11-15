//! End-to-end autonomous battle test (issue #64): with the autonomous
//! stance enabled, two teams spawned 40 m apart on the Crossing map and NO
//! per-unit commands submitted detect each other, navigate toward each
//! other, engage, and fight until `MatchOver` — all through the real
//! `SimulationRunner`. Also verifies that destroyed units disappear from the
//! render snapshot, that the run is deterministic, and that a non-autonomous
//! world (control) never acquires targets without commands.

use phalanx_core::command::CommandPayload;
use phalanx_core::simulation::{SimulationRunner, TickRate};
use phalanx_game::events::BattleEvent;
use phalanx_game::scenario::{
    AUTONOMOUS_PAD_A, AUTONOMOUS_PAD_B, SCENARIO_SEED, SPAWN_PAD_A, SPAWN_PAD_B, TEAM_A, TEAM_B,
    setup_autonomous_battle, setup_battle, spawn_team,
};
use phalanx_game::{SharedBattleWorld, extract_snapshot};
use phalanx_navigation::crossing_map;

/// Acceptance budget: the autonomous battle ends well within 2000 ticks.
const MAX_TICKS: u64 = 2000;

/// Builds the autonomous battle: both teams spawned on the close pads, zero
/// commands submitted.
fn autonomous_battle() -> (SimulationRunner<CommandPayload>, SharedBattleWorld) {
    let mut runner = SimulationRunner::new(TickRate::DEFAULT, SCENARIO_SEED);
    let world = setup_autonomous_battle(&mut runner);
    spawn_team(runner.simulation_mut(), &world, TEAM_A, AUTONOMOUS_PAD_A);
    spawn_team(runner.simulation_mut(), &world, TEAM_B, AUTONOMOUS_PAD_B);
    (runner, world)
}

#[test]
fn autonomous_units_detect_approach_and_engage_without_any_commands() {
    let (mut runner, world) = autonomous_battle();
    assert!(
        runner.simulation().applied_command_count() == 0,
        "the battle runs without individual unit commands"
    );
    let start: Vec<(u32, [f32; 2])> = {
        let world = world.borrow();
        world
            .movement
            .units()
            .iter()
            .map(|(id, unit)| (id.index(), [unit.position.0, unit.position.1]))
            .collect()
    };
    // Advance a fixed horizon — no commands are ever submitted.
    runner.advance_ticks(100).expect("ticks");
    let world = world.borrow();
    let first_damage_tick = world.events.iter().find_map(|event| match event {
        BattleEvent::DamageDealt { tick, .. } => Some(*tick),
        _ => None,
    });
    let first_damage_tick = first_damage_tick.expect("units autonomously engaged");
    // Units actually moved toward each other (not just stood at spawn).
    let moved = world
        .movement
        .units()
        .iter()
        .zip(&start)
        .filter(|((id, unit), (start_index, start_pos))| {
            id.index() == *start_index
                && ((unit.position.0 - start_pos[0]).powi(2)
                    + (unit.position.1 - start_pos[1]).powi(2))
                .sqrt()
                    > 1.0
        })
        .count();
    assert!(
        moved > 0,
        "units navigated toward enemies on their own (first damage at tick {first_damage_tick})"
    );
    // Targets were acquired autonomously (nearest enemy within sight).
    assert!(
        world.combat.values().any(|state| state.target.is_some()),
        "units acquired enemy targets without commands"
    );
}

#[test]
fn autonomous_battle_reaches_match_over_and_destroyed_units_vanish() {
    let (mut runner, world) = autonomous_battle();
    let mut ticks = 0_u64;
    while world.borrow().match_over.is_none() && ticks < MAX_TICKS {
        runner.advance_ticks(1).expect("tick");
        ticks += 1;
    }
    let world = world.borrow();
    let winner = world.match_over.expect("battle reaches a terminal state");
    assert!(winner == TEAM_A || winner == TEAM_B);
    let match_over_event = world.events.iter().find_map(|event| match event {
        BattleEvent::MatchOver { winner, tick } => Some((*winner, *tick)),
        _ => None,
    });
    assert_eq!(match_over_event, Some((winner, ticks)));
    // The losing team is wiped out; only the winner's units remain.
    let living = world.living_per_team();
    assert_eq!(living.len(), 1, "exactly one team remains");
    let survivors = living[&winner];
    assert!((1..=6).contains(&survivors));
    assert!(world.combat.values().all(|state| state.team == winner));
    // Destroyed units are removed from both the combat state and the
    // movement layer, and disappear from the render snapshot.
    let destroyed: Vec<_> = world
        .events
        .iter()
        .filter_map(|event| match event {
            BattleEvent::UnitDestroyed { unit, .. } => Some(*unit),
            _ => None,
        })
        .collect();
    assert_eq!(destroyed.len(), 12 - survivors);
    for unit in &destroyed {
        assert_eq!(world.combat.get(unit), None, "destroyed unit removed");
        assert!(world.movement.unit(*unit).is_none());
    }
    let snapshot = extract_snapshot(&world, &crossing_map(), ticks);
    assert_eq!(snapshot.entities.len(), survivors);
    assert!(snapshot.entities.iter().all(|entity| entity.team == winner));
    assert_eq!(snapshot.obstacles.len(), 4);
}

#[test]
fn autonomous_battle_is_deterministic_for_a_fixed_seed() {
    let run = |seed| {
        let (mut runner, world) = {
            let mut runner = SimulationRunner::new(TickRate::DEFAULT, seed);
            let world = setup_autonomous_battle(&mut runner);
            spawn_team(runner.simulation_mut(), &world, TEAM_A, AUTONOMOUS_PAD_A);
            spawn_team(runner.simulation_mut(), &world, TEAM_B, AUTONOMOUS_PAD_B);
            (runner, world)
        };
        let mut ticks = 0_u64;
        while world.borrow().match_over.is_none() && ticks < MAX_TICKS {
            runner.advance_ticks(1).expect("tick");
            ticks += 1;
        }
        (runner.canonical_digest(), world.borrow().match_over, ticks)
    };
    let first = run(SCENARIO_SEED);
    let second = run(SCENARIO_SEED);
    assert_eq!(first, second, "same seed = identical outcome and digest");
    // The battle is largely RNG-free: other seeds terminate too.
    let other = run(SCENARIO_SEED + 1);
    assert!(other.1.is_some(), "battle still terminates for other seeds");
}

#[test]
fn control_non_autonomous_world_never_engages_without_commands() {
    let mut runner = SimulationRunner::new(TickRate::DEFAULT, SCENARIO_SEED);
    let world = setup_battle(&mut runner);
    assert!(!world.borrow().autonomous);
    spawn_team(runner.simulation_mut(), &world, TEAM_A, SPAWN_PAD_A);
    spawn_team(runner.simulation_mut(), &world, TEAM_B, SPAWN_PAD_B);
    runner.advance_ticks(100).expect("ticks");
    let world = world.borrow();
    assert!(world.events.is_empty(), "no combat without commands");
    assert!(world.match_over.is_none());
    assert!(
        world.combat.values().all(|state| state.target.is_none()),
        "no target acquisition in the default stance"
    );
    assert_eq!(world.combat.len(), 12);
}
