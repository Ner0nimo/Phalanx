//! Global route queries: deterministic A* on the [`NavGrid`] plus the
//! budgeted [`PathPlanner`] request queue (`RtsEngineDoc.md` §7.1).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

use phalanx_core::entity::EntityId;
use phalanx_core::hash::{
    CanonicalDecode, CanonicalError, CanonicalReader, CanonicalSerialize, CanonicalSink,
};

use crate::grid::NavGrid;
use crate::map::Map;

/// Orthogonal step cost (integer grid costs keep pathfinding exact).
pub const ORTHO_COST: u32 = 10;
/// Diagonal step cost (approximately sqrt(2) * ORTHO_COST).
pub const DIAG_COST: u32 = 14;

/// A route as a sequence of grid cells, start cell first, goal cell last.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Path {
    /// Cells from start to goal, inclusive.
    pub cells: Vec<(u32, u32)>,
}

impl Path {
    /// Number of cells in the path.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// Returns whether the path contains no cells.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }
}

impl CanonicalSerialize for Path {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        self.cells.write_canonical(sink);
    }
}

impl CanonicalDecode for Path {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        Ok(Self {
            cells: CanonicalDecode::read_canonical(reader)?,
        })
    }
}

/// Explicit route-query outcomes (`RtsEngineDoc.md` §7.1: explicit outcomes
/// for unreachable, blocked, stale, or invalid destinations).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteError {
    /// Start or goal is outside the map extent.
    OutOfBounds,
    /// The unit's current cell has no traversable cell nearby.
    StartBlocked,
    /// The destination has no traversable cell nearby.
    GoalBlocked,
    /// No route exists between start and goal.
    Unreachable,
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfBounds => formatter.write_str("destination outside the map"),
            Self::StartBlocked => formatter.write_str("unit is on a blocked cell"),
            Self::GoalBlocked => formatter.write_str("destination is blocked"),
            Self::Unreachable => formatter.write_str("no route to destination"),
        }
    }
}

impl std::error::Error for RouteError {}

impl CanonicalSerialize for RouteError {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u32(match self {
            Self::OutOfBounds => 0,
            Self::StartBlocked => 1,
            Self::GoalBlocked => 2,
            Self::Unreachable => 3,
        });
    }
}

impl CanonicalDecode for RouteError {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        match reader.read_u32()? {
            0 => Ok(Self::OutOfBounds),
            1 => Ok(Self::StartBlocked),
            2 => Ok(Self::GoalBlocked),
            3 => Ok(Self::Unreachable),
            other => Err(CanonicalError::UnknownTag(other)),
        }
    }
}

/// Octile heuristic in the same integer units as the step costs.
fn heuristic(from: (u32, u32), to: (u32, u32)) -> u32 {
    let dx = from.0.abs_diff(to.0);
    let dz = from.1.abs_diff(to.1);
    let (min, max) = (dx.min(dz), dx.max(dz));
    DIAG_COST * min + ORTHO_COST * (max - min)
}

/// Deterministic A* over the static grid. Ties break by `(f, h, cell)` in
/// the heap, so identical inputs always produce the identical route.
pub fn find_path(grid: &NavGrid, start: (u32, u32), goal: (u32, u32)) -> Result<Path, RouteError> {
    find_path_avoiding(grid, start, goal, &|_| false)
}

