//! Authoritative local movement: units follow planned routes at their
//! profile speed with deterministic pairwise separation
//! (`docs/vertical-slice-scenario.md` §5, `RtsEngineDoc.md` §7.2).
//!
//! All movement state lives in [`MovementWorld`], which is canonical
//! (de)serializable, so it is covered by checkpoint digests, snapshots, and
//! replays through the issue #34 system-state contract.

use std::collections::{BTreeMap, BTreeSet};

use phalanx_core::entity::{EntityAllocator, EntityId};
use phalanx_core::hash::{
    CanonicalDecode, CanonicalError, CanonicalReader, CanonicalSerialize, CanonicalSink, Digest,
    canonical_digest,
};

use crate::grid::NavGrid;
use crate::map::Map;
use crate::pathfind::{Path, PathPlanner, RouteError};

/// A data-driven movement profile (`RtsEngineDoc.md` §7.4).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MovementProfile {
    /// Top speed in meters per second.
    pub speed_m_per_s: f32,
    /// Collision radius in meters (used for grid inflation).
    pub radius_m: f32,
    /// Desired separation distance to other units in meters.
    pub separation_m: f32,
    /// Distance to the destination at which a move order completes, meters.
    pub stop_distance_m: f32,
}

/// The initial ground profile (striker-like values from the slice spec §3).
pub const GROUND: MovementProfile = MovementProfile {
    speed_m_per_s: 4.0,
    radius_m: 0.5,
    separation_m: 1.0,
    stop_distance_m: 0.5,
};

impl CanonicalSerialize for MovementProfile {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u32(self.speed_m_per_s.to_bits());
        sink.write_u32(self.radius_m.to_bits());
        sink.write_u32(self.separation_m.to_bits());
        sink.write_u32(self.stop_distance_m.to_bits());
    }
}

impl CanonicalDecode for MovementProfile {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        Ok(Self {
            speed_m_per_s: f32::from_bits(reader.read_u32()?),
            radius_m: f32::from_bits(reader.read_u32()?),
            separation_m: f32::from_bits(reader.read_u32()?),
            stop_distance_m: f32::from_bits(reader.read_u32()?),
        })
    }
}

/// Observable lifecycle of a move order (`docs/vertical-slice-scenario.md`
/// §4: completion/failure must be observable).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OrderStatus {
    /// Order issued; route pending or being followed.
    Active,
    /// The unit reached its destination.
    Completed,
    /// The order failed with an explicit reason (unreachable/blocked).
    Failed(RouteError),
}

impl CanonicalSerialize for OrderStatus {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        match self {
            Self::Active => sink.write_u32(0),
            Self::Completed => sink.write_u32(1),
            Self::Failed(error) => {
                sink.write_u32(2);
                error.write_canonical(sink);
            }
        }
    }
}

impl CanonicalDecode for OrderStatus {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        match reader.read_u32()? {
            0 => Ok(Self::Active),
            1 => Ok(Self::Completed),
            2 => Ok(Self::Failed(CanonicalDecode::read_canonical(reader)?)),
            other => Err(CanonicalError::UnknownTag(other)),
        }
    }
}

/// One moving unit's authoritative state.
#[derive(Clone, Debug, PartialEq)]
pub struct Unit {
    /// Stable logical entity ID.
    pub id: EntityId,
    /// World position (meters, x/z ground plane).
    pub position: (f32, f32),
    /// Movement profile.
    pub profile: MovementProfile,
    /// Current destination, if an order is active.
    pub destination: Option<(f32, f32)>,
    /// Planned route (grid cells), if any.
    pub path: Option<Path>,
    /// Index of the next waypoint in `path`.
    pub next_waypoint: u64,
    /// Order lifecycle status.
    pub status: Option<OrderStatus>,
    /// Ticks since the unit last made progress while ordered.
    pub blocked_ticks: u32,
    /// Last separation push applied to this unit (avoidance vector).
    pub last_avoidance: Option<(f32, f32)>,
}

