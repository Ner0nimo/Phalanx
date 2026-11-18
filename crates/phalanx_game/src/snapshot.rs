//! Simulation-to-render extraction (issue #63): builds a presentation-only
//! [`RenderSnapshot`] from the authoritative [`BattleWorld`] without mutating
//! it. The snapshot is rebuilt from live entities on every call, so entity
//! destruction is reflected by simple absence.

use phalanx_core::entity::EntityId;
use phalanx_navigation::map::Map;
use phalanx_render::snapshot::{
    EntityCategory, ObstacleShape, RenderEntity, RenderSnapshot, RenderSpace, RenderSquad,
};

use crate::world::BattleWorld;

/// Extracts the render snapshot of `world` at `tick` from `map`'s static
/// obstacles. Read-only: takes `&BattleWorld` and never mutates
/// authoritative state (verified by test via the canonical world digest).
///
/// Living entities are the units present in the combat state; their
/// positions come from the movement world. Destroyed units are absent from
/// both, so they simply do not appear. `selected` marks which entity indices
/// the player has selected (issues #90/#92 — presentation-only).
#[must_use]
pub fn extract_snapshot(world: &BattleWorld, map: &Map, tick: u64) -> RenderSnapshot {
    extract_snapshot_selected(world, map, tick, &[])
}