/// Deterministic A* over the grid with extra dynamically blocked cells
/// (e.g. cells occupied by other units), so recomputed routes go around
/// congestion instead of deadlocking against it.
pub fn find_path_avoiding(
    grid: &NavGrid,
    start: (u32, u32),
    goal: (u32, u32),
    avoid: &dyn Fn((u32, u32)) -> bool,
) -> Result<Path, RouteError> {
    if !grid.in_bounds(start) || !grid.in_bounds(goal) {
        return Err(RouteError::OutOfBounds);
    }
    if grid.is_blocked(start) || avoid(start) {
        return Err(RouteError::StartBlocked);
    }
    if grid.is_blocked(goal) || avoid(goal) {
        return Err(RouteError::GoalBlocked);
    }
    if start == goal {
        return Ok(Path { cells: vec![start] });
    }

    let width = grid.width();
    let index = |cell: (u32, u32)| (cell.1 * width + cell.0) as usize;
    let cell_of_index = |i: usize| ((i as u32) % width, (i as u32) / width);

    let mut g_score = vec![u32::MAX; (grid.width() * grid.depth()) as usize];
    let mut came_from = vec![u32::MAX; g_score.len()];
    let mut heap: BinaryHeap<Reverse<(u32, u32, u32)>> = BinaryHeap::new();

    g_score[index(start)] = 0;
    let h0 = heuristic(start, goal);
    heap.push(Reverse((h0, h0, index(start) as u32)));

    while let Some(Reverse((f, h, idx))) = heap.pop() {
        let cell = cell_of_index(idx as usize);
        if cell == goal {
            let mut cells = vec![goal];
            let start_index = index(start) as u32;
            let mut current = idx;
            while current != start_index {
                current = came_from[current as usize];
                cells.push(cell_of_index(current as usize));
            }
            cells.reverse();
            return Ok(Path { cells });
        }
        // Stale heap entry.
        if f != g_score[idx as usize] + h {
            continue;
        }
        for neighbor in grid.neighbors_avoiding(cell, avoid) {
            let diagonal = neighbor.0 != cell.0 && neighbor.1 != cell.1;
            let step = if diagonal { DIAG_COST } else { ORTHO_COST };
            let next = index(neighbor);
            let tentative = g_score[idx as usize] + step;
            if tentative < g_score[next] {
                g_score[next] = tentative;
                came_from[next] = idx;
                let h = heuristic(neighbor, goal);
                heap.push(Reverse((tentative + h, h, next as u32)));
            }
        }
    }
    Err(RouteError::Unreachable)
}

/// A queued route request for one unit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PathRequest {
    /// The unit requesting a route.
    pub unit: EntityId,
    /// The requested destination in world coordinates (meters).
    pub destination: (f32, f32),
}

impl CanonicalSerialize for PathRequest {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        self.unit.write_canonical(sink);
        self.destination.write_canonical(sink);
    }
}

impl CanonicalDecode for PathRequest {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        Ok(Self {
            unit: CanonicalDecode::read_canonical(reader)?,
            destination: CanonicalDecode::read_canonical(reader)?,
        })
    }
}

/// Planner counters, recorded in authoritative state and diagnostics.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PathStats {
    /// Requests ever submitted.
    pub submitted: u64,
    /// Requests processed (within budget) in total.
    pub processed: u64,
    /// Requests that failed with a route error.
    pub unreachable: u64,
    /// Requests still queued at the end of a tick (deferred by the budget).
    pub deferred: u64,
    /// Paths recomputed because a unit was blocked too long.
    pub recomputed: u64,
}

impl CanonicalSerialize for PathStats {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u64(self.submitted);
        sink.write_u64(self.processed);
        sink.write_u64(self.unreachable);
        sink.write_u64(self.deferred);
        sink.write_u64(self.recomputed);
    }
}

impl CanonicalDecode for PathStats {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        Ok(Self {
            submitted: reader.read_u64()?,
            processed: reader.read_u64()?,
            unreachable: reader.read_u64()?,
            deferred: reader.read_u64()?,
            recomputed: reader.read_u64()?,
        })
    }
}

/// The budgeted path-request queue. At most `max_per_tick` requests are
/// processed per tick; the rest are deferred deterministically (FIFO).
/// Re-submitting for a unit replaces its queued request (latest wins).
#[derive(Clone, Debug)]
pub struct PathPlanner {
    queue: VecDeque<PathRequest>,
    max_per_tick: u32,
    stats: PathStats,
}

impl PathPlanner {
    /// Creates a planner with a per-tick processing budget.
    #[must_use]
    pub fn new(max_per_tick: u32) -> Self {
        Self {
            queue: VecDeque::new(),
            max_per_tick,
            stats: PathStats::default(),
        }
    }