impl CanonicalSerialize for Unit {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        self.id.write_canonical(sink);
        self.position.write_canonical(sink);
        self.profile.write_canonical(sink);
        self.destination.write_canonical(sink);
        self.path.write_canonical(sink);
        sink.write_u64(self.next_waypoint);
        self.status.write_canonical(sink);
        sink.write_u32(self.blocked_ticks);
        self.last_avoidance.write_canonical(sink);
    }
}

impl CanonicalDecode for Unit {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        Ok(Self {
            id: CanonicalDecode::read_canonical(reader)?,
            position: CanonicalDecode::read_canonical(reader)?,
            profile: CanonicalDecode::read_canonical(reader)?,
            destination: CanonicalDecode::read_canonical(reader)?,
            path: CanonicalDecode::read_canonical(reader)?,
            next_waypoint: reader.read_u64()?,
            status: CanonicalDecode::read_canonical(reader)?,
            blocked_ticks: reader.read_u32()?,
            last_avoidance: CanonicalDecode::read_canonical(reader)?,
        })
    }
}

/// Errors when issuing a move order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MoveOrderError {
    /// The entity ID does not refer to a live unit in this world (stale IDs
    /// never resolve to a reused entity).
    UnknownUnit,
}

impl std::fmt::Display for MoveOrderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownUnit => formatter.write_str("unknown unit (stale or foreign entity id)"),
        }
    }
}

impl std::error::Error for MoveOrderError {}

/// Errors when spawning a unit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpawnError {
    /// The position is outside the map extent.
    OutOfBounds,
    /// The position is on a blocked (inflated obstacle) cell.
    BlockedCell,
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfBounds => formatter.write_str("spawn position outside the map"),
            Self::BlockedCell => formatter.write_str("spawn position on a blocked cell"),
        }
    }
}

impl std::error::Error for SpawnError {}

/// Returns whether a world position is inside the map and not on a blocked
/// cell.
fn position_is_free(grid: &NavGrid, map: &Map, position: (f32, f32)) -> bool {
    grid.cell_of_checked(map, position).is_some() && !grid.is_blocked(grid.cell_of(position))
}

/// Float epsilon for the separation invariant.
pub const SEPARATION_EPS: f32 = 1e-4;

/// Maximum outer separation passes per tick (bounded, deterministic).
pub const MAX_SEPARATION_PASSES: u32 = 8;

/// Spatial hash key of a position (1 m cells).
fn hash_key(position: (f32, f32)) -> (i32, i32) {
    (position.0.floor() as i32, position.1.floor() as i32)
}

/// Whether `candidate` keeps the separation distance from every lower-ID
/// unit in the spatial hash.
fn separation_ok(
    spatial: &BTreeMap<(i32, i32), Vec<EntityId>>,
    units: &BTreeMap<EntityId, Unit>,
    candidate: (f32, f32),
    b: EntityId,
) -> bool {
    let home = hash_key(candidate);
    for dx in -1..=1_i32 {
        for dz in -1..=1_i32 {
            if let Some(anchors) = spatial.get(&(home.0 + dx, home.1 + dz)) {
                for &a in anchors {
                    if a >= b {
                        continue;
                    }
                    let pa = units[&a].position;
                    let distance =
                        ((candidate.0 - pa.0).powi(2) + (candidate.1 - pa.1).powi(2)).sqrt();
                    let separation =
                        (units[&a].profile.separation_m + units[&b].profile.separation_m) / 2.0;
                    if distance < separation - SEPARATION_EPS {
                        return false;
                    }
                }
            }
        }
    }
    true
}

/// Ticks without progress after which a path is recomputed (slice spec §5).
pub const RECOMPUTE_BLOCKED_TICKS: u32 = 10;
/// Blocked ticks after which a unit is reported as stuck in diagnostics.
pub const STUCK_THRESHOLD: u32 = 20;

