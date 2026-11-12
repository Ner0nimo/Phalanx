//! Deterministic navigation and movement prototype (issue #12,
//! `RtsEngineDoc.md` §7, `docs/vertical-slice-scenario.md` §2/§5).
//!
//! Layers (all CPU-side, deterministic, no wall-clock reads):
//!
//! - [`map`]: static map data, including the "Crossing" prototype map.
//! - [`grid`]: [`NavGrid`], the 1 m navigation grid built from a map with
//!   obstacles inflated by the unit radius.
//! - [`pathfind`]: A* route queries with explicit outcomes
//!   ([`RouteError`]) and the budgeted [`PathPlanner`] request queue.
//! - [`movement`]: [`MovementWorld`], the authoritative movement state:
//!   units, move orders with observable lifecycle ([`OrderStatus`]), path
//!   following, and deterministic pairwise separation.
//! - [`system`]: [`MovementSystem`], the `SimulationSystem` glue that steps
//!   the world in `Phase::ResolveMovement` and exposes its state through the
//!   canonical checkpoint digest (issue #34 contract).
//! - [`diagnostics`]: machine-readable navigation diagnostics (paths, blocked
//!   cells, avoidance, stuck state, planner budget stats), encodable with the
//!   versioned canonical encoding.
//!
//! Out of scope for this prototype: flow fields, hierarchical navigation,
//! GPU avoidance (ORCA/RVO), formation movement, and multiple movement
//! profiles beyond the data-driven [`MovementProfile`].

pub mod diagnostics;
pub mod grid;
pub mod map;
pub mod movement;
pub mod pathfind;
pub mod system;

pub use diagnostics::{NavDiagnostics, UnitDiagnostics};
pub use grid::NavGrid;
pub use map::{Map, Obstacle, crossing_map};
pub use movement::{
    GROUND, MoveOrderError, MovementProfile, MovementWorld, OrderStatus, SpawnError, Unit,
};
pub use pathfind::{Path, PathPlanner, PathRequest, PathStats, RouteError, find_path};
pub use system::MovementSystem;
