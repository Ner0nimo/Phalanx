//! The navigation grid: a uniform grid over a static map with obstacles
//! inflated by a movement radius (`RtsEngineDoc.md` §7.1).

use crate::map::Map;

/// Neighbor exploration order (fixed, so pathfinding is deterministic).
const NEIGHBOR_OFFSETS: [(i32, i32); 8] = [
    (0, 1),
    (1, 1),
    (1, 0),
    (1, -1),
    (0, -1),
    (-1, -1),
    (-1, 0),
    (-1, 1),
];

/// A uniform navigation grid. Cells are blocked when their square intersects
/// any obstacle inflated by `inflate_m`; the inflation accounts for the unit
/// radius so paths keep clearance.
#[derive(Clone, Debug)]
pub struct NavGrid {
    width: u32,
    depth: u32,
    cell_size_m: f32,
    /// Row-major blocked flags: cell `(x, z)` is `blocked[z * width + x]`.
    blocked: Vec<bool>,
}

impl NavGrid {
    /// Builds the grid for `map` with the given cell size (meters) and
    /// obstacle inflation (meters, typically the unit radius).
    #[must_use]
    pub fn from_map(map: &Map, cell_size_m: f32, inflate_m: f32) -> Self {
        assert!(cell_size_m > 0.0, "cell size must be positive");
        let width = (map.width_m / cell_size_m).ceil() as u32;
        let depth = (map.depth_m / cell_size_m).ceil() as u32;
        let mut blocked = vec![false; (width * depth) as usize];
        for z in 0..depth {
            for x in 0..width {
                let cell_min = (x as f32 * cell_size_m, z as f32 * cell_size_m);
                let cell_max = ((x + 1) as f32 * cell_size_m, (z + 1) as f32 * cell_size_m);
                let cell_blocked = map.obstacles.iter().any(|obstacle| {
                    cell_min.0 < obstacle.max.0 + inflate_m
                        && cell_max.0 > obstacle.min.0 - inflate_m
                        && cell_min.1 < obstacle.max.1 + inflate_m
                        && cell_max.1 > obstacle.min.1 - inflate_m
                });
                blocked[(z * width + x) as usize] = cell_blocked;
            }
        }
        Self {
            width,
            depth,
            cell_size_m,
            blocked,
        }
    }

    /// Grid width in cells.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Grid depth in cells.
    #[must_use]
    pub const fn depth(&self) -> u32 {
        self.depth
    }

    /// Cell edge length in meters.
    #[must_use]
    pub const fn cell_size_m(&self) -> f32 {
        self.cell_size_m
    }

    /// Returns whether the cell is inside the grid.
    #[must_use]
    pub const fn in_bounds(&self, cell: (u32, u32)) -> bool {
        cell.0 < self.width && cell.1 < self.depth
    }

    /// Returns whether the cell is blocked. Out-of-bounds cells count as
    /// blocked.
    #[must_use]
    pub fn is_blocked(&self, cell: (u32, u32)) -> bool {
        if !self.in_bounds(cell) {
            return true;
        }
        self.blocked[(cell.1 * self.width + cell.0) as usize]
    }

    /// Maps a world position (meters) to its cell, clamped into the grid.
    #[must_use]
    pub fn cell_of(&self, position: (f32, f32)) -> (u32, u32) {
        let x = (position.0 / self.cell_size_m)
            .floor()
            .clamp(0.0, (self.width - 1) as f32) as u32;
        let z = (position.1 / self.cell_size_m)
            .floor()
            .clamp(0.0, (self.depth - 1) as f32) as u32;
        (x, z)
    }

    /// Maps a world position to its cell, or `None` when the position is
    /// outside the map extent.
    #[must_use]
    pub fn cell_of_checked(&self, map: &Map, position: (f32, f32)) -> Option<(u32, u32)> {
        if position.0 < 0.0
            || position.1 < 0.0
            || position.0 >= map.width_m
            || position.1 >= map.depth_m
        {
            return None;
        }
        Some(self.cell_of(position))
    }

    /// Returns the center of a cell in world coordinates (meters).
    #[must_use]
    pub fn cell_center(&self, cell: (u32, u32)) -> (f32, f32) {
        (
            (cell.0 as f32 + 0.5) * self.cell_size_m,
            (cell.1 as f32 + 0.5) * self.cell_size_m,
        )
    }

    /// All blocked cells, in row-major order (for diagnostics).
    #[must_use]
    pub fn blocked_cells(&self) -> Vec<(u32, u32)> {
        let mut cells = Vec::new();
        for z in 0..self.depth {
            for x in 0..self.width {
                if self.is_blocked((x, z)) {
                    cells.push((x, z));
                }
            }
        }
        cells
    }

    /// Traversable neighbor cells in the fixed exploration order, treating
    /// cells matching `avoid` as blocked (dynamic obstacles such as units).
    #[must_use]
    pub fn neighbors_avoiding(
        &self,
        cell: (u32, u32),
        avoid: &dyn Fn((u32, u32)) -> bool,
    ) -> Vec<(u32, u32)> {
        let (x, z) = cell;
        let mut out = Vec::with_capacity(8);
        for (dx, dz) in NEIGHBOR_OFFSETS {
            let nx = x as i32 + dx;
            let nz = z as i32 + dz;
            if nx < 0 || nz < 0 || nx >= self.width as i32 || nz >= self.depth as i32 {
                continue;
            }
            let next = (nx as u32, nz as u32);
            if self.is_blocked(next) || avoid(next) {
                continue;
            }
            if dx != 0 && dz != 0 {
                let side_a = ((x as i32 + dx) as u32, z);
                let side_b = (x, (z as i32 + dz) as u32);
                if self.is_blocked(side_a)
                    || avoid(side_a)
                    || self.is_blocked(side_b)
                    || avoid(side_b)
                {
                    continue;
                }
            }
            out.push(next);
        }
        out
    }