/// The authoritative movement state: grid, units, and the budgeted planner.
///
/// Deterministic by construction: units iterate in ascending `EntityId`
/// order, separation resolves pairs in ascending ID order (lower ID keeps its
/// position), and all arithmetic is exactly-defined IEEE-754 single ops.
#[derive(Clone, Debug)]
pub struct MovementWorld {
    map_digest: Digest,
    grid: NavGrid,
    map: Map,
    tick_rate: u32,
    units: BTreeMap<EntityId, Unit>,
    planner: PathPlanner,
}

impl MovementWorld {
    /// Creates an empty world over `map` with a 1 m grid (or the given cell
    /// size), obstacles inflated by `profile.radius_m` (the caller passes
    /// the inflation explicitly via `inflate_m`), at the given tick rate.
    #[must_use]
    pub fn new(
        map: &Map,
        cell_size_m: f32,
        inflate_m: f32,
        tick_rate: u32,
        planner_budget: u32,
    ) -> Self {
        Self {
            map_digest: canonical_digest(map),
            grid: NavGrid::from_map(map, cell_size_m, inflate_m),
            map: map.clone(),
            tick_rate,
            units: BTreeMap::new(),
            planner: PathPlanner::new(planner_budget),
        }
    }

    /// The navigation grid.
    #[must_use]
    pub const fn grid(&self) -> &NavGrid {
        &self.grid
    }

    /// The map this world was built from.
    #[must_use]
    pub const fn map(&self) -> &Map {
        &self.map
    }

    /// Canonical digest of the map (content identity, part of world state).
    #[must_use]
    pub fn map_digest(&self) -> Digest {
        self.map_digest.clone()
    }

    /// The configured tick rate (ticks per second).
    #[must_use]
    pub const fn tick_rate(&self) -> u32 {
        self.tick_rate
    }

    /// The path planner.
    #[must_use]
    pub const fn planner(&self) -> &PathPlanner {
        &self.planner
    }

    /// All units in ascending entity-ID order.
    #[must_use]
    pub fn units(&self) -> &BTreeMap<EntityId, Unit> {
        &self.units
    }

    /// Returns one unit.
    #[must_use]
    pub fn unit(&self, id: EntityId) -> Option<&Unit> {
        self.units.get(&id)
    }

    /// The observable status of a unit's current order.
    #[must_use]
    pub fn order_status(&self, id: EntityId) -> Option<OrderStatus> {
        self.units.get(&id).and_then(|unit| unit.status)
    }

    /// Spawns a unit at `position` (must be in bounds and on a free cell),
    /// allocating its stable entity ID from `allocator`.
    pub fn spawn(
        &mut self,
        allocator: &mut EntityAllocator,
        position: (f32, f32),
        profile: MovementProfile,
    ) -> Result<EntityId, SpawnError> {
        if self.grid.cell_of_checked(&self.map, position).is_none() {
            return Err(SpawnError::OutOfBounds);
        }
        if self.grid.is_blocked(self.grid.cell_of(position)) {
            return Err(SpawnError::BlockedCell);
        }
        let id = allocator.allocate().expect("entity slot space");
        self.units.insert(
            id,
            Unit {
                id,
                position,
                profile,
                destination: None,
                path: None,
                next_waypoint: 0,
                status: None,
                blocked_ticks: 0,
                last_avoidance: None,
            },
        );
        Ok(id)
    }

    /// Removes a unit (e.g. on destruction) and cancels its queued request.
    pub fn remove(&mut self, id: EntityId) {
        self.units.remove(&id);
        self.planner.cancel(id);
    }

