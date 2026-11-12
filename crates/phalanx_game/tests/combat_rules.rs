//! Combat-rule tests for the first CPU-authoritative combat loop (issue #13;
//! `docs/vertical-slice-scenario.md` §6): target acquisition with the
//! deterministic tie-break, hold-and-fire with cooldown, damage math
//! (`max(1, damage - armor)`), destruction with entity release, stale
//! reference command failures, command replacement, and simultaneous fire in
//! ascending entity-ID order.

use std::collections::BTreeMap;

use phalanx_core::command::{ActorId, CommandPayload, CommandRequest, RequestId};
use phalanx_core::entity::EntityId;
use phalanx_core::simulation::{SimulationRunner, TickRate};
use phalanx_navigation::Map;

use phalanx_game::{
    BattleCommand, BattleEvent, BattleWorld, CommandFailReason, Roster, SharedBattleWorld, UnitDef,
    WeaponDef, slice_roster,
};

const SEED: u64 = 0x00C0_FFEE;
const TICK_RATE: u32 = 20;

fn empty_map(size_m: f32) -> Map {
    Map {
        width_m: size_m,
        depth_m: size_m,
        obstacles: Vec::new(),
    }
}

fn runner_with(world: BattleWorld) -> (SimulationRunner<CommandPayload>, SharedBattleWorld) {
    let mut runner = SimulationRunner::new(TickRate::DEFAULT, SEED);
    let shared = phalanx_game::register_battle_systems(&mut runner, world);
    (runner, shared)
}

fn world_for(roster: Roster) -> (SimulationRunner<CommandPayload>, SharedBattleWorld) {
    runner_with(BattleWorld::new(
        &empty_map(60.0),
        1.0,
        0.5,
        TICK_RATE,
        64,
        roster,
    ))
}

fn spawn(
    runner: &mut SimulationRunner<CommandPayload>,
    world: &SharedBattleWorld,
    position: (f32, f32),
    def_id: &str,
    team: u64,
) -> EntityId {
    world
        .borrow_mut()
        .spawn(
            runner.simulation_mut().entities_mut(),
            position,
            def_id,
            team,
        )
        .expect("spawn on a free cell")
}

fn submit(
    runner: &mut SimulationRunner<CommandPayload>,
    request_id: u64,
    actor: u64,
    tick: u64,
    subject: Option<EntityId>,
    payload: CommandPayload,
) {
    runner
        .submit_command(CommandRequest {
            request_id: RequestId(request_id),
            actor: ActorId(actor),
            tick,
            subject,
            payload,
        })
        .expect("command accepted");
}

fn def(
    id: &str,
    health: u32,
    armor: u32,
    range_m: f32,
    damage: u32,
    cooldown_ticks: u32,
) -> UnitDef {
    UnitDef {
        id: id.to_owned(),
        health,
        armor,
        speed_m_per_s: 4.0,
        sight_range_m: 40.0,
        radius_m: 0.5,
        weapon: WeaponDef {
            range_m,
            damage,
            cooldown_ticks,
        },
    }
}

fn roster_of(defs: Vec<UnitDef>) -> Roster {
    let mut units = BTreeMap::new();
    for unit in defs {
        units.insert(unit.id.clone(), unit);
    }
    Roster { units }
}

fn roster_with_extra(extra: Vec<UnitDef>) -> Roster {
    let mut roster = slice_roster();
    for unit in extra {
        roster.units.insert(unit.id.clone(), unit);
    }
    roster
}

#[test]
fn acquisition_picks_the_nearest_enemy_and_breaks_ties_by_lowest_entity_id() {
    let (mut runner, world) = world_for(slice_roster());
    let a = spawn(&mut runner, &world, (10.0, 10.0), "unit.striker", 1);
    let near = spawn(&mut runner, &world, (22.0, 10.0), "unit.striker", 2);
    let tie = spawn(&mut runner, &world, (10.0, 22.0), "unit.striker", 2);
    let far = spawn(&mut runner, &world, (25.0, 10.0), "unit.striker", 2);
    assert!(near < tie && tie < far);
    submit(
        &mut runner,
        1,
        1,
        1,
        Some(a),
        CommandPayload::AttackMove {
            destination: (10.0, 10.0),
        },
    );
    runner.step_tick().expect("tick 1");

    let world = world.borrow();
    assert_eq!(
        world.combat[&a].target,
        Some(near),
        "nearest enemy wins; the equal-distance pair (near, tie) goes to the lowest EntityId"
    );
    assert!(
        world.events.is_empty(),
        "all enemies are outside weapon range (dist >= 12 > 10), so nothing fires yet"
    );
}

