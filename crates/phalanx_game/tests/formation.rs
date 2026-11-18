//! Integration tests for squads, formation slots, and group orders
//! (issue #110; `RtsEngineDoc.md` §2.4, §7.3, §18 M2): membership lifecycle
//! without stale references, deterministic slot assignment, group
//! Move/AttackMove/Defend with observable completion, chokepoint traversal
//! without entering obstacles, replay/snapshot coverage, and render-snapshot
//! exposure.

use phalanx_core::command::{ActorId, CommandPayload, CommandRequest, RequestId, SessionId};
use phalanx_core::entity::{EntityId, SquadId};
use phalanx_core::hash::canonical_digest;
use phalanx_core::replay::{ReplayRecorder, replay_recording};
use phalanx_core::simulation::{SimulationRunner, TickRate};
use phalanx_game::events::{BattleEvent, CommandFailReason};
use phalanx_game::formation::{FORMATION_SPACING_M, FormationTemplate};
use phalanx_game::scenario::{
    SCENARIO_SEED, SPAWN_PAD_A, SPAWN_PAD_B, TEAM_A, TEAM_B, setup_battle, spawn_team,
};
use phalanx_game::snapshot::extract_snapshot;
use phalanx_game::{SharedBattleWorld, slice_roster};
use phalanx_navigation::crossing_map;
use std::collections::BTreeMap;

const PAD_A: (f32, f32) = SPAWN_PAD_A;
const PAD_B: (f32, f32) = SPAWN_PAD_B;

/// A small deterministic battle: one runner, one world, Team A spawned.
fn battle_with_team_a() -> (
    SimulationRunner<CommandPayload>,
    SharedBattleWorld,
    Vec<EntityId>,
) {
    let mut runner = SimulationRunner::new(TickRate::DEFAULT, SCENARIO_SEED);
    let world = setup_battle(&mut runner);
    let ids = spawn_team(runner.simulation_mut(), &world, TEAM_A, PAD_A);
    (runner, world, ids)
}

fn group_request(
    tick: u64,
    request_id: u64,
    payload: CommandPayload,
) -> CommandRequest<CommandPayload> {
    CommandRequest {
        request_id: RequestId(request_id),
        actor: ActorId(TEAM_A),
        session: SessionId(0),
        tick,
        subject: None,
        payload,
    }
}

fn step_until(
    runner: &mut SimulationRunner<CommandPayload>,
    world: &SharedBattleWorld,
    max_ticks: u64,
    pred: impl Fn(&BattleEvent) -> bool,
) -> u64 {
    let mut ticks = 0;
    while ticks < max_ticks {
        runner.step_tick().expect("tick");
        ticks += 1;
        let hit = world.borrow().events.iter().any(&pred);
        if hit {
            return ticks;
        }
    }
    ticks
}

#[test]
fn squad_membership_lifecycle_has_no_stale_references() {
    let (mut runner, world, ids) = battle_with_team_a();
    let members = &ids[..3];

    // Create a squad in line formation.
    runner
        .submit_command(group_request(
            1,
            1,
            CommandPayload::CreateSquad {
                members: members.to_vec(),
                formation: FormationTemplate::Line.tag(),
            },
        ))
        .expect("create accepted");
    runner.step_tick().expect("tick");
    let squad_id = {
        let world = world.borrow();
        assert_eq!(world.squads.len(), 1);
        let (id, squad) = world.squads.iter().next().expect("one squad");
        assert_eq!(squad.team, TEAM_A);
        assert_eq!(squad.formation, FormationTemplate::Line);
        assert_eq!(squad.members.len(), 3);
        for member in members {
            assert_eq!(world.combat[member].squad, Some(*id));
        }
        assert!(world.events.iter().any(|e| matches!(
            e,
            BattleEvent::SquadCreated { squad, .. } if *squad == *id
        )));
        *id
    };

    // Reassign: drop one member, add another; the dropped member must not
    // keep a stale squad reference.
    runner
        .submit_command(group_request(
            2,
            2,
            CommandPayload::SetSquadMembers {
                squad: squad_id,
                members: vec![ids[1], ids[2], ids[3]],
            },
        ))
        .expect("reassign accepted");
    runner.step_tick().expect("tick");
    {
        let world = world.borrow();
        let squad = &world.squads[&squad_id];
        assert_eq!(squad.members.len(), 3);
        assert!(!squad.members.contains(&ids[0]));
        assert_eq!(world.combat[&ids[0]].squad, None, "dropped member detached");
        assert_eq!(world.combat[&ids[3]].squad, Some(squad_id));
        assert!(world.events.iter().any(|e| matches!(
            e,
            BattleEvent::SquadMembersChanged { squad, .. } if *squad == squad_id
        )));
    }

    // Disband: all members unassigned, squad gone.
    runner
        .submit_command(group_request(
            3,
            3,
            CommandPayload::DisbandSquad { squad: squad_id },
        ))
        .expect("disband accepted");
    runner.step_tick().expect("tick");
    {
        let world = world.borrow();
        assert!(world.squads.is_empty());
        for id in &ids[..4] {
            assert_eq!(world.combat[id].squad, None);
        }
        assert!(world.events.iter().any(|e| matches!(
            e,
            BattleEvent::SquadDisbanded { squad, .. } if *squad == squad_id
        )));
    }
}