/// Like [`extract_snapshot`] but marks the given entity indices as selected
/// and includes health/destination for HUD rendering (issues #90/#92).
#[must_use]
pub fn extract_snapshot_selected(
    world: &BattleWorld,
    map: &Map,
    tick: u64,
    selected: &[u32],
) -> RenderSnapshot {
    let mut entities: Vec<RenderEntity> = Vec::with_capacity(world.combat.len());
    for (id, state) in &world.combat {
        let Some(unit) = world.movement.unit(*id) else {
            continue;
        };
        let def = world.def_of(*id);
        let radius_m = def.map(|d| d.radius_m).unwrap_or(0.5);
        let max_health = def.map(|d| d.health).unwrap_or(1);
        let is_selected = selected.contains(&id.index());
        entities.push(render_entity(
            *id,
            unit.position,
            state.team,
            radius_m,
            state.health,
            max_health,
            is_selected,
            state.move_destination,
        ));
    }
    entities.sort_by_key(|entity| entity.entity_index);
    let obstacles = map
        .obstacles
        .iter()
        .map(|obstacle| ObstacleShape {
            center: [
                (obstacle.min.0 + obstacle.max.0) / 2.0,
                (obstacle.min.1 + obstacle.max.1) / 2.0,
            ],
            half_size: [
                (obstacle.max.0 - obstacle.min.0) / 2.0,
                (obstacle.max.1 - obstacle.min.1) / 2.0,
            ],
        })
        .collect();
    let squads = world
        .squads
        .iter()
        .map(|(id, squad)| RenderSquad {
            squad_index: id.index(),
            team: squad.team,
            members: squad.members.iter().map(|m| m.index()).collect(),
            formation: squad.formation.tag(),
            destination: squad
                .command
                .as_ref()
                .and_then(|command| match command {
                    crate::world::SquadCommand::Move { destination }
                    | crate::world::SquadCommand::AttackMove { destination } => Some(*destination),
                    crate::world::SquadCommand::Defend => squad.anchor,
                })
                .map(|(x, z)| RenderSpace::from_world(x, z)),
        })
        .collect();
    RenderSnapshot {
        tick,
        entities,
        obstacles,
        squads,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_entity(
    id: EntityId,
    position: (f32, f32),
    team: u64,
    radius_m: f32,
    health: u32,
    max_health: u32,
    selected: bool,
    move_destination: Option<(f32, f32)>,
) -> RenderEntity {
    RenderEntity {
        entity_index: id.index(),
        position: RenderSpace::from_world(position.0, position.1),
        team,
        category: EntityCategory::Unit,
        radius_m,
        health,
        max_health,
        selected,
        move_destination: move_destination.map(|(x, z)| RenderSpace::from_world(x, z)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::{SCENARIO_SEED, TEAM_A, TEAM_B, setup_battle, spawn_team};

    const PAD_A: (f32, f32) = (20.0, 100.0);
    const PAD_B: (f32, f32) = (180.0, 100.0);
    use phalanx_core::hash::SnapshotSink;
    use phalanx_core::simulation::{SimulationRunner, TickRate};
    use phalanx_navigation::crossing_map;
    use phalanx_render::snapshot::team_color;

    fn battle() -> (
        SimulationRunner<phalanx_core::command::CommandPayload>,
        crate::SharedBattleWorld,
    ) {
        let mut runner = SimulationRunner::new(TickRate::DEFAULT, SCENARIO_SEED);
        let world = setup_battle(&mut runner);
        spawn_team(runner.simulation_mut(), &world, TEAM_A, PAD_A);
        spawn_team(runner.simulation_mut(), &world, TEAM_B, PAD_B);
        (runner, world)
    }

    fn world_digest(world: &BattleWorld) -> Vec<u8> {
        let mut sink = SnapshotSink::new();
        world.write_state(&mut sink);
        sink.bytes
    }

    #[test]
    fn extraction_reads_the_actual_simulation_state() {
        let (_runner, world) = battle();
        let world = world.borrow();
        let map = crossing_map();
        let snapshot = extract_snapshot(&world, &map, 42);
        assert_eq!(snapshot.tick, 42);
        // Both teams present, 6 units each.
        assert_eq!(snapshot.entities.len(), 12);
        assert_eq!(
            snapshot
                .entities
                .iter()
                .filter(|e| e.team == TEAM_A)
                .count(),
            6
        );
        assert_eq!(
            snapshot
                .entities
                .iter()
                .filter(|e| e.team == TEAM_B)
                .count(),
            6
        );
        // Positions correspond to the authoritative movement world.
        for entity in &snapshot.entities {
            let id = world
                .movement
                .units()
                .keys()
                .find(|id| id.index() == entity.entity_index)
                .copied()
                .expect("entity exists");
            let unit = world.movement.unit(id).expect("unit");
            assert_eq!(entity.position, [unit.position.0, unit.position.1]);
            assert_eq!(entity.team, world.combat.get(&id).expect("combat").team);
        }
        // Crossing map obstacles (2 walls + 2 rocks) are in the snapshot.
        assert_eq!(snapshot.obstacles.len(), map.obstacles.len());
        assert_eq!(snapshot.obstacles.len(), 4);
        // Shapes: obstacles gray, entities team-colored.
        let shapes = snapshot.shapes();
        assert_eq!(shapes.len(), 16);
        assert!(
            shapes[..4]
                .iter()
                .all(|s| s.color() == phalanx_render::snapshot::OBSTACLE_COLOR)
        );
        assert!(
            snapshot
                .entities
                .iter()
                .any(|e| team_color(e.team) == team_color(TEAM_A))
        );
    }

    #[test]
    fn extraction_does_not_mutate_authoritative_state() {
        let (_runner, world) = battle();
        let map = crossing_map();
        let before = world_digest(&world.borrow());
        let _snapshot = extract_snapshot(&world.borrow(), &map, 1);
        let after = world_digest(&world.borrow());
        assert_eq!(before, after, "extraction must be read-only");
    }

    #[test]
    fn destroyed_entities_disappear_from_the_snapshot() {
        let (_runner, world) = battle();
        let map = crossing_map();
        let victim = {
            let world = world.borrow();
            let snapshot = extract_snapshot(&world, &map, 0);
            snapshot.entities[0].entity_index
        };
        // Destroy the unit in authoritative state (as combat resolution does).
        {
            let mut world = world.borrow_mut();
            let id = world
                .movement
                .units()
                .keys()
                .find(|id| id.index() == victim)
                .copied()
                .expect("unit");
            world.combat.remove(&id);
            world.movement.remove(id);
        }
        let world = world.borrow();
        let snapshot = extract_snapshot(&world, &map, 1);
        assert_eq!(snapshot.entities.len(), 11);
        assert!(
            !snapshot.entities.iter().any(|e| e.entity_index == victim),
            "the destroyed unit must be absent"
        );
    }

    #[test]
    fn snapshot_reflects_simulation_progress_between_ticks() {
        let (mut runner, world) = battle();
        let map = crossing_map();
        let ids: Vec<EntityId> = {
            let world = world.borrow();
            world.movement.units().keys().copied().collect()
        };
        // Move one unit through a command.
        use phalanx_core::command::{
            ActorId, CommandPayload, CommandRequest, RequestId, SessionId,
        };
        runner
            .submit_command(CommandRequest {
                request_id: RequestId(1),
                actor: ActorId(TEAM_A),
                session: SessionId(0),
                tick: 1,
                subject: Some(ids[0]),
                payload: CommandPayload::AttackMove {
                    destination: (100.0, 100.0),
                },
            })
            .expect("submit");
        let initial = {
            let world = world.borrow();
            let unit = world.movement.unit(ids[0]).expect("unit");
            unit.position
        };
        runner.advance_ticks(30).expect("advance");
        let snapshot = extract_snapshot(&world.borrow(), &map, 30);
        let moved = snapshot
            .entities
            .iter()
            .find(|e| e.entity_index == ids[0].index())
            .expect("unit present");
        let world = world.borrow();
        let unit = world.movement.unit(ids[0]).expect("unit");
        assert_eq!(moved.position, [unit.position.0, unit.position.1]);
        let distance = |pos: [f32; 2]| ((pos[0] - 100.0).powi(2) + (pos[1] - 100.0).powi(2)).sqrt();
        let initial_distance = distance([initial.0, initial.1]);
        // 30 ticks at 4 m/s allows up to 6 m of straight-line progress, but
        // pathing around the rock at (60, 100) and separation from squad
        // mates slow the start, so require a clear margin (observed ~3.9 m)
        // rather than the theoretical maximum.
        assert!(
            distance(moved.position) < initial_distance - 2.0,
            "the unit actually moved toward the destination ({initial:?} -> {:?}, {} -> {} m)",
            moved.position,
            initial_distance,
            distance(moved.position)
        );
    }
}