#[test]
fn plain_move_units_do_not_acquire_targets() {
    let (mut runner, world) = world_for(slice_roster());
    let a = spawn(&mut runner, &world, (10.0, 10.0), "unit.striker", 1);
    let _b = spawn(&mut runner, &world, (15.0, 10.0), "unit.striker", 2);
    submit(
        &mut runner,
        1,
        1,
        1,
        Some(a),
        CommandPayload::Move {
            destination: (10.0, 10.0),
        },
    );
    runner.step_tick().expect("tick 1");

    let world = world.borrow();
    assert_eq!(
        world.combat[&a].target, None,
        "Move has no attack semantics"
    );
    assert!(
        world.events.is_empty(),
        "no damage without an attack command"
    );
}

#[test]
fn hold_and_fire_restarts_the_cooldown_each_shot() {
    let (mut runner, world) = world_for(slice_roster());
    let a = spawn(&mut runner, &world, (10.0, 10.0), "unit.striker", 1);
    let b = spawn(&mut runner, &world, (18.0, 10.0), "unit.striker", 2);
    submit(
        &mut runner,
        1,
        1,
        1,
        Some(a),
        CommandPayload::AttackMove {
            destination: (10.0, 10.0),
        },
    );
    runner.advance_ticks(21).expect("21 ticks");

    let world = world.borrow();
    let damage_ticks: Vec<u64> = world
        .events
        .iter()
        .filter_map(|event| match event {
            BattleEvent::DamageDealt {
                source,
                target,
                tick,
                ..
            } if *source == a && *target == b => Some(*tick),
            _ => None,
        })
        .collect();
    assert_eq!(
        damage_ticks,
        vec![1, 21],
        "striker cooldown is 20 ticks: shots at tick 1 and tick 21"
    );
    assert_eq!(world.combat[&b].health, 80, "two hits of 10 damage");
    assert_eq!(
        world.movement.unit(a).expect("unit").position,
        (10.0, 10.0),
        "in range: hold and fire, do not move"
    );
}

#[test]
fn damage_is_weapon_damage_minus_armor_with_a_minimum_of_one() {
    let roster = roster_of(vec![
        def("unit.hitter", 100, 0, 10.0, 10, 20),
        def("unit.tank", 100, 15, 10.0, 10, 20),
        def("unit.plate", 100, 5, 10.0, 10, 20),
    ]);
    let (mut runner, world) = world_for(roster);
    let a1 = spawn(&mut runner, &world, (10.0, 10.0), "unit.hitter", 1);
    let tank = spawn(&mut runner, &world, (15.0, 10.0), "unit.tank", 2);
    let a2 = spawn(&mut runner, &world, (10.0, 40.0), "unit.hitter", 1);
    let plate = spawn(&mut runner, &world, (15.0, 40.0), "unit.plate", 2);
    submit(
        &mut runner,
        1,
        1,
        1,
        Some(a1),
        CommandPayload::AttackMove {
            destination: (10.0, 10.0),
        },
    );
    submit(
        &mut runner,
        2,
        1,
        1,
        Some(a2),
        CommandPayload::AttackMove {
            destination: (10.0, 40.0),
        },
    );
    runner.step_tick().expect("tick 1");

    let world = world.borrow();
    let damage_of = |source: EntityId, target: EntityId| {
        world
            .events
            .iter()
            .find_map(|event| match event {
                BattleEvent::DamageDealt {
                    source: s,
                    target: t,
                    damage,
                    ..
                } if *s == source && *t == target => Some(*damage),
                _ => None,
            })
            .expect("damage event")
    };
    assert_eq!(damage_of(a1, tank), 1, "max(1, 10 - 15)");
    assert_eq!(damage_of(a2, plate), 5, "10 - 5");
    assert_eq!(world.combat[&tank].health, 99);
    assert_eq!(world.combat[&plate].health, 95);
}

