//! The authoritative battle world: movement (via `phalanx_navigation`)
//! plus combat state, commands, events, and the match outcome. All of it is
//! canonical state, covered by checkpoint digests, snapshots, and replays
//! (issue #13; `docs/vertical-slice-scenario.md` §4–§7).

use std::collections::{BTreeMap, BTreeSet};

use phalanx_core::command::{CommandEnvelope, CommandPayload};
use phalanx_core::entity::{EntityAllocator, EntityId};
use phalanx_core::hash::{
    CanonicalDecode, CanonicalError, CanonicalReader, CanonicalSerialize, CanonicalSink, Digest,
};
use phalanx_core::simulation::Simulation;
use phalanx_navigation::{Map, MovementWorld};

use crate::defs::{Roster, UnitDef};
use crate::events::{BattleEvent, CommandFailReason};

/// A battle command issued to a unit (slice spec §4). A new command replaces
/// the unit's current one.
#[derive(Clone, Debug, PartialEq)]
pub enum BattleCommand {
    /// Path to the destination and stop.
    Move {
        /// Destination in meters.
        destination: (f32, f32),
    },
    /// Path to the destination; engage enemies in weapon range along the way.
    AttackMove {
        /// Destination in meters.
        destination: (f32, f32),
    },
    /// Engage the target until it dies or a new command replaces this one.
    Attack {
        /// Target entity.
        target: EntityId,
    },
}

impl CanonicalSerialize for BattleCommand {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        match self {
            Self::Move { destination } => {
                sink.write_u32(0);
                destination.write_canonical(sink);
            }
            Self::AttackMove { destination } => {
                sink.write_u32(1);
                destination.write_canonical(sink);
            }
            Self::Attack { target } => {
                sink.write_u32(2);
                target.write_canonical(sink);
            }
        }
    }
}

impl CanonicalDecode for BattleCommand {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        match reader.read_u32()? {
            0 => Ok(Self::Move {
                destination: CanonicalDecode::read_canonical(reader)?,
            }),
            1 => Ok(Self::AttackMove {
                destination: CanonicalDecode::read_canonical(reader)?,
            }),
            2 => Ok(Self::Attack {
                target: CanonicalDecode::read_canonical(reader)?,
            }),
            other => Err(CanonicalError::UnknownTag(other)),
        }
    }
}

/// Per-unit combat state.
#[derive(Clone, Debug, PartialEq)]
pub struct CombatState {
    /// Definition id in the roster.
    pub def_id: String,
    /// Team id (1 = Team A, 2 = Team B in the slice).
    pub team: u64,
    /// Current health.
    pub health: u32,
    /// Ticks until the weapon can fire again.
    pub cooldown_remaining: u32,
    /// The unit's current command.
    pub command: Option<BattleCommand>,
    /// The current target (validated every tick).
    pub target: Option<EntityId>,
    /// The last movement destination issued to the movement world.
    pub move_destination: Option<(f32, f32)>,
}

impl CanonicalSerialize for CombatState {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        self.def_id.write_canonical(sink);
        sink.write_u64(self.team);
        sink.write_u32(self.health);
        sink.write_u32(self.cooldown_remaining);
        self.command.write_canonical(sink);
        self.target.write_canonical(sink);
        self.move_destination.write_canonical(sink);
    }
}

impl CanonicalDecode for CombatState {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        Ok(Self {
            def_id: CanonicalDecode::read_canonical(reader)?,
            team: reader.read_u64()?,
            health: reader.read_u32()?,
            cooldown_remaining: reader.read_u32()?,
            command: CanonicalDecode::read_canonical(reader)?,
            target: CanonicalDecode::read_canonical(reader)?,
            move_destination: CanonicalDecode::read_canonical(reader)?,
        })
    }
}

/// Errors when spawning a battle unit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BattleSpawnError {
    /// Unknown definition id.
    UnknownDef,
    /// The spawn position is out of bounds or blocked.
    InvalidPosition,
}

