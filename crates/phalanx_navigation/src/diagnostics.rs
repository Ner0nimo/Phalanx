//! Machine-readable navigation diagnostics (`RtsEngineDoc.md` §7.5): paths,
//! blocked cells, avoidance vectors, stuck state, and planner budget stats.
//! Encodable with the versioned canonical encoding, so tests and agents can
//! consume them as bytes.

use phalanx_core::entity::EntityId;
use phalanx_core::hash::{CanonicalSerialize, CanonicalSink, canonical_encode};

use crate::movement::{MovementWorld, OrderStatus, STUCK_THRESHOLD, Unit};
use crate::pathfind::PathStats;

/// Per-unit movement diagnostics.
#[derive(Clone, Debug, PartialEq)]
pub struct UnitDiagnostics {
    /// Stable entity ID.
    pub id: EntityId,
    /// Current world position (meters).
    pub position: (f32, f32),
    /// Current order status.
    pub status: Option<OrderStatus>,
    /// Planned route cells (empty when idle).
    pub path: Vec<(u32, u32)>,
    /// Ticks without progress while ordered.
    pub blocked_ticks: u32,
    /// Whether the unit is currently stuck.
    pub stuck: bool,
    /// Last separation push applied (avoidance vector).
    pub last_avoidance: Option<(f32, f32)>,
}

impl CanonicalSerialize for UnitDiagnostics {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        self.id.write_canonical(sink);
        sink.write_u32(self.position.0.to_bits());
        sink.write_u32(self.position.1.to_bits());
        self.status.write_canonical(sink);
        self.path.write_canonical(sink);
        sink.write_u32(self.blocked_ticks);
        sink.write_bool(self.stuck);
        self.last_avoidance.write_canonical(sink);
    }
}

/// A snapshot of navigation diagnostics for one tick.
#[derive(Clone, Debug, PartialEq)]
pub struct NavDiagnostics {
    /// Grid width in cells.
    pub grid_width: u32,
    /// Grid depth in cells.
    pub grid_depth: u32,
    /// All blocked cells, row-major.
    pub blocked_cells: Vec<(u32, u32)>,
    /// Queued (unprocessed) path requests.
    pub path_queue_pending: usize,
    /// Planner budget: max requests processed per tick.
    pub path_budget_per_tick: u32,
    /// Lifetime planner counters.
    pub path_stats: PathStats,
    /// Per-unit diagnostics in ascending entity-ID order.
    pub units: Vec<UnitDiagnostics>,
}

impl NavDiagnostics {
    /// Captures diagnostics for the world's current state.
    #[must_use]
    pub fn capture(world: &MovementWorld) -> Self {
        let grid = world.grid();
        Self {
            grid_width: grid.width(),
            grid_depth: grid.depth(),
            blocked_cells: grid.blocked_cells(),
            path_queue_pending: world.planner().pending(),
            path_budget_per_tick: world.planner().max_per_tick(),
            path_stats: world.planner().stats(),
            units: world
                .units()
                .values()
                .map(UnitDiagnostics::from_unit)
                .collect(),
        }
    }

    /// Encodes the diagnostics with the versioned canonical encoding
    /// (machine-readable bytes for tests/agents).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        canonical_encode(self)
    }
}

impl UnitDiagnostics {
    fn from_unit(unit: &Unit) -> Self {
        Self {
            id: unit.id,
            position: unit.position,
            status: unit.status,
            path: unit
                .path
                .as_ref()
                .map(|path| path.cells.clone())
                .unwrap_or_default(),
            blocked_ticks: unit.blocked_ticks,
            stuck: unit.status == Some(OrderStatus::Active)
                && unit.blocked_ticks >= STUCK_THRESHOLD,
            last_avoidance: unit.last_avoidance,
        }
    }
}

impl CanonicalSerialize for NavDiagnostics {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u32(self.grid_width);
        sink.write_u32(self.grid_depth);
        self.blocked_cells.write_canonical(sink);
        sink.write_u64(self.path_queue_pending as u64);
        sink.write_u32(self.path_budget_per_tick);
        self.path_stats.write_canonical(sink);
        self.units.write_canonical(sink);
    }
}