    /// The per-tick processing budget.
    #[must_use]
    pub const fn max_per_tick(&self) -> u32 {
        self.max_per_tick
    }

    /// Queued (unprocessed) requests, in FIFO order.
    #[must_use]
    pub fn pending_requests(&self) -> &VecDeque<PathRequest> {
        &self.queue
    }

    /// Number of queued requests.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    /// Lifetime planner counters.
    #[must_use]
    pub const fn stats(&self) -> PathStats {
        self.stats
    }

    /// Queues a route request for `unit`; an existing queued request for the
    /// same unit is replaced.
    pub fn submit(&mut self, unit: EntityId, destination: (f32, f32)) {
        self.queue.retain(|request| request.unit != unit);
        self.queue.push_back(PathRequest { unit, destination });
        self.stats.submitted += 1;
    }

    /// Cancels a queued request for `unit`, if any.
    pub fn cancel(&mut self, unit: EntityId) {
        self.queue.retain(|request| request.unit != unit);
    }

    /// Records that a path was recomputed for a blocked unit.
    pub fn note_recompute(&mut self) {
        self.stats.recomputed += 1;
    }

    /// Processes up to `max_per_tick` queued requests against the grid.
    /// `position_of` resolves a unit's current world position. Requests for
    /// units that no longer exist are dropped. Returns the outcome per unit,
    /// in processing order.
    pub fn process_tick(
        &mut self,
        grid: &NavGrid,
        map: &Map,
        position_of: impl Fn(EntityId) -> Option<(f32, f32)>,
        avoid_cells: &std::collections::BTreeSet<(u32, u32)>,
    ) -> Vec<(EntityId, Result<Path, RouteError>)> {
        let avoid = |cell: (u32, u32)| avoid_cells.contains(&cell);
        let mut outcomes = Vec::new();
        let mut budget = self.max_per_tick;
        while budget > 0 {
            let Some(request) = self.queue.pop_front() else {
                break;
            };
            budget -= 1;
            self.stats.processed += 1;
            let Some(position) = position_of(request.unit) else {
                continue;
            };
            let Some(start_raw) = grid.cell_of_checked(map, position) else {
                outcomes.push((request.unit, Err(RouteError::OutOfBounds)));
                self.stats.unreachable += 1;
                continue;
            };
            let Some(goal_raw) = grid.cell_of_checked(map, request.destination) else {
                outcomes.push((request.unit, Err(RouteError::OutOfBounds)));
                self.stats.unreachable += 1;
                continue;
            };
            let start = grid
                .nearest_free_cell_avoiding(start_raw, 4, &avoid)
                .ok_or(RouteError::StartBlocked);
            let goal = grid
                .nearest_free_cell_avoiding(goal_raw, 8, &avoid)
                .ok_or(RouteError::GoalBlocked);
            let result = match (start, goal) {
                (Ok(start), Ok(goal)) => find_path_avoiding(grid, start, goal, &avoid),
                (Err(error), _) | (_, Err(error)) => Err(error),
            };
            if result.is_err() {
                self.stats.unreachable += 1;
            }
            outcomes.push((request.unit, result));
        }
        if !self.queue.is_empty() {
            self.stats.deferred += self.queue.len() as u64;
        }
        outcomes
    }
}

impl CanonicalSerialize for PathPlanner {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u32(self.max_per_tick);
        self.queue
            .iter()
            .copied()
            .collect::<Vec<_>>()
            .write_canonical(sink);
        self.stats.write_canonical(sink);
    }
}

/// Decoded planner state (queue + budget + counters).
#[derive(Clone, Debug, PartialEq)]
pub struct PlannerState {
    /// The budget.
    pub max_per_tick: u32,
    /// Queued requests in FIFO order.
    pub queue: Vec<PathRequest>,
    /// Lifetime counters.
    pub stats: PathStats,
}

impl CanonicalDecode for PlannerState {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        Ok(Self {
            max_per_tick: reader.read_u32()?,
            queue: CanonicalDecode::read_canonical(reader)?,
            stats: CanonicalDecode::read_canonical(reader)?,
        })
    }
}