impl std::fmt::Display for BattleSpawnError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownDef => formatter.write_str("unknown unit definition"),
            Self::InvalidPosition => formatter.write_str("invalid spawn position"),
        }
    }
}

impl std::error::Error for BattleSpawnError {}

/// The authoritative battle world.
#[derive(Clone, Debug)]
pub struct BattleWorld {
    roster: Roster,
    roster_digest: Digest,
    /// Every team that has ever fielded a unit (a wiped team has no living
    /// units and would otherwise be indistinguishable from a team that never
    /// played; needed for the match-over condition).
    teams: BTreeSet<u64>,
    /// Movement layer (positions, paths, separation).
    pub movement: MovementWorld,
    /// Per-unit combat state, ascending entity ID.
    pub combat: BTreeMap<EntityId, CombatState>,
    /// Deterministic event stream.
    pub events: Vec<BattleEvent>,
    /// Winner team once the match is over.
    pub match_over: Option<u64>,
}

impl BattleWorld {
    /// Creates an empty battle world over `map` with the given roster.
    #[must_use]
    pub fn new(
        map: &Map,
        cell_size_m: f32,
        inflate_m: f32,
        tick_rate: u32,
        planner_budget: u32,
        roster: Roster,
    ) -> Self {
        let roster_digest = roster.digest();
        Self {
            movement: MovementWorld::new(map, cell_size_m, inflate_m, tick_rate, planner_budget),
            roster,
            roster_digest,
            teams: BTreeSet::new(),
            combat: BTreeMap::new(),
            events: Vec::new(),
            match_over: None,
        }
    }

    /// The roster.
    #[must_use]
    pub const fn roster(&self) -> &Roster {
        &self.roster
    }

    /// Roster content digest.
    #[must_use]
    pub fn roster_digest(&self) -> Digest {
        self.roster_digest.clone()
    }

    /// Spawns a unit of `def_id` for `team` at `position`.
    pub fn spawn(
        &mut self,
        allocator: &mut EntityAllocator,
        position: (f32, f32),
        def_id: &str,
        team: u64,
    ) -> Result<EntityId, BattleSpawnError> {
        let def = self
            .roster
            .get(def_id)
            .ok_or(BattleSpawnError::UnknownDef)?;
        self.teams.insert(team);
        let profile = def.movement_profile();
        let id = self
            .movement
            .spawn(allocator, position, profile)
            .map_err(|_| BattleSpawnError::InvalidPosition)?;
        self.combat.insert(
            id,
            CombatState {
                def_id: def_id.to_owned(),
                team,
                health: def.health,
                cooldown_remaining: 0,
                command: None,
                target: None,
                move_destination: None,
            },
        );
        Ok(id)
    }

    /// The unit definition of a combat unit.
    #[must_use]
    pub fn def_of(&self, id: EntityId) -> Option<&UnitDef> {
        self.combat
            .get(&id)
            .and_then(|state| self.roster.get(&state.def_id))
    }

    /// All teams that have ever fielded a unit.
    #[must_use]
    pub fn teams(&self) -> &BTreeSet<u64> {
        &self.teams
    }

    /// Living unit count per team (only teams with at least one living unit
    /// appear).
    #[must_use]
    pub fn living_per_team(&self) -> BTreeMap<u64, usize> {
        let mut counts = BTreeMap::new();
        for state in self.combat.values() {
            *counts.entry(state.team).or_insert(0) += 1;
        }
        counts
    }

