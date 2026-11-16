//! Simulation-to-render snapshot (issue #63).
//!
//! The renderer never reads simulation internals directly: the authoritative
//! state is extracted into a [`RenderSnapshot`] — a presentation-only view of
//! entities (position, team, category, radius) and static obstacles — by a
//! read-only extraction function on the simulation side. Extraction takes
//! `&BattleWorld` and cannot mutate authoritative state; the snapshot is
//! rebuilt from live entities on every extraction, so destroyed entities
//! disappear automatically. Rendering frame rate is independent of the
//! simulation tick rate: the runtime steps the simulation and extracts a
//! snapshot per rendered frame.
//!
//! Sim-to-render coordinate conversion is [`RenderSpace::from_world`] (the
//! top-down battlefield maps world meters (x right, z down) 1:1 to render
//! world coordinates; [`crate::geometry::OrthoProjection`] maps those to clip
//! space at draw time).

use crate::geometry::DebugShape;

/// Visual category of a rendered entity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntityCategory {
    /// A mobile unit.
    Unit,
    /// A static structure.
    Structure,
}

/// One entity in a render snapshot, in render world coordinates (meters).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderEntity {
    /// The entity's slot index (debug identity; stable while alive).
    pub entity_index: u32,
    /// Position in render world coordinates (meters, x/z plane).
    pub position: [f32; 2],
    /// Owning team (1 = Team A, 2 = Team B in the slice).
    pub team: u64,
    /// Visual category.
    pub category: EntityCategory,
    /// Collision/render radius in meters.
    pub radius_m: f32,
}

/// A static obstacle as a rectangle in render world coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ObstacleShape {
    /// Center (x, z) in meters.
    pub center: [f32; 2],
    /// Half extents (x, z) in meters.
    pub half_size: [f32; 2],
}

/// A presentation-only snapshot of the visible world at one tick.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderSnapshot {
    /// The simulation tick this snapshot was extracted at.
    pub tick: u64,
    /// Live entities (destroyed entities are absent).
    pub entities: Vec<RenderEntity>,
    /// Static obstacles.
    pub obstacles: Vec<ObstacleShape>,
}

impl RenderSnapshot {
    /// Converts the snapshot to debug shapes: gray obstacle rectangles plus
    /// one team-colored rectangle per entity (sized by its radius).
    #[must_use]
    pub fn shapes(&self) -> Vec<DebugShape> {
        let mut shapes: Vec<DebugShape> = self
            .obstacles
            .iter()
            .map(|obstacle| DebugShape::Rect {
                center: obstacle.center,
                half_size: obstacle.half_size,
                color: OBSTACLE_COLOR,
            })
            .collect();
        shapes.extend(self.entities.iter().map(|entity| DebugShape::Rect {
            center: entity.position,
            half_size: [entity.radius_m.max(0.5), entity.radius_m.max(0.5)],
            color: team_color(entity.team),
        }));
        shapes
    }
}

/// The obstacle color (neutral gray).
pub const OBSTACLE_COLOR: [f32; 4] = [0.45, 0.42, 0.38, 1.0];

/// Team colors: clearly distinguishable per team (issue #63: at least two
/// distinct teams represented visually).
#[must_use]
pub fn team_color(team: u64) -> [f32; 4] {
    match team {
        1 => [0.30, 0.45, 0.95, 1.0], // Team A: blue
        2 => [0.95, 0.30, 0.30, 1.0], // Team B: red
        _ => [0.85, 0.75, 0.20, 1.0], // other: yellow
    }
}

/// Sim-to-render coordinate conversion. The top-down battlefield uses world
/// meters directly as render world coordinates (x right, z down); the
/// orthographic projection maps them to clip space at draw time.
pub struct RenderSpace;

impl RenderSpace {
    /// Converts a simulation world coordinate (x, z) to render world
    /// coordinates.
    #[must_use]
    pub fn from_world(x: f32, z: f32) -> [f32; 2] {
        [x, z]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_colors_are_distinct() {
        assert_ne!(team_color(1), team_color(2));
        assert_ne!(team_color(1), team_color(99));
        assert_ne!(team_color(2), team_color(99));
    }

    #[test]
    fn snapshot_shapes_carry_obstacles_and_team_colored_entities() {
        let snapshot = RenderSnapshot {
            tick: 7,
            entities: vec![
                RenderEntity {
                    entity_index: 0,
                    position: [10.0, 20.0],
                    team: 1,
                    category: EntityCategory::Unit,
                    radius_m: 0.5,
                },
                RenderEntity {
                    entity_index: 6,
                    position: [180.0, 100.0],
                    team: 2,
                    category: EntityCategory::Unit,
                    radius_m: 0.5,
                },
            ],
            obstacles: vec![ObstacleShape {
                center: [100.0, 60.0],
                half_size: [2.0, 45.0],
            }],
        };
        let shapes = snapshot.shapes();
        assert_eq!(shapes.len(), 3);
        // Obstacle first, gray.
        match shapes[0] {
            DebugShape::Rect { color, .. } => assert_eq!(color, OBSTACLE_COLOR),
            _ => panic!("obstacle must be a rect"),
        }
        // Entities carry their team color and position.
        match shapes[1] {
            DebugShape::Rect {
                center,
                color,
                half_size,
            } => {
                assert_eq!(center, [10.0, 20.0]);
                assert_eq!(color, team_color(1));
                assert_eq!(half_size, [0.5, 0.5]);
            }
            _ => panic!("entity must be a rect"),
        }
        match shapes[2] {
            DebugShape::Rect { color, .. } => assert_eq!(color, team_color(2)),
            _ => panic!("entity must be a rect"),
        }
        // Render space conversion is the documented identity mapping.
        assert_eq!(RenderSpace::from_world(3.0, -4.0), [3.0, -4.0]);
    }
}