    /// Issues a move order: plan a route to `destination` and follow it.
    /// A new order replaces the unit's current one. Routing outcomes
    /// (unreachable/blocked) surface via [`MovementWorld::order_status`]
    /// once the planner processes the request.
    pub fn order_move(
        &mut self,
        unit: EntityId,
        destination: (f32, f32),
    ) -> Result<(), MoveOrderError> {
        if !self.units.contains_key(&unit) {
            return Err(MoveOrderError::UnknownUnit);
        }
        self.planner.submit(unit, destination);
        let unit_ref = self.units.get_mut(&unit).expect("checked above");
        unit_ref.destination = Some(destination);
        unit_ref.path = None;
        unit_ref.next_waypoint = 0;
        unit_ref.status = Some(OrderStatus::Active);
        unit_ref.blocked_ticks = 0;
        Ok(())
    }

    /// Cancels a unit's current order.
    pub fn cancel_order(&mut self, unit: EntityId) {
        self.planner.cancel(unit);
        if let Some(unit_ref) = self.units.get_mut(&unit) {
            unit_ref.status = None;
            unit_ref.destination = None;
            unit_ref.path = None;
        }
    }

    /// Advances the world by exactly one tick: process the planner budget,
    /// advance units along their routes, then resolve separation and obstacle
    /// constraints deterministically.
    pub fn step(&mut self) {
        let Self {
            grid,
            map,
            tick_rate,
            units,
            planner,
            ..
        } = self;
        let tick_rate = *tick_rate;

        // 1. Route requests, within the per-tick budget. Cells occupied by
        //    other units (their cell plus the separation neighborhood) are
        //    dynamically blocked, so routes go around congestion.
        let mut occupied: BTreeSet<(u32, u32)> = BTreeSet::new();
        for unit in units.values() {
            let cell = grid.cell_of(unit.position);
            for dx in -1..=1_i32 {
                for dz in -1..=1_i32 {
                    let nx = cell.0 as i32 + dx;
                    let nz = cell.1 as i32 + dz;
                    if nx >= 0
                        && nz >= 0
                        && (nx as u32) < grid.width()
                        && (nz as u32) < grid.depth()
                    {
                        occupied.insert((nx as u32, nz as u32));
                    }
                }
            }
        }
        let outcomes = planner.process_tick(
            grid,
            map,
            |id| units.get(&id).map(|u| u.position),
            &occupied,
        );
        for (unit_id, result) in outcomes {
            let Some(unit) = units.get_mut(&unit_id) else {
                continue;
            };
            match result {
                Ok(path) => {
                    unit.path = Some(path);
                    unit.next_waypoint = 1;
                }
                Err(error) => {
                    unit.status = Some(OrderStatus::Failed(error));
                    unit.path = None;
                }
            }
        }

        // Spatial hash over unit positions (cell edge = 1 m). Rebuilt entries
        // as units move, so when a unit moves, all lower-ID units (anchors)
        // are already at their final positions for this tick.
        let ids: Vec<EntityId> = units.keys().copied().collect();
        let mut spatial: BTreeMap<(i32, i32), Vec<EntityId>> = BTreeMap::new();
        for &id in &ids {
            let position = units[&id].position;
            spatial
                .entry((position.0.floor() as i32, position.1.floor() as i32))
                .or_default()
                .push(id);
        }

        // 2. Advance each ordered unit along its route, ascending entity-ID
        //    order. A unit never moves into a blocked cell or into another
        //    unit's separation radius: axis-aligned fallbacks, else it stays.
        let previous: BTreeMap<EntityId, (f32, f32)> = units
            .iter()
            .map(|(id, unit)| (*id, unit.position))
            .collect();
        for &b in &ids {
            let origin = units[&b].position;
            // Compute the desired advance (short-lived mutable borrow).
            let (target, arrived) = {
                let Some(unit) = units.get_mut(&b) else {
                    continue;
                };
                if unit.status != Some(OrderStatus::Active) {
                    continue;
                }
                let Some(path) = &unit.path else {
                    continue;
                };
                let mut remaining = unit.profile.speed_m_per_s / tick_rate as f32;
                let mut target = origin;
                while let Some(&cell) = path.cells.get(unit.next_waypoint as usize) {
                    let waypoint = grid.cell_center(cell);
                    let dx = waypoint.0 - target.0;
                    let dz = waypoint.1 - target.1;
                    let distance = (dx * dx + dz * dz).sqrt();
                    if distance <= remaining {
                        target = waypoint;
                        unit.next_waypoint += 1;
                        remaining -= distance;
                    } else if distance > 0.0 {
                        target = (
                            target.0 + dx / distance * remaining,
                            target.1 + dz / distance * remaining,
                        );
                        remaining = 0.0;
                    }
                    if remaining <= 0.0 {
                        break;
                    }
                }
                // Stop within stop distance of the destination.
                let arrived = unit.destination.map(|destination| {
                    let dx = destination.0 - target.0;
                    let dz = destination.1 - target.1;
                    (dx * dx + dz * dz).sqrt() <= unit.profile.stop_distance_m
                });
                (target, arrived.unwrap_or(false))
            };
            // Clamp the candidate: free cell, then separation from lower IDs.
            let candidates = [target, (target.0, origin.1), (origin.0, target.1), origin];
            let chosen = candidates
                .into_iter()
                .find(|candidate| {
                    position_is_free(grid, map, *candidate)
                        && separation_ok(&spatial, units, *candidate, b)
                })
                .unwrap_or(origin);
            {
                let unit = units.get_mut(&b).expect("id from keys");
                unit.position = chosen;
                if arrived {
                    unit.status = Some(OrderStatus::Completed);
                    unit.path = None;
                }
            }
            if chosen != origin {
                // Update the spatial hash entry for this unit.
                let old_key = hash_key(origin);
                if let Some(bucket) = spatial.get_mut(&old_key) {
                    bucket.retain(|id| *id != b);
                }
                spatial.entry(hash_key(chosen)).or_default().push(b);
            }
        }

        // 3. Residual separation: pushes any still-overlapping higher-ID unit
        //    out (bounded passes, deterministic order). With movement clamped
        //    in step 2 this normally resolves nothing; it repairs overlaps
        //    inherited from spawning or congestion.
        for _pass in 0..MAX_SEPARATION_PASSES {
            let mut pushed_any = false;
            for &b in &ids {
                let mut pb = units[&b].position;
                let home = hash_key(pb);
                let mut pushed = false;
                for _inner in 0..8 {
                    let mut moved = false;
                    for dx in -1..=1_i32 {
                        for dz in -1..=1_i32 {
                            let Some(anchors) = spatial.get(&(home.0 + dx, home.1 + dz)) else {
                                continue;
                            };
                            for &a in anchors {
                                if a >= b {
                                    continue;
                                }
                                let pa = units[&a].position;
                                let dxv = pb.0 - pa.0;
                                let dzv = pb.1 - pa.1;
                                let distance = (dxv * dxv + dzv * dzv).sqrt();
                                let separation = (units[&a].profile.separation_m
                                    + units[&b].profile.separation_m)
                                    / 2.0;
                                if distance >= separation - SEPARATION_EPS {
                                    continue;
                                }
                                let (nx, nz, push) = if distance > 0.0 {
                                    (dxv / distance, dzv / distance, separation - distance)
                                } else {
                                    (1.0, 0.0, separation)
                                };
                                let sx = if nx >= 0.0 { 1.0 } else { -1.0 };
                                let sz = if nz >= 0.0 { 1.0 } else { -1.0 };
                                let candidates = [
                                    (pb.0 + nx * push, pb.1 + nz * push),
                                    (pb.0 + sx * push, pb.1),
                                    (pb.0, pb.1 + sz * push),
                                    (pb.0 - sx * push, pb.1),
                                    (pb.0, pb.1 - sz * push),
                                ];
                                if let Some(chosen) = candidates
                                    .into_iter()
                                    .find(|candidate| position_is_free(grid, map, *candidate))
                                {
                                    pb = chosen;
                                    moved = true;
                                    pushed = true;
                                }
                            }
                        }
                    }
                    if !moved {
                        break;
                    }
                }
                if pushed {
                    let unit_b = units.get_mut(&b).expect("id from keys");
                    unit_b.last_avoidance =
                        Some((pb.0 - unit_b.position.0, pb.1 - unit_b.position.1));
                    unit_b.position = pb;
                    pushed_any = true;
                }
            }
            if !pushed_any {
                break;
            }
        }

        // 4. Progress tracking and blocked handling. A unit whose new
        //    position is blocked or separation-clamped reverted/stayed.
        for (id, unit) in units.iter_mut() {
            let previous_position = previous[id];
            if unit.status == Some(OrderStatus::Active) {
                let moved_dx = unit.position.0 - previous_position.0;
                let moved_dz = unit.position.1 - previous_position.1;
                let moved = (moved_dx * moved_dx + moved_dz * moved_dz).sqrt();
                let speed_per_tick = unit.profile.speed_m_per_s / tick_rate as f32;
                if moved < speed_per_tick * 0.5 {
                    unit.blocked_ticks += 1;
                } else {
                    unit.blocked_ticks = 0;
                }
                if unit.blocked_ticks > RECOMPUTE_BLOCKED_TICKS {
                    // Congested arrival: the unit is at the destination area
                    // but separation keeps it just outside stop distance.
                    // It completes instead of deadlocking against parked units.
                    let arrived = unit
                        .destination
                        .map(|destination| {
                            let dx = destination.0 - unit.position.0;
                            let dz = destination.1 - unit.position.1;
                            (dx * dx + dz * dz).sqrt()
                                <= unit.profile.separation_m * 3.0 + unit.profile.stop_distance_m
                        })
                        .unwrap_or(false);
                    if arrived {
                        unit.status = Some(OrderStatus::Completed);
                        unit.path = None;
                    } else {
                        if let Some(destination) = unit.destination {
                            planner.submit(unit.id, destination);
                        }
                        planner.note_recompute();
                    }
                    unit.blocked_ticks = 0;
                }
            }
        }
    }