    /// Applies the commands assigned to the current tick (called in
    /// `Phase::ApplyCommands`). A new command replaces the unit's current
    /// one; invalid subjects/targets fail with a structured event and never
    /// resolve to a reused entity.
    pub fn apply_commands(&mut self, simulation: &Simulation<CommandPayload>) {
        let tick = simulation.tick();
        let envelopes: Vec<CommandEnvelope<CommandPayload>> =
            simulation.commands_for_current_tick().to_vec();
        for envelope in envelopes {
            let Some(subject) = envelope.subject() else {
                continue;
            };
            let Some(team) = self.combat.get(&subject).map(|state| state.team) else {
                self.events.push(BattleEvent::CommandFailed {
                    unit: subject,
                    reason: CommandFailReason::UnknownSubject,
                    tick,
                });
                continue;
            };
            let attack_valid = match envelope.payload() {
                CommandPayload::Attack { target } => self
                    .combat
                    .get(target)
                    .is_some_and(|other| other.team != team),
                _ => true,
            };
            let state = self.combat.get_mut(&subject).expect("checked above");
            match envelope.payload() {
                CommandPayload::Move { destination } => {
                    state.command = Some(BattleCommand::Move {
                        destination: *destination,
                    });
                    state.move_destination = Some(*destination);
                    if self.movement.order_move(subject, *destination).is_err() {
                        self.events.push(BattleEvent::CommandFailed {
                            unit: subject,
                            reason: CommandFailReason::UnknownSubject,
                            tick,
                        });
                    }
                }
                CommandPayload::AttackMove { destination } => {
                    state.command = Some(BattleCommand::AttackMove {
                        destination: *destination,
                    });
                    state.move_destination = Some(*destination);
                    if self.movement.order_move(subject, *destination).is_err() {
                        self.events.push(BattleEvent::CommandFailed {
                            unit: subject,
                            reason: CommandFailReason::UnknownSubject,
                            tick,
                        });
                    }
                }
                CommandPayload::Attack { target } => {
                    if attack_valid {
                        state.command = Some(BattleCommand::Attack { target: *target });
                        state.target = Some(*target);
                    } else {
                        state.command = None;
                        state.target = None;
                        self.events.push(BattleEvent::CommandFailed {
                            unit: subject,
                            reason: CommandFailReason::InvalidTarget,
                            tick,
                        });
                    }
                }
                CommandPayload::Diagnostics { .. } => {}
            }
        }
    }

    /// Steps movement (called in `Phase::ResolveMovement`): combat-driven
    /// destinations, then the movement layer.
    pub fn step_movement(&mut self) {
        let ids: Vec<EntityId> = self.combat.keys().copied().collect();
        for id in ids {
            let Some(state) = self.combat.get(&id) else {
                continue;
            };
            let desired = self.desired_destination(id, state);
            if desired != state.move_destination {
                let state = self.combat.get_mut(&id).expect("checked");
                match desired {
                    Some(destination) => {
                        if self.movement.order_move(id, destination).is_ok() {
                            state.move_destination = Some(destination);
                        }
                    }
                    None => {
                        self.movement.cancel_order(id);
                        state.move_destination = None;
                    }
                }
            }
        }
        self.movement.step();
    }

    /// The movement destination implied by the unit's command and target:
    /// hold and fire when the target is in weapon range; otherwise move
    /// (Attack chases the target, AttackMove continues to its destination).
    fn desired_destination(&self, id: EntityId, state: &CombatState) -> Option<(f32, f32)> {
        let Some(command) = &state.command else {
            return None;
        };
        let position = self.movement.unit(id).map(|unit| unit.position)?;
        let in_range = state.target.and_then(|target| {
            let target_position = self.movement.unit(target)?.position;
            let def = self.def_of(id)?;
            let dx = target_position.0 - position.0;
            let dz = target_position.1 - position.1;
            Some((dx * dx + dz * dz).sqrt() <= def.weapon.range_m)
        });
        match command {
            BattleCommand::Move { destination } => Some(*destination),
            BattleCommand::AttackMove { destination } => {
                if in_range == Some(true) {
                    None
                } else {
                    Some(*destination)
                }
            }
            BattleCommand::Attack { target } => {
                if in_range == Some(true) {
                    None
                } else {
                    self.movement.unit(*target).map(|unit| unit.position)
                }
            }
        }
    }