#[test]
fn death_releases_the_entity_and_clears_stale_references() {
    let roster = roster_with_extra(vec![def("unit.glass", 10, 0, 10.0, 10, 20)]);
    let (mut runner, world) = world_for(roster);
    let a = spawn(&mut runner, &world, (10.0, 10.0), "unit.striker", 1);
    let b = spawn(&mut runner, &world, (15.0, 10.0), "unit.glass", 2);
    submit(
        &mut runner,
        1,
        1,
        1,
        Some(a),
        CommandPayload::AttackMove {
            destination: (10.0, 10.0),
        },
    );
    runner.step_tick().expect("tick 1");

    {
        let world = world.borrow();
        assert_eq!(
            world.events,
            vec![
                BattleEvent::DamageDealt {
                    source: a,
                    target: b,
                    damage: 10,
                    tick: 1,
                },
                BattleEvent::UnitDestroyed { unit: b, tick: 1 },
                BattleEvent::MatchOver { winner: 1, tick: 1 },
            ]
        );
        assert!(!world.combat.contains_key(&b));
        assert!(world.movement.unit(b).is_none());
        assert_eq!(world.combat[&a].target, None, "stale target cleared");
        assert!(
            !runner.simulation().entities().contains(b),
            "entity released (generation bumped)"
        );
        assert_eq!(runner.simulation().entities().alive_count(), 1);
    }

    // Commands referencing the destroyed entity fail with structured reasons;
    // they never resolve to a reused entity.
    submit(
        &mut runner,
        2,
        2,
        2,
        Some(b),
        CommandPayload::AttackMove {
            destination: (10.0, 10.0),
        },
    );
    submit(
        &mut runner,
        3,
        1,
        2,
        Some(a),
        CommandPayload::Attack { target: b },
    );
    runner.step_tick().expect("tick 2");
    let world = world.borrow();
    assert!(
        world.events.contains(&BattleEvent::CommandFailed {
            unit: b,
            reason: CommandFailReason::UnknownSubject,
            tick: 2,
        }),
        "dead subject: UnknownSubject"
    );
    assert!(
        world.events.contains(&BattleEvent::CommandFailed {
            unit: a,
            reason: CommandFailReason::InvalidTarget,
            tick: 2,
        }),
        "dead target: InvalidTarget"
    );
}

#[test]
fn attack_on_a_same_team_target_fails_with_invalid_target() {
    let (mut runner, world) = world_for(slice_roster());
    let a = spawn(&mut runner, &world, (10.0, 10.0), "unit.striker", 1);
    let c = spawn(&mut runner, &world, (12.0, 10.0), "unit.striker", 1);
    submit(
        &mut runner,
        1,
        1,
        1,
        Some(a),
        CommandPayload::Attack { target: c },
    );
    runner.step_tick().expect("tick 1");

    let world = world.borrow();
    assert_eq!(
        world.events,
        vec![BattleEvent::CommandFailed {
            unit: a,
            reason: CommandFailReason::InvalidTarget,
            tick: 1,
        }]
    );
    assert_eq!(world.combat[&a].command, None);
    assert_eq!(world.combat[&a].target, None);
}

#[test]
fn a_new_command_replaces_the_current_one() {
    let (mut runner, world) = world_for(slice_roster());
    let a = spawn(&mut runner, &world, (10.0, 10.0), "unit.striker", 1);
    let _b = spawn(&mut runner, &world, (50.0, 50.0), "unit.striker", 2);
    submit(
        &mut runner,
        1,
        1,
        1,
        Some(a),
        CommandPayload::AttackMove {
            destination: (40.0, 40.0),
        },
    );
    runner.step_tick().expect("tick 1");
    assert_eq!(
        world.borrow().combat[&a].command,
        Some(BattleCommand::AttackMove {
            destination: (40.0, 40.0)
        })
    );

    submit(
        &mut runner,
        2,
        1,
        2,
        Some(a),
        CommandPayload::Move {
            destination: (20.0, 20.0),
        },
    );
    runner.step_tick().expect("tick 2");
    let world = world.borrow();
    assert_eq!(
        world.combat[&a].command,
        Some(BattleCommand::Move {
            destination: (20.0, 20.0)
        }),
        "the new command cancels the previous one"
    );
    assert_eq!(
        world.combat[&a].target, None,
        "Move drops the attack target"
    );
}

#[test]
fn simultaneous_fire_applies_damage_in_ascending_entity_id_order() {
    let roster = roster_of(vec![def("unit.duelist", 15, 0, 50.0, 10, 20)]);
    let (mut runner, world) = world_for(roster);
    let a1 = spawn(&mut runner, &world, (10.0, 10.0), "unit.duelist", 1);
    let a2 = spawn(&mut runner, &world, (10.0, 20.0), "unit.duelist", 1);
    let b = spawn(&mut runner, &world, (12.0, 15.0), "unit.duelist", 2);
    assert!(a1 < a2);
    submit(
        &mut runner,
        1,
        1,
        1,
        Some(a1),
        CommandPayload::AttackMove {
            destination: (10.0, 10.0),
        },
    );
    submit(
        &mut runner,
        2,
        1,
        1,
        Some(a2),
        CommandPayload::AttackMove {
            destination: (10.0, 20.0),
        },
    );
    runner.step_tick().expect("tick 1");

    let world = world.borrow();
    assert_eq!(
        world.events,
        vec![
            BattleEvent::DamageDealt {
                source: a1,
                target: b,
                damage: 10,
                tick: 1,
            },
            BattleEvent::DamageDealt {
                source: a2,
                target: b,
                damage: 10,
                tick: 1,
            },
            BattleEvent::UnitDestroyed { unit: b, tick: 1 },
            BattleEvent::MatchOver { winner: 1, tick: 1 },
        ],
        "same-tick volley applies in ascending EntityId order (15 -> 5 -> 0)"
    );
}