#[test]
fn invalid_squad_commands_are_rejected_with_structured_reasons() {
    let (mut runner, world, ids) = battle_with_team_a();
    // SquadMove for a squad that does not exist.
    runner
        .submit_command(group_request(
            1,
            1,
            CommandPayload::SquadMove {
                squad: SquadId(999),
                destination: (100.0, 100.0),
            },
        ))
        .expect("submitted");
    // CreateSquad with a dead/unknown member id.
    let ghost = phalanx_core::entity::EntityId::from_raw_parts(4242, 7);
    runner
        .submit_command(group_request(
            1,
            2,
            CommandPayload::CreateSquad {
                members: vec![ids[0], ghost],
                formation: FormationTemplate::Line.tag(),
            },
        ))
        .expect("submitted");
    // CreateSquad with an unknown formation tag.
    runner
        .submit_command(group_request(
            1,
            3,
            CommandPayload::CreateSquad {
                members: vec![ids[0]],
                formation: 77,
            },
        ))
        .expect("submitted");
    runner.step_tick().expect("tick");
    let world = world.borrow();
    assert!(world.squads.is_empty(), "rejected commands create nothing");
    let failed: Vec<CommandFailReason> = world
        .events
        .iter()
        .filter_map(|e| match e {
            BattleEvent::SquadCommandFailed { reason, .. }
            | BattleEvent::GroupCommandFailed { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect();
    assert_eq!(failed.len(), 3);
    assert!(failed.contains(&CommandFailReason::UnknownSquad));
    assert!(failed.contains(&CommandFailReason::UnknownMember));
    assert!(failed.contains(&CommandFailReason::InvalidFormation));
}

#[test]
fn squad_move_assigns_deterministic_slots_and_completes() {
    let (mut runner, world, ids) = battle_with_team_a();
    let members = &ids[..3];
    runner
        .submit_command(group_request(
            1,
            1,
            CommandPayload::CreateSquad {
                members: members.to_vec(),
                formation: FormationTemplate::Line.tag(),
            },
        ))
        .expect("create");
    runner
        .submit_command(group_request(
            2,
            2,
            CommandPayload::SquadMove {
                squad: SquadId(0),
                destination: (100.0, 100.0),
            },
        ))
        .expect("squad move");
    runner.step_tick().expect("tick"); // tick 1: create
    runner.step_tick().expect("tick"); // tick 2: move

    // Each member got a Move order to a slot near the anchor (100, 100), and
    // the slots match the deterministic template layout (line perpendicular
    // to the +x heading: same x, spread in z).
    {
        let world = world.borrow();
        let squad = &world.squads[&SquadId(0)];
        match &squad.command {
            Some(phalanx_game::SquadCommand::Move { destination }) => {
                assert_eq!(*destination, (100.0, 100.0));
            }
            other => panic!("expected Move(100,100), got {other:?}"),
        }
        let offsets = FormationTemplate::Line.slot_offsets(3);
        for (i, member) in squad.members.iter().enumerate() {
            let state = &world.combat[member];
            let Some(phalanx_game::BattleCommand::Move { destination }) = &state.command else {
                panic!("member must have a Move command");
            };
            // Expected slot: anchor + template offset rotated by the squad's
            // stored heading (unless the fallback compression moved it to a
            // free cell — then it must still be near the raw slot).
            let raw = phalanx_game::rotate_slot((100.0, 100.0), squad.heading, offsets[i]);
            let dist = ((destination.0 - raw.0).powi(2) + (destination.1 - raw.1).powi(2)).sqrt();
            assert!(
                dist < FORMATION_SPACING_M,
                "slot {i} must be the template slot (or its free-cell fallback): got {destination:?}, raw {raw:?}"
            );
            assert_eq!(state.move_destination, Some(*destination));
        }
    }

    // Run until the group order completes; members must arrive near their
    // slots and never stand on a blocked nav cell.
    let ticks = step_until(&mut runner, &world, 1500, |e| {
        matches!(e, BattleEvent::SquadOrderCompleted { .. })
    });
    assert!(ticks < 1500, "squad move must complete within the budget");
    {
        let world = world.borrow();
        assert!(world.events.iter().any(|e| matches!(
            e,
            BattleEvent::SquadOrderCompleted { squad, .. } if *squad == SquadId(0)
        )));
        let grid = world.movement.grid();
        let map = world.movement.map();
        for member in &world.squads[&SquadId(0)].members {
            let unit = world.movement.unit(*member).expect("alive");
            let cell = grid.cell_of_checked(map, unit.position).expect("in bounds");
            assert!(
                !grid.is_blocked(cell),
                "unit must not stand on an obstacle cell"
            );
            // Arrived: within one spacing of its slot (anchor + line offset).
            let anchor = (100.0, 100.0);
            let dz = unit.position.1 - anchor.1;
            assert!(
                dz.abs() <= FORMATION_SPACING_M * 2.0
                    && (unit.position.0 - anchor.0).abs() <= FORMATION_SPACING_M,
                "member must arrive near its slot: {:?}",
                unit.position
            );
        }
    }
}

#[test]
fn squad_move_through_the_chokepoint_never_enters_the_wall() {
    let (mut runner, world, ids) = battle_with_team_a();
    let members = &ids[..4];
    runner
        .submit_command(group_request(
            1,
            1,
            CommandPayload::CreateSquad {
                members: members.to_vec(),
                formation: FormationTemplate::Wedge.tag(),
            },
        ))
        .expect("create");
    // Across the map, through the wall's lane (the Crossing wall at x in
    // [55, 145] has a gap around z in (62, 138)).
    runner
        .submit_command(group_request(
            2,
            2,
            CommandPayload::SquadMove {
                squad: SquadId(0),
                destination: (180.0, 100.0),
            },
        ))
        .expect("squad move");

    let mut ticks = 0_u64;
    loop {
        runner.step_tick().expect("tick");
        ticks += 1;
        {
            let world = world.borrow();
            let grid = world.movement.grid();
            let map = world.movement.map();
            for unit in world.movement.units().values() {
                let cell = grid.cell_of_checked(map, unit.position).expect("in bounds");
                assert!(
                    !grid.is_blocked(cell),
                    "unit crossed the wall at {:?}",
                    unit.position
                );
            }
        }
        let settled = world.borrow().events.iter().any(|e| {
            matches!(
                e,
                BattleEvent::SquadOrderCompleted { .. } | BattleEvent::SquadOrderFailed { .. }
            )
        });
        assert!(ticks <= 2000, "no unbounded deadlock at the chokepoint");
        if settled {
            break;
        }
    }
    let world = world.borrow();
    let completed = world
        .events
        .iter()
        .any(|e| matches!(e, BattleEvent::SquadOrderCompleted { .. }));
    assert!(completed, "the wedge must traverse the chokepoint lane");
}

#[test]
fn destroyed_member_is_removed_from_its_squad() {
    let mut runner = SimulationRunner::new(TickRate::DEFAULT, SCENARIO_SEED);
    let world = setup_battle(&mut runner);
    let ids_a = spawn_team(runner.simulation_mut(), &world, TEAM_A, PAD_A);
    let ids_b = spawn_team(runner.simulation_mut(), &world, TEAM_B, (40.0, 100.0));
    // Squad of two Team A units; Team B is close enough to engage.
    runner
        .submit_command(group_request(
            1,
            1,
            CommandPayload::CreateSquad {
                members: vec![ids_a[0], ids_a[1]],
                formation: FormationTemplate::Line.tag(),
            },
        ))
        .expect("create");
    runner.step_tick().expect("tick");
    let squad_id = world.borrow().squads.keys().copied().next().expect("squad");
    // Hunt the squad member with overwhelming force: attack it with all of B.
    for (i, b) in ids_b.iter().enumerate() {
        runner
            .submit_command(CommandRequest {
                request_id: RequestId(10 + i as u64),
                actor: ActorId(TEAM_B),
                session: SessionId(0),
                tick: 2,
                subject: Some(*b),
                payload: CommandPayload::Attack { target: ids_a[0] },
            })
            .expect("attack");
    }
    let ticks = step_until(
        &mut runner,
        &world,
        2000,
        |e| matches!(e, BattleEvent::UnitDestroyed { unit, .. } if *unit == ids_a[0]),
    );
    assert!(ticks < 2000, "the targeted member must be destroyed");
    let world = world.borrow();
    let squad = &world.squads[&squad_id];
    assert!(!squad.members.contains(&ids_a[0]), "no stale membership");
    assert_eq!(squad.members.len(), 1);
    assert!(!world.combat.contains_key(&ids_a[0]));
}

#[test]
fn squad_attack_move_engages_and_defend_holds() {
    let mut runner = SimulationRunner::new(TickRate::DEFAULT, SCENARIO_SEED);
    let world = setup_battle(&mut runner);
    let ids_a = spawn_team(runner.simulation_mut(), &world, TEAM_A, PAD_A);
    let _ids_b = spawn_team(runner.simulation_mut(), &world, TEAM_B, PAD_B);
    runner
        .submit_command(group_request(
            1,
            1,
            CommandPayload::CreateSquad {
                members: ids_a[..2].to_vec(),
                formation: FormationTemplate::Column.tag(),
            },
        ))
        .expect("create");
    runner
        .submit_command(group_request(
            2,
            2,
            CommandPayload::SquadAttackMove {
                squad: SquadId(0),
                destination: (170.0, 100.0),
            },
        ))
        .expect("squad attack-move");
    // Run until the squad gets into contact (damage dealt) or settles.
    let mut ticks = 0_u64;
    loop {
        runner.step_tick().expect("tick");
        ticks += 1;
        assert!(ticks <= 2000, "bounded run");
        let world_ref = world.borrow();
        let damaged = world_ref
            .events
            .iter()
            .any(|e| matches!(e, BattleEvent::DamageDealt { .. }));
        let settled = world_ref.events.iter().any(|e| {
            matches!(
                e,
                BattleEvent::SquadOrderCompleted { .. } | BattleEvent::SquadOrderFailed { .. }
            )
        });
        if damaged || settled {
            break;
        }
    }
    {
        let squad = &world.borrow().squads[&SquadId(0)];
        assert!(matches!(
            squad.command,
            Some(phalanx_game::SquadCommand::AttackMove { .. })
        ));
        // Defend: hold at the current centroid; members get hold-and-engage
        // orders at their slots.
    }
    runner
        .submit_command(group_request(
            ticks + 1,
            3,
            CommandPayload::SquadDefend { squad: SquadId(0) },
        ))
        .expect("defend");
    runner.step_tick().expect("tick");
    let world_ref = world.borrow();
    let squad = &world_ref.squads[&SquadId(0)];
    assert!(matches!(
        squad.command,
        Some(phalanx_game::SquadCommand::Defend)
    ));
    for member in &squad.members {
        let state = &world_ref.combat[member];
        assert!(matches!(
            state.command,
            Some(phalanx_game::BattleCommand::AttackMove { .. })
        ));
    }
}

#[test]
fn squads_are_covered_by_replay_snapshot_and_render_snapshot() {
    let mut recorder = ReplayRecorder::new(TickRate::DEFAULT, SCENARIO_SEED);
    let world = setup_battle(recorder.runner_mut());
    let ids_a = spawn_team(
        recorder.runner_mut().simulation_mut(),
        &world,
        TEAM_A,
        PAD_A,
    );
    recorder
        .submit_command(group_request(
            1,
            1,
            CommandPayload::CreateSquad {
                members: ids_a[..3].to_vec(),
                formation: FormationTemplate::Wedge.tag(),
            },
        ))
        .expect("create");
    recorder
        .submit_command(group_request(
            2,
            2,
            CommandPayload::SquadMove {
                squad: SquadId(0),
                destination: (80.0, 100.0),
            },
        ))
        .expect("move");
    for _ in 0..50 {
        recorder.step_tick().expect("tick");
    }
    let recording = recorder.finish();
    let hashes: Vec<_> = recording.tick_hashes().to_vec();

    // Replay reproduces identical per-tick hashes.
    let mut content = BTreeMap::new();
    content.insert("map".to_owned(), canonical_digest(&crossing_map()));
    content.insert("roster".to_owned(), slice_roster().digest());
    let report = replay_recording(&recording, 50, &content, |runner| {
        let world = setup_battle(runner);
        spawn_team(runner.simulation_mut(), &world, TEAM_A, PAD_A);
    })
    .expect("replay");
    assert_eq!(
        report.tick_hashes(),
        hashes.as_slice(),
        "replay must be hash-identical"
    );
    assert_eq!(report.first_divergent_tick(), None);

    // Snapshot/restore preserves squad state (canonical digest identical).
    let mut runner = SimulationRunner::new(TickRate::DEFAULT, SCENARIO_SEED);
    let world = setup_battle(&mut runner);
    let ids_a = spawn_team(runner.simulation_mut(), &world, TEAM_A, PAD_A);
    runner
        .submit_command(group_request(
            1,
            1,
            CommandPayload::CreateSquad {
                members: ids_a[..3].to_vec(),
                formation: FormationTemplate::Wedge.tag(),
            },
        ))
        .expect("create");
    runner
        .submit_command(group_request(
            2,
            2,
            CommandPayload::SquadMove {
                squad: SquadId(0),
                destination: (80.0, 100.0),
            },
        ))
        .expect("move");
    runner.advance_ticks(20).expect("ticks");
    let digest_before = runner.canonical_digest();
    let snapshot = runner.snapshot();
    runner.advance_ticks(10).expect("more ticks");
    runner.restore_snapshot(&snapshot).expect("restore");
    assert_eq!(
        runner.canonical_digest(),
        digest_before,
        "squad state round-trips"
    );

    // The render snapshot exposes the squad (issue #110 diagnostics).
    let snap = extract_snapshot(&world.borrow(), &crossing_map(), runner.simulation().tick());
    assert_eq!(snap.squads.len(), 1);
    let squad = &snap.squads[0];
    assert_eq!(squad.squad_index, 0);
    assert_eq!(squad.team, TEAM_A);
    assert_eq!(squad.members.len(), 3);
    assert_eq!(squad.formation, FormationTemplate::Wedge.tag());
    assert!(squad.destination.is_some(), "active group order is visible");
}

#[test]
fn formation_change_reresolves_slots_deterministically() {
    let (mut runner, world, ids) = battle_with_team_a();
    runner
        .submit_command(group_request(
            1,
            1,
            CommandPayload::CreateSquad {
                members: ids[..3].to_vec(),
                formation: FormationTemplate::Line.tag(),
            },
        ))
        .expect("create");
    runner
        .submit_command(group_request(
            2,
            2,
            CommandPayload::SquadMove {
                squad: SquadId(0),
                destination: (100.0, 100.0),
            },
        ))
        .expect("move");
    runner
        .submit_command(group_request(
            2,
            3,
            CommandPayload::SetSquadFormation {
                squad: SquadId(0),
                formation: FormationTemplate::Wedge.tag(),
            },
        ))
        .expect("formation");
    runner.step_tick().expect("tick"); // tick 1: create
    runner.step_tick().expect("tick"); // tick 2: move + formation
    let world = world.borrow();
    let squad = &world.squads[&SquadId(0)];
    assert_eq!(squad.formation, FormationTemplate::Wedge);
    let offsets = FormationTemplate::Wedge.slot_offsets(3);
    for (i, member) in squad.members.iter().enumerate() {
        let Some(phalanx_game::BattleCommand::Move { destination }) = &world.combat[member].command
        else {
            panic!("member keeps its slot Move command");
        };
        let raw = phalanx_game::rotate_slot((100.0, 100.0), squad.heading, offsets[i]);
        let dist = ((destination.0 - raw.0).powi(2) + (destination.1 - raw.1).powi(2)).sqrt();
        assert!(
            dist < FORMATION_SPACING_M,
            "wedge slot {i}: got {destination:?}, raw {raw:?}"
        );
    }
}