    /// Resolves combat for the current tick (called in `Phase::ResolveCombat`):
    /// target acquisition/validation, movement-vs-firing, hitscan damage in
    /// ascending entity-ID order, destruction with entity release, and the
    /// match-over condition (slice spec §6–§7).
    pub fn step_combat(&mut self, simulation: &mut Simulation<CommandPayload>) {
        if self.match_over.is_some() {
            return;
        }
        let tick = simulation.tick();

        // Cooldowns tick down.
        for state in self.combat.values_mut() {
            state.cooldown_remaining = state.cooldown_remaining.saturating_sub(1);
        }

        // Validate and acquire targets (nearest living enemy within sight
        // range; tie-break by lowest EntityId — spec §6.1/§9.4). Work from a
        // snapshot so no borrows conflict.
        let snapshot: Vec<(EntityId, u64, (f32, f32), f32)> = self
            .combat
            .iter()
            .map(|(id, state)| {
                let position = self
                    .movement
                    .unit(*id)
                    .map(|unit| unit.position)
                    .unwrap_or((0.0, 0.0));
                let sight = self.def_of(*id).map(|def| def.sight_range_m).unwrap_or(0.0);
                (*id, state.team, position, sight)
            })
            .collect();
        let ids: Vec<EntityId> = self.combat.keys().copied().collect();
        for id in &ids {
            let Some((_, team, position, sight)) = snapshot
                .iter()
                .find(|(other, _, _, _)| other == id)
                .copied()
            else {
                continue;
            };
            let has_attack_command = matches!(
                self.combat[id].command,
                Some(BattleCommand::Attack { .. }) | Some(BattleCommand::AttackMove { .. })
            );
            if !has_attack_command {
                self.combat.get_mut(id).expect("id").target = None;
                continue;
            }
            let current_target = self.combat[id].target;
            let target_valid = current_target.is_some_and(|target| {
                self.combat
                    .get(&target)
                    .is_some_and(|other| other.team != team)
            });
            if target_valid {
                continue;
            }
            let mut best: Option<(f32, EntityId)> = None;
            for (other_id, other_team, other_position, _) in &snapshot {
                if *other_team == team {
                    continue;
                }
                let dx = other_position.0 - position.0;
                let dz = other_position.1 - position.1;
                let distance = (dx * dx + dz * dz).sqrt();
                if distance > sight {
                    continue;
                }
                match best {
                    Some((best_distance, best_id)) => {
                        if distance < best_distance
                            || (distance == best_distance && *other_id < best_id)
                        {
                            best = Some((distance, *other_id));
                        }
                    }
                    None => best = Some((distance, *other_id)),
                }
            }
            self.combat.get_mut(id).expect("id").target = best.map(|(_, target)| target);
        }

        // Firing: units with a valid target in weapon range and no cooldown.
        // Simultaneous fire applies in ascending entity-ID order (spec §6.4).
        let volley_snapshot: Vec<(EntityId, (f32, f32))> = self
            .combat
            .keys()
            .filter_map(|id| self.movement.unit(*id).map(|unit| (*id, unit.position)))
            .collect();
        let mut firing: Vec<EntityId> = Vec::new();
        for id in &ids {
            let Some(state) = self.combat.get(id) else {
                continue;
            };
            let Some(target) = state.target else {
                continue;
            };
            let Some(def) = self.def_of(*id) else {
                continue;
            };
            let Some(position) = volley_snapshot
                .iter()
                .find(|(o, _)| o == id)
                .map(|(_, p)| *p)
            else {
                continue;
            };
            let Some(target_position) = volley_snapshot
                .iter()
                .find(|(o, _)| *o == target)
                .map(|(_, p)| *p)
            else {
                continue;
            };
            let dx = target_position.0 - position.0;
            let dz = target_position.1 - position.1;
            let distance = (dx * dx + dz * dz).sqrt();
            if distance <= def.weapon.range_m && state.cooldown_remaining == 0 {
                firing.push(*id);
            }
        }
        firing.sort();
        for source in firing {
            let Some(state) = self.combat.get(&source) else {
                continue;
            };
            let Some(target) = state.target else { continue };
            if !self.combat.contains_key(&target) {
                continue;
            }
            let def = self.def_of(source).expect("def").clone();
            let target_armor = self.def_of(target).map(|d| d.armor).unwrap_or(0);
            let damage = def.weapon.damage.saturating_sub(target_armor).max(1);
            let cooldown = def.weapon.cooldown_ticks;
            self.events.push(BattleEvent::DamageDealt {
                source,
                target,
                damage,
                tick,
            });
            {
                let state = self.combat.get_mut(&source).expect("checked");
                state.cooldown_remaining = cooldown;
            }
            let target_state = self.combat.get_mut(&target).expect("checked alive");
            target_state.health = target_state.health.saturating_sub(damage);
            let dead = target_state.health == 0;
            if dead {
                self.events
                    .push(BattleEvent::UnitDestroyed { unit: target, tick });
                self.movement.remove(target);
                self.combat.remove(&target);
                let _ = simulation.entities_mut().release(target);
                // Clear stale references to the destroyed unit.
                for other in self.combat.values_mut() {
                    if other.target == Some(target) {
                        other.target = None;
                    }
                }
            }
        }

        // Match over: first tick where a participating team has zero living
        // units (spec §7). `teams` tracks every team ever spawned because a
        // wiped team no longer appears in `living_per_team`.
        let living = self.living_per_team();
        let wiped_out = self
            .teams
            .iter()
            .any(|team| living.get(team).is_none_or(|count| *count == 0));
        if self.teams.len() >= 2 && wiped_out {
            let winner = self
                .teams
                .iter()
                .filter(|team| living.get(*team).is_some_and(|count| *count > 0))
                .max_by_key(|team| {
                    (
                        living.get(*team).copied().unwrap_or(0),
                        std::cmp::Reverse(**team),
                    )
                })
                .copied()
                .unwrap_or_else(|| self.teams.iter().next().copied().unwrap_or(0));
            self.match_over = Some(winner);
            self.events.push(BattleEvent::MatchOver { winner, tick });
        }
    }

