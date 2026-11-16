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
    /// Current health (for HUD health bars, issue #92).
    pub health: u32,
    /// Maximum health (for HUD health bars, issue #92).
    pub max_health: u32,
    /// Whether this entity is currently selected by the player (issue #90).
    pub selected: bool,
    /// The entity's current move destination, if any (for order markers, issue #92).
    pub move_destination: Option<[f32; 2]>,
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

    /// Converts the snapshot to shapes including HUD elements (issue #92):
    /// health bars above each unit, selection rings around selected units,
    /// and order-marker lines toward move destinations.
    ///
    /// `view_height_m` is the world-space height of the current camera view;
    /// it scales HUD element sizes so they stay readable at any zoom level.
    #[must_use]
    pub fn shapes_with_hud(&self, view_height_m: f32) -> Vec<DebugShape> {
        let mut shapes = self.shapes();
        // HUD scale: health bars and rings sized relative to the visible height.
        let bar_height = (view_height_m * 0.015).max(0.3);
        let bar_width = (view_height_m * 0.04).max(1.0);
        let ring_extra = (view_height_m * 0.008).max(0.2);
        let line_thickness = (view_height_m * 0.004).max(0.1);

        for entity in &self.entities {
            let [x, z] = entity.position;
            let r = entity.radius_m.max(0.5);

            // Health bar: background (dark) + foreground (green/yellow/red).
            let bar_z = z - r - bar_height;
            let frac = if entity.max_health > 0 {
                entity.health as f32 / entity.max_health as f32
            } else {
                0.0
            };
            let health_color = if frac > 0.5 {
                [0.2, 0.8, 0.2, 1.0]
            } else if frac > 0.25 {
                [0.9, 0.7, 0.1, 1.0]
            } else {
                [0.9, 0.2, 0.2, 1.0]
            };
            // Background bar (full width, dark).
            shapes.push(DebugShape::Rect {
                center: [x, bar_z],
                half_size: [bar_width * 0.5, bar_height * 0.5],
                color: [0.1, 0.1, 0.1, 0.8],
            });
            // Foreground bar (proportional to health).
            let fg_width = bar_width * frac;
            if fg_width > 0.0 {
                shapes.push(DebugShape::Rect {
                    center: [x - bar_width * 0.5 + fg_width * 0.5, bar_z],
                    half_size: [fg_width * 0.5, bar_height * 0.5],
                    color: health_color,
                });
            }

            // Selection ring: a slightly larger rect outline (rendered as a
            // thin rect ring using two nested rects — outer white, inner team color).
            if entity.selected {
                shapes.push(DebugShape::Rect {
                    center: [x, z],
                    half_size: [r + ring_extra, r + ring_extra],
                    color: [1.0, 1.0, 1.0, 0.9],
                });
                // Re-draw the unit on top so the ring appears as an outline.
                shapes.push(DebugShape::Rect {
                    center: [x, z],
                    half_size: [r, r],
                    color: team_color(entity.team),
                });
            }

            // Order marker: a line from the unit toward its move destination.
            if let Some(dest) = entity.move_destination {
                shapes.push(DebugShape::Line {
                    from: [x, z],
                    to: dest,
                    thickness: line_thickness,
                    color: [1.0, 1.0, 0.4, 0.6],
                });
            }
        }
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
                    health: 100,
                    max_health: 100,
                    selected: false,
                    move_destination: None,
                },
                RenderEntity {
                    entity_index: 6,
                    position: [180.0, 100.0],
                    team: 2,
                    category: EntityCategory::Unit,
                    radius_m: 0.5,
                    health: 30,
                    max_health: 60,
                    selected: true,
                    move_destination: Some([100.0, 100.0]),
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

    #[test]
    fn shapes_with_hud_adds_health_bars_selection_rings_and_order_markers() {
        let snapshot = RenderSnapshot {
            tick: 1,
            entities: vec![
                RenderEntity {
                    entity_index: 0,
                    position: [50.0, 50.0],
                    team: 1,
                    category: EntityCategory::Unit,
                    radius_m: 0.5,
                    health: 100,
                    max_health: 100,
                    selected: true,
                    move_destination: Some([100.0, 100.0]),
                },
                RenderEntity {
                    entity_index: 1,
                    position: [60.0, 60.0],
                    team: 2,
                    category: EntityCategory::Unit,
                    radius_m: 0.5,
                    health: 15,
                    max_health: 60,
                    selected: false,
                    move_destination: None,
                },
            ],
            obstacles: vec![],
        };
        let plain = snapshot.shapes();
        assert_eq!(plain.len(), 2, "just the two unit rects");

        let hud = snapshot.shapes_with_hud(200.0);
        // Per selected unit: base rect + ring outer + ring inner + health bg
        // + health fg + order line = 6. Per unselected unit: base rect +
        // health bg + health fg = 3. Total = 9.
        assert_eq!(hud.len(), 9, "HUD shapes added for selected + unselected");

        // The order marker is a Line from the unit toward its destination.
        let lines: Vec<_> = hud
            .iter()
            .filter_map(|s| match s {
                DebugShape::Line { from, to, .. } => Some((*from, *to)),
                _ => None,
            })
            .collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].0, [50.0, 50.0]);
        assert_eq!(lines[0].1, [100.0, 100.0]);

        // Low-health unit gets a red health bar.
        let red = [0.9, 0.2, 0.2, 1.0];
        assert!(
            hud.iter().any(|s| s.color() == red),
            "low-health unit should have a red health bar"
        );
        // Full-health unit gets a green health bar.
        let green = [0.2, 0.8, 0.2, 1.0];
        assert!(
            hud.iter().any(|s| s.color() == green),
            "full-health unit should have a green health bar"
        );
    }
}