impl PathPlanner {
    /// Rebuilds a planner from decoded state (queue + budget + counters).
    #[must_use]
    pub fn from_state(state: PlannerState) -> Self {
        Self {
            queue: state.queue.into(),
            max_per_tick: state.max_per_tick,
            stats: state.stats,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{Map, Obstacle, crossing_map};

    fn crossing_grid() -> NavGrid {
        NavGrid::from_map(&crossing_map(), 1.0, 0.5)
    }

    #[test]
    fn a_star_routes_through_the_chokepoint_lane() {
        let grid = crossing_grid();
        let path = find_path(&grid, (20, 100), (180, 100)).expect("route exists");
        assert_eq!(path.cells.first(), Some(&(20, 100)));
        assert_eq!(path.cells.last(), Some(&(180, 100)));
        assert!(path.len() > 160, "route is not a straight blocked line");
        for &cell in &path.cells {
            assert!(!grid.is_blocked(cell), "path cell {cell:?} is blocked");
            // While crossing the wall band, the path must stay in the lane.
            if (55..=145).contains(&cell.0) {
                assert!(
                    cell.1 > 62 && cell.1 < 138,
                    "path leaves the lane at {cell:?}"
                );
            }
        }
        // Consecutive cells are neighbors.
        for pair in path.cells.windows(2) {
            assert!(grid.neighbors(pair[0]).contains(&pair[1]));
        }
    }

    #[test]
    fn a_star_is_deterministic() {
        let grid = crossing_grid();
        let first = find_path(&grid, (20, 100), (180, 100)).unwrap();
        let second = find_path(&grid, (20, 100), (180, 100)).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn unreachable_and_invalid_goals_are_explicit() {
        // Enclose a pocket so the goal is truly unreachable.
        let map = Map {
            width_m: 20.0,
            depth_m: 20.0,
            obstacles: vec![
                Obstacle::new(10.0, 4.0, 12.0, 2.0),
                Obstacle::new(10.0, 16.0, 12.0, 2.0),
                Obstacle::new(4.0, 10.0, 2.0, 12.0),
                Obstacle::new(16.0, 10.0, 2.0, 12.0),
            ],
        };
        let grid = NavGrid::from_map(&map, 1.0, 0.5);
        let result = find_path(&grid, (2, 2), (10, 10));
        assert_eq!(result, Err(RouteError::Unreachable));
        assert_eq!(
            find_path(&grid, (2, 2), (25, 2)),
            Err(RouteError::OutOfBounds)
        );
        // (10, 4) is inside the north wall: blocked start.
        assert_eq!(
            find_path(&grid, (10, 4), (2, 2)),
            Err(RouteError::StartBlocked)
        );
    }

    #[test]
    fn planner_respects_the_per_tick_budget() {
        let grid = crossing_grid();
        let map = crossing_map();
        let mut planner = PathPlanner::new(2);
        let unit = EntityId::from_raw_parts(0, 1);
        for i in 0..5_u32 {
            planner.submit(unit, (10.0 + i as f32, 10.0));
        }
        // Resubmitting the same unit replaces the queued request.
        assert_eq!(planner.pending(), 1);
        planner.submit(EntityId::from_raw_parts(1, 1), (30.0, 30.0));
        planner.submit(EntityId::from_raw_parts(2, 1), (40.0, 40.0));
        assert_eq!(planner.pending(), 3);

        let positions = |_: EntityId| Some((10.5, 10.5));
        let outcomes =
            planner.process_tick(&grid, &map, positions, &std::collections::BTreeSet::new());
        assert_eq!(outcomes.len(), 2, "budget of 2 per tick");
        assert_eq!(planner.pending(), 1);
        assert!(outcomes.iter().all(|(_, result)| result.is_ok()));
        let stats = planner.stats();
        assert_eq!(stats.submitted, 7);
        assert_eq!(stats.processed, 2);
        assert_eq!(stats.deferred, 1);
    }
}