    /// Writes the canonical battle state (system state hooks).
    pub fn write_state(&self, sink: &mut dyn CanonicalSink) {
        self.roster_digest.write_canonical(sink);
        self.teams
            .iter()
            .copied()
            .collect::<Vec<u64>>()
            .write_canonical(sink);
        self.movement.write_canonical(sink);
        self.combat
            .iter()
            .map(|(id, state)| (*id, state.clone()))
            .collect::<Vec<_>>()
            .write_canonical(sink);
        self.events.write_canonical(sink);
        self.match_over.write_canonical(sink);
    }

    /// Restores canonical battle state, validating the roster digest.
    pub fn restore_state(
        &mut self,
        reader: &mut CanonicalReader<'_>,
    ) -> Result<(), CanonicalError> {
        let roster_digest = Digest::read_canonical(reader)?;
        if roster_digest != self.roster_digest {
            return Err(CanonicalError::InvalidState("roster digest mismatch"));
        }
        let teams: Vec<u64> = CanonicalDecode::read_canonical(reader)?;
        self.teams = teams.into_iter().collect();
        self.movement.restore_state(reader)?;
        let combat: Vec<(EntityId, CombatState)> = CanonicalDecode::read_canonical(reader)?;
        self.combat = combat.into_iter().collect();
        self.events = CanonicalDecode::read_canonical(reader)?;
        self.match_over = CanonicalDecode::read_canonical(reader)?;
        Ok(())
    }
}