    /// Machine-readable navigation diagnostics for the current tick.
    #[must_use]
    pub fn diagnostics(&self) -> crate::diagnostics::NavDiagnostics {
        crate::diagnostics::NavDiagnostics::capture(self)
    }
}

/// Canonical world state, written by the system's state hooks so movement is
/// covered by checkpoint digests and snapshots (issue #34 contract).
impl CanonicalSerialize for MovementWorld {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        self.map_digest.write_canonical(sink);
        sink.write_u32(self.tick_rate);
        self.planner.write_canonical(sink);
        self.units
            .values()
            .cloned()
            .collect::<Vec<_>>()
            .write_canonical(sink);
    }
}

impl MovementWorld {
    /// Writes the canonical world state (used by the system state hooks).
    pub fn write_state(&self, sink: &mut dyn CanonicalSink) {
        CanonicalSerialize::write_canonical(self, sink);
    }

    /// Restores canonical world state, validating that the map content
    /// identity matches this world's map.
    pub fn restore_state(
        &mut self,
        reader: &mut CanonicalReader<'_>,
    ) -> Result<(), CanonicalError> {
        let map_digest = Digest::read_canonical(reader)?;
        if map_digest != self.map_digest {
            return Err(CanonicalError::InvalidState("map digest mismatch"));
        }
        let tick_rate = reader.read_u32()?;
        if tick_rate != self.tick_rate {
            return Err(CanonicalError::InvalidState("tick rate mismatch"));
        }
        let planner = crate::pathfind::PlannerState::read_canonical(reader)?;
        let units: Vec<Unit> = CanonicalDecode::read_canonical(reader)?;
        self.planner = PathPlanner::from_state(planner);
        self.units = units.into_iter().map(|unit| (unit.id, unit)).collect();
        Ok(())
    }
}