    /// Traversable neighbor cells (static grid only).
    #[must_use]
    pub fn neighbors(&self, cell: (u32, u32)) -> Vec<(u32, u32)> {
        self.neighbors_avoiding(cell, &|_| false)
    }

    /// Finds the nearest traversable cell to `cell` by expanding square rings
    /// up to `max_radius`, treating `avoid` cells as blocked; returns `cell`
    /// itself when it is free.
    #[must_use]
    pub fn nearest_free_cell_avoiding(
        &self,
        cell: (u32, u32),
        max_radius: u32,
        avoid: &dyn Fn((u32, u32)) -> bool,
    ) -> Option<(u32, u32)> {
        if self.in_bounds(cell) && !self.is_blocked(cell) && !avoid(cell) {
            return Some(cell);
        }
        let (cx, cz) = (cell.0 as i32, cell.1 as i32);
        for radius in 1..=max_radius as i32 {
            for dz in -radius..=radius {
                for dx in -radius..=radius {
                    if dx.abs().max(dz.abs()) != radius {
                        continue;
                    }
                    let nx = cx + dx;
                    let nz = cz + dz;
                    if nx < 0 || nz < 0 || nx >= self.width as i32 || nz >= self.depth as i32 {
                        continue;
                    }
                    let candidate = (nx as u32, nz as u32);
                    if !self.is_blocked(candidate) && !avoid(candidate) {
                        return Some(candidate);
                    }
                }
            }
        }
        None
    }
}

impl NavGrid {
    /// Finds the nearest traversable cell (static grid only).
    #[must_use]
    pub fn nearest_free_cell(&self, cell: (u32, u32), max_radius: u32) -> Option<(u32, u32)> {
        self.nearest_free_cell_avoiding(cell, max_radius, &|_| false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{Obstacle, crossing_map};

    fn crossing_grid() -> NavGrid {
        NavGrid::from_map(&crossing_map(), 1.0, 0.5)
    }

    #[test]
    fn walls_and_rocks_are_blocked_and_inflated() {
        let grid = crossing_grid();
        assert_eq!((grid.width(), grid.depth()), (200, 200));
        // Wall N center cell is blocked; inflation reaches one cell beyond.
        assert!(grid.is_blocked((100, 60)));
        assert!(grid.is_blocked((100, 57)), "inflated one cell north");
        assert!(grid.is_blocked((100, 62)), "inflated one cell south");
        assert!(!grid.is_blocked((100, 63)), "lane side of Wall N is free");
        // Rock W.
        assert!(grid.is_blocked((60, 100)));
        // Spawn pads are free.
        assert!(!grid.is_blocked((20, 100)));
        assert!(!grid.is_blocked((180, 100)));
        // Out of bounds counts as blocked.
        assert!(grid.is_blocked((200, 0)));
    }

    #[test]
    fn chokepoint_lane_is_the_only_passage_between_walls() {
        let grid = crossing_grid();
        // For every column x in the wall span, the free cells between the
        // walls lie strictly inside z in (62, 138).
        for x in 55..=145_u32 {
            let free: Vec<u32> = (0..200).filter(|&z| !grid.is_blocked((x, z))).collect();
            let in_lane: Vec<u32> = free
                .iter()
                .copied()
                .filter(|&z| z > 62 && z < 138)
                .collect();
            // The only free cells in the wall band are the lane (minus rocks).
            let outside: Vec<u32> = free
                .iter()
                .copied()
                .filter(|&z| !(z > 62 && z < 138))
                .collect();
            assert!(
                outside.iter().all(|&z| z <= 57 || z >= 143),
                "unexpected free cell at x={x}: {free:?}"
            );
            assert!(!in_lane.is_empty());
        }
    }

    #[test]
    fn neighbors_do_not_cut_corners() {
        let grid = crossing_grid();
        // Cell just south-east of Wall N's south-east corner area.
        let cell = (146, 63);
        let neighbors = grid.neighbors(cell);
        // Diagonal into the wall corner must be excluded when a side is blocked.
        assert!(
            !neighbors.contains(&(145, 62)),
            "no corner cutting into Wall N"
        );
        assert!(neighbors.contains(&(147, 63)));
    }

    #[test]
    fn nearest_free_cell_rings_outward() {
        let grid = crossing_grid();
        let found = grid
            .nearest_free_cell((100, 60), 8)
            .expect("free cell nearby");
        assert!(!grid.is_blocked(found));
        assert!(
            found.1 > 62 && found.1 < 138,
            "snaps into the lane: {found:?}"
        );
        assert_eq!(grid.nearest_free_cell((20, 100), 4), Some((20, 100)));
        // A fully enclosed cell finds nothing within the radius.
        let mut map = Map {
            width_m: 10.0,
            depth_m: 10.0,
            obstacles: vec![
                Obstacle::new(5.0, 2.0, 6.0, 2.0),
                Obstacle::new(5.0, 8.0, 6.0, 2.0),
                Obstacle::new(2.0, 5.0, 2.0, 6.0),
                Obstacle::new(8.0, 5.0, 2.0, 6.0),
            ],
        };
        map.obstacles.push(Obstacle::new(5.0, 5.0, 2.0, 2.0)); // center block
        let grid = NavGrid::from_map(&map, 1.0, 0.5);
        assert!(grid.is_blocked((5, 5)));
        assert!(grid.nearest_free_cell((5, 5), 1).is_none());
        assert!(grid.nearest_free_cell((5, 5), 4).is_some());
    }
}
