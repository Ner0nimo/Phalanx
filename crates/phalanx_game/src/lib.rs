//! Game rules and authoritative systems (issue #13): the first
//! CPU-authoritative combat loop on top of `phalanx_core` and
//! `phalanx_navigation` (`RtsEngineDoc.md` §5, §9.4;
//! `docs/vertical-slice-scenario.md` §3–§8).
//!
//! - [`defs`]: data-driven unit/weapon definitions and the slice roster.
//! - [`events`]: typed, deterministic battle events (damage, destruction,
//!   match over, command failure).
//! - [`world`]: [`BattleWorld`], the authoritative battle state — movement
//!   layer plus per-unit combat state, events, and match outcome, all
//!   canonical (de)serializable.
//! - [`system`]: the three battle systems (commands / movement / combat) and
//!   [`system::register_battle_systems`].
//! - [`scenario`]: the scripted "Crossing assault" acceptance scenario.
//!
//! Deliberately narrow: hitscan weapons only, one damage/armor interaction,
//! no full damage-type matrix, no projectiles, no GPU simulation.

pub mod defs;
pub mod events;
pub mod scenario;
pub mod snapshot;
pub mod system;
pub mod world;

pub use defs::{Roster, UnitDef, WeaponDef, slice_roster};
pub use events::{BattleEvent, CommandFailReason};
pub use scenario::{
    AUTONOMOUS_PAD_A, AUTONOMOUS_PAD_B, SCENARIO_SEED, SPAWN_PAD_A, SPAWN_PAD_B, TEAM_A, TEAM_B,
    UNITS_PER_TEAM, battle_world, crossing_assault_commands, setup_autonomous_battle, setup_battle,
    spawn_team,
};
pub use snapshot::{extract_snapshot, extract_snapshot_selected};
pub use system::{
    BattleCommandSystem, BattleMovementSystem, BattleSystem, SharedBattleWorld,
    register_battle_systems,
};
pub use world::{BattleCommand, BattleSpawnError, BattleWorld, CombatState};
