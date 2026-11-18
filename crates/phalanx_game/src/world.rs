//! The authoritative battle world: movement (via `phalanx_navigation`)
//! plus combat state, commands, events, and the match outcome. All of it is
//! canonical state, covered by checkpoint digests, snapshots, and replays
//! (issue #13; `docs/vertical-slice-scenario.md` §4–§7).

use std::collections::{BTreeMap, BTreeSet};

use phalanx_core::command::{CommandEnvelope, CommandOutcome, CommandPayload};
use phalanx_core::entity::{EntityAllocator, EntityId, SquadId};
use phalanx_core::hash::{
    CanonicalDecode, CanonicalError, CanonicalReader, CanonicalSerialize, CanonicalSink, Digest,
};
use phalanx_core::simulation::Simulation;
use phalanx_navigation::{Map, MovementWorld};

use crate::defs::{Roster, UnitDef};
use crate::events::{BattleEvent, CommandFailReason};
use crate::formation::{FormationTemplate, normalize_heading, rotate_slot};

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

/// A group-level order for a squad (issue #110). Resolving it assigns each
/// member a formation-slot destination as an ordinary per-unit command, so
/// member-level navigation, avoidance, and combat behavior are preserved.
#[derive(Clone, Debug, PartialEq)]
pub enum SquadCommand {
    /// Move to formation slots around the destination and stop.
    Move { destination: (f32, f32) },
    /// Move to formation slots around the destination; engage enemies in range.
    AttackMove { destination: (f32, f32) },
    /// Hold the current formation and engage enemies in range.
    Defend,
}

impl CanonicalSerialize for SquadCommand {
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
            Self::Defend => sink.write_u32(2),
        }
    }
}

impl CanonicalDecode for SquadCommand {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        match reader.read_u32()? {
            0 => Ok(Self::Move {
                destination: CanonicalDecode::read_canonical(reader)?,
            }),
            1 => Ok(Self::AttackMove {
                destination: CanonicalDecode::read_canonical(reader)?,
            }),
            2 => Ok(Self::Defend),
            other => Err(CanonicalError::UnknownTag(other)),
        }
    }
}

/// A squad: a stable tactical grouping with a formation template and member
/// set (issue #110; `RtsEngineDoc.md` §2.4). Membership is mirrored on each
/// unit's [`CombatState::squad`], and destroyed units are removed eagerly, so
/// no stale references survive.
#[derive(Clone, Debug, PartialEq)]
pub struct Squad {
    /// Owning team (taken from the members at creation).
    pub team: u64,
    /// Formation template.
    pub formation: FormationTemplate,
    /// Members in ascending entity-ID order (canonical member ordering).
    pub members: BTreeSet<EntityId>,
    /// Unit heading vector (forward direction of group movement).
    pub heading: (f32, f32),
    /// Formation anchor (the group destination, or the centroid for Defend).
    pub anchor: Option<(f32, f32)>,
    /// The squad's current group order.
    pub command: Option<SquadCommand>,
    /// Whether the current order's completion/failure was already reported.
    pub order_settled: bool,
}

impl CanonicalSerialize for Squad {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u64(self.team);
        self.formation.write_canonical(sink);
        self.members
            .iter()
            .copied()
            .collect::<Vec<EntityId>>()
            .write_canonical(sink);
        self.heading.write_canonical(sink);
        self.anchor.write_canonical(sink);
        self.command.write_canonical(sink);
        self.order_settled.write_canonical(sink);
    }
}

impl CanonicalDecode for Squad {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        Ok(Self {
            team: reader.read_u64()?,
            formation: CanonicalDecode::read_canonical(reader)?,
            members: {
                let members: Vec<EntityId> = CanonicalDecode::read_canonical(reader)?;
                members.into_iter().collect()
            },
            heading: CanonicalDecode::read_canonical(reader)?,
            anchor: CanonicalDecode::read_canonical(reader)?,
            command: CanonicalDecode::read_canonical(reader)?,
            order_settled: CanonicalDecode::read_canonical(reader)?,
        })
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
    /// Queued follow-up commands (issue #94): when the current command's
    /// movement order completes or fails, the next queued command activates.
    pub command_queue: Vec<BattleCommand>,
    /// The current target (validated every tick).
    pub target: Option<EntityId>,
    /// The last movement destination issued to the movement world.
    pub move_destination: Option<(f32, f32)>,
    /// The squad this unit belongs to, if any (issue #110).
    pub squad: Option<SquadId>,
}

impl CanonicalSerialize for CombatState {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        self.def_id.write_canonical(sink);
        sink.write_u64(self.team);
        sink.write_u32(self.health);
        sink.write_u32(self.cooldown_remaining);
        self.command.write_canonical(sink);
        self.command_queue.write_canonical(sink);
        self.target.write_canonical(sink);
        self.move_destination.write_canonical(sink);
        self.squad.write_canonical(sink);
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
            command_queue: CanonicalDecode::read_canonical(reader)?,
            target: CanonicalDecode::read_canonical(reader)?,
            move_destination: CanonicalDecode::read_canonical(reader)?,
            squad: CanonicalDecode::read_canonical(reader)?,
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
    /// Autonomous stance (issue #64): when set, units without a command
    /// acquire, approach, and engage enemies on their own.
    pub autonomous: bool,
    /// Squads (issue #110), ascending squad ID. IDs are assigned
    /// monotonically and never reused.
    pub squads: BTreeMap<SquadId, Squad>,
    /// Next squad index to assign.
    next_squad_index: u32,
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
            autonomous: false,
            squads: BTreeMap::new(),
            next_squad_index: 0,
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
                command_queue: Vec::new(),
                target: None,
                move_destination: None,
                squad: None,
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

    /// Enables/disables the autonomous stance (issue #64): units without a
    /// command then acquire the nearest enemy in sight, navigate toward it
    /// when outside weapon range, and hold-and-fire in range. Part of the
    /// canonical state (round-trips through write/restore).
    pub fn set_autonomous(&mut self, autonomous: bool) {
        self.autonomous = autonomous;
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
    /// one. The canonical outcome of each command was decided at this tick's
    /// boundary (lifecycle stages 3-4, issue #43): rejected commands are
    /// recorded as structured `CommandFailed` events and never applied, so
    /// stale subjects/targets never resolve to a reused entity.
    pub fn apply_commands(&mut self, simulation: &Simulation<CommandPayload>) {
        let tick = simulation.tick();
        let envelopes: Vec<CommandEnvelope<CommandPayload>> =
            simulation.commands_for_current_tick().to_vec();
        for envelope in envelopes {
            if envelope.subject().is_none() {
                // Group (squad) commands carry no per-unit subject (#110).
                self.apply_group_command(&envelope, simulation);
                continue;
            }
            let subject = envelope.subject().expect("checked above");
            if let Some(CommandOutcome::Rejected { reason }) =
                simulation.command_outcome(envelope.command_id())
            {
                let reason = CommandFailReason::from_code(reason)
                    .expect("battle validators only emit battle reason codes");
                self.events.push(BattleEvent::CommandFailed {
                    unit: subject,
                    reason,
                    tick,
                });
                continue;
            }
            let state = self
                .combat
                .get_mut(&subject)
                .expect("accepted at the tick boundary");
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
                    // Target validity was validated at this tick's boundary.
                    state.command = Some(BattleCommand::Attack { target: *target });
                    state.target = Some(*target);
                }
                CommandPayload::Diagnostics { .. } => {}
                // Group commands have no per-unit subject and are routed
                // before this match (issue #110).
                _ => {}
            }
        }
    }

    /// Steps movement (called in `Phase::ResolveMovement`): combat-driven
    /// destinations, then the movement layer. When the current command's
    /// order completes or fails, the next queued command activates (issue #94).
    pub fn step_movement(&mut self, tick: u64) {
        let ids: Vec<EntityId> = self.combat.keys().copied().collect();
        for id in ids {
            // Command queue advancement (issue #94): if the movement order
            // for the current command is done, pop the next queued command.
            if let Some(status) = self.movement.order_status(id) {
                let done = matches!(
                    status,
                    phalanx_navigation::OrderStatus::Completed
                        | phalanx_navigation::OrderStatus::Failed(_)
                );
                if done && self.command_queue_nonempty(id) {
                    self.advance_command_queue(id);
                }
            }
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
        // Squad order lifecycle (issue #110): report group completion/failure
        // exactly once per order.
        let squad_ids: Vec<SquadId> = self.squads.keys().copied().collect();
        for squad_id in squad_ids {
            let Some(squad) = self.squads.get(&squad_id) else {
                continue;
            };
            if squad.command.is_none() || squad.order_settled {
                continue;
            }
            let statuses: Vec<Option<phalanx_navigation::OrderStatus>> = squad
                .members
                .iter()
                .map(|member| self.movement.order_status(*member))
                .collect();
            if statuses
                .iter()
                .all(|status| matches!(status, Some(phalanx_navigation::OrderStatus::Completed)))
            {
                let squad_ref = self.squads.get_mut(&squad_id).expect("checked");
                squad_ref.order_settled = true;
                self.events.push(BattleEvent::SquadOrderCompleted {
                    squad: squad_id,
                    tick,
                });
            } else if statuses
                .iter()
                .any(|status| matches!(status, Some(phalanx_navigation::OrderStatus::Failed(_))))
            {
                let squad_ref = self.squads.get_mut(&squad_id).expect("checked");
                squad_ref.order_settled = true;
                squad_ref.command = None;
                self.events.push(BattleEvent::SquadOrderFailed {
                    squad: squad_id,
                    tick,
                });
            }
        }
    }

    /// Whether the unit has queued commands (issue #94).
    fn command_queue_nonempty(&self, id: EntityId) -> bool {
        self.combat
            .get(&id)
            .is_some_and(|s| !s.command_queue.is_empty())
    }

    /// Pops the next queued command into the active slot (issue #94).
    pub fn advance_command_queue(&mut self, id: EntityId) {
        let Some(state) = self.combat.get_mut(&id) else {
            return;
        };
        if let Some(next) = state.command_queue.first().cloned() {
            state.command_queue.remove(0);
            state.command = Some(next.clone());
            match &next {
                BattleCommand::Move { destination } => {
                    state.move_destination = Some(*destination);
                    let _ = self.movement.order_move(id, *destination);
                }
                BattleCommand::AttackMove { destination } => {
                    state.move_destination = Some(*destination);
                    let _ = self.movement.order_move(id, *destination);
                }
                BattleCommand::Attack { target } => {
                    state.target = Some(*target);
                }
            }
        }
    }

    /// Appends a command to the unit's queue instead of replacing the
    /// current one (issue #94, shift+right-click).
    pub fn queue_command(&mut self, id: EntityId, command: BattleCommand) {
        if let Some(state) = self.combat.get_mut(&id) {
            state.command_queue.push(command);
        }
    }

    /// Cancels the unit's active command and clears the queue (issue #94, stop key).
    pub fn cancel_commands(&mut self, id: EntityId) {
        if let Some(state) = self.combat.get_mut(&id) {
            state.command = None;
            state.command_queue.clear();
            state.target = None;
            state.move_destination = None;
        }
        self.movement.cancel_order(id);
    }

    /// The centroid (mean position) of a squad's members, or `None`.
    fn squad_centroid(&self, members: &BTreeSet<EntityId>) -> Option<(f32, f32)> {
        if members.is_empty() {
            return None;
        }
        let mut sum = (0.0_f32, 0.0_f32);
        for id in members {
            if let Some(unit) = self.movement.unit(*id) {
                sum.0 += unit.position.0;
                sum.1 += unit.position.1;
            }
        }
        let n = members.len() as f32;
        Some((sum.0 / n, sum.1 / n))
    }

    /// Removes `unit` from any squad it belongs to (destroyed/detached units
    /// never leave stale references, issue #110).
    fn remove_unit_from_squads(&mut self, unit: EntityId) {
        let squad_id = self.combat.get(&unit).and_then(|state| state.squad);
        if let Some(squad_id) = squad_id {
            if let Some(squad) = self.squads.get_mut(&squad_id) {
                squad.members.remove(&unit);
            }
        }
    }

    /// Creates a squad from live `members` with the given formation template
    /// (issue #110). Members are first detached from any other squad, so
    /// membership never aliases. Emits [`BattleEvent::SquadCreated`].
    pub fn create_squad(
        &mut self,
        members: &[EntityId],
        formation: FormationTemplate,
        tick: u64,
    ) -> Result<SquadId, CommandFailReason> {
        if members.is_empty() {
            return Err(CommandFailReason::EmptySquad);
        }
        if members.iter().any(|m| !self.combat.contains_key(m)) {
            return Err(CommandFailReason::UnknownMember);
        }
        let team = self.combat[&members[0]].team;
        for member in members {
            self.remove_unit_from_squads(*member);
            if let Some(state) = self.combat.get_mut(member) {
                state.squad = None;
            }
        }
        let id = SquadId(self.next_squad_index);
        self.next_squad_index += 1;
        let member_set: BTreeSet<EntityId> = members.iter().copied().collect();
        for member in &member_set {
            if let Some(state) = self.combat.get_mut(member) {
                state.squad = Some(id);
            }
        }
        self.squads.insert(
            id,
            Squad {
                team,
                formation,
                members: member_set.clone(),
                heading: (1.0, 0.0),
                anchor: None,
                command: None,
                order_settled: false,
            },
        );
        self.events.push(BattleEvent::SquadCreated {
            squad: id,
            team,
            members: member_set.into_iter().collect(),
            tick,
        });
        Ok(id)
    }

    /// Disbands a squad; members become unassigned (issue #110).
    pub fn disband_squad(&mut self, squad: SquadId, tick: u64) -> Result<(), CommandFailReason> {
        let Some(existing) = self.squads.remove(&squad) else {
            return Err(CommandFailReason::UnknownSquad);
        };
        for member in &existing.members {
            if let Some(state) = self.combat.get_mut(member) {
                state.squad = None;
            }
        }
        self.events
            .push(BattleEvent::SquadDisbanded { squad, tick });
        Ok(())
    }

    /// Replaces a squad's member set (issue #110): listed members are detached
    /// from any other squad first, and previous members not in the list are
    /// detached from this squad. Re-resolves the formation when a group order
    /// is active.
    pub fn set_squad_members(
        &mut self,
        squad: SquadId,
        members: Vec<EntityId>,
        tick: u64,
    ) -> Result<(), CommandFailReason> {
        if members.is_empty() {
            return Err(CommandFailReason::EmptySquad);
        }
        if members.iter().any(|m| !self.combat.contains_key(m)) {
            return Err(CommandFailReason::UnknownMember);
        }
        let Some(existing) = self.squads.get(&squad).cloned() else {
            return Err(CommandFailReason::UnknownSquad);
        };
        for member in &members {
            self.remove_unit_from_squads(*member);
        }
        for member in &existing.members {
            if !members.contains(member) {
                if let Some(state) = self.combat.get_mut(member) {
                    state.squad = None;
                }
            }
        }
        let member_set: BTreeSet<EntityId> = members.iter().copied().collect();
        for member in &member_set {
            if let Some(state) = self.combat.get_mut(member) {
                state.squad = Some(squad);
            }
        }
        let has_command = existing.command.is_some();
        let squad_ref = self.squads.get_mut(&squad).expect("checked");
        squad_ref.members = member_set.clone();
        let members_vec: Vec<EntityId> = member_set.iter().copied().collect();
        self.events.push(BattleEvent::SquadMembersChanged {
            squad,
            members: members_vec,
            tick,
        });
        if has_command {
            self.resolve_formation(squad);
        }
        Ok(())
    }

    /// Changes a squad's formation template and re-resolves slots (issue #110).
    pub fn set_squad_formation(
        &mut self,
        squad: SquadId,
        formation: FormationTemplate,
        tick: u64,
    ) -> Result<(), CommandFailReason> {
        let Some(squad_ref) = self.squads.get_mut(&squad) else {
            return Err(CommandFailReason::UnknownSquad);
        };
        squad_ref.formation = formation;
        let has_command = squad_ref.command.is_some();
        if has_command {
            self.resolve_formation(squad);
        }
        let _ = tick;
        Ok(())
    }

    /// Issues a group move/attack-move (issue #110): the squad's anchor and
    /// heading are set from the destination, and each member receives its
    /// formation-slot destination as an ordinary per-unit command.
    pub fn squad_move(
        &mut self,
        squad: SquadId,
        destination: (f32, f32),
        attack_move: bool,
        tick: u64,
    ) -> Result<(), CommandFailReason> {
        let Some(existing) = self.squads.get(&squad).cloned() else {
            return Err(CommandFailReason::UnknownSquad);
        };
        let command = if attack_move {
            SquadCommand::AttackMove { destination }
        } else {
            SquadCommand::Move { destination }
        };
        let centroid = self.squad_centroid(&existing.members);
        let heading = match centroid {
            Some(c) => normalize_heading(destination.0 - c.0, destination.1 - c.1),
            None => existing.heading,
        };
        let squad_ref = self.squads.get_mut(&squad).expect("checked");
        squad_ref.command = Some(command);
        squad_ref.anchor = Some(destination);
        squad_ref.heading = heading;
        squad_ref.order_settled = false;
        let _ = tick;
        self.resolve_formation(squad);
        Ok(())
    }

    /// Issues a group defend order (issue #110): hold the formation around
    /// the current centroid and engage enemies in range.
    pub fn squad_defend(&mut self, squad: SquadId, tick: u64) -> Result<(), CommandFailReason> {
        let Some(existing) = self.squads.get(&squad).cloned() else {
            return Err(CommandFailReason::UnknownSquad);
        };
        let centroid = self.squad_centroid(&existing.members).unwrap_or((0.0, 0.0));
        let squad_ref = self.squads.get_mut(&squad).expect("checked");
        squad_ref.command = Some(SquadCommand::Defend);
        squad_ref.anchor = Some(centroid);
        squad_ref.order_settled = false;
        let _ = tick;
        self.resolve_formation(squad);
        Ok(())
    }

    /// Recomputes each member's formation-slot destination for the squad's
    /// active group order (issue #110). Slots are anchor + heading-rotated
    /// template offsets; a slot whose nav cell is blocked falls back to the
    /// nearest free cell within a bounded radius (formation compression at
    /// chokepoints), then to the anchor's free cell — never an obstacle cell.
    fn resolve_formation(&mut self, squad_id: SquadId) {
        let Some(squad) = self.squads.get(&squad_id).cloned() else {
            return;
        };
        if squad.members.is_empty() {
            return;
        }
        let anchor = squad
            .anchor
            .or_else(|| self.squad_centroid(&squad.members))
            .unwrap_or((0.0, 0.0));
        let offsets = squad.formation.slot_offsets(squad.members.len());
        let members: Vec<EntityId> = squad.members.iter().copied().collect();
        let command_kind = match &squad.command {
            Some(SquadCommand::Move { .. }) => 0,
            Some(SquadCommand::AttackMove { .. }) | Some(SquadCommand::Defend) => 1,
            None => return, // no active group order: leave member commands alone
        };
        let anchor_cell = self
            .movement
            .grid()
            .cell_of_checked(self.movement.map(), anchor);
        for (i, member) in members.iter().enumerate() {
            let raw = rotate_slot(anchor, squad.heading, offsets[i]);
            // Clamp into the map and off blocked cells (compression fallback).
            let mut slot = raw;
            let cell = self
                .movement
                .grid()
                .cell_of_checked(self.movement.map(), slot);
            let blocked = cell.is_none_or(|c| self.movement.grid().is_blocked(c));
            if blocked {
                if let Some(free) = cell.and_then(|c| self.movement.grid().nearest_free_cell(c, 4))
                {
                    slot = self.movement.grid().cell_center(free);
                } else if let Some(free) =
                    anchor_cell.and_then(|c| self.movement.grid().nearest_free_cell(c, 4))
                {
                    slot = self.movement.grid().cell_center(free);
                }
            }
            let Some(state) = self.combat.get_mut(member) else {
                continue;
            };
            let command = if command_kind == 0 {
                BattleCommand::Move { destination: slot }
            } else {
                BattleCommand::AttackMove { destination: slot }
            };
            state.command = Some(command);
            state.command_queue.clear();
            state.move_destination = Some(slot);
            if self.movement.order_move(*member, slot).is_err() {
                state.move_destination = None;
            }
        }
    }

    /// Applies a group (squad) command with no per-unit subject (issue #110).
    fn apply_group_command(
        &mut self,
        envelope: &CommandEnvelope<CommandPayload>,
        simulation: &Simulation<CommandPayload>,
    ) {
        let tick = simulation.tick();
        let payload = envelope.payload();
        let squad_of = |p: &CommandPayload| -> Option<SquadId> {
            match p {
                CommandPayload::DisbandSquad { squad }
                | CommandPayload::SetSquadMembers { squad, .. }
                | CommandPayload::SetSquadFormation { squad, .. }
                | CommandPayload::SquadMove { squad, .. }
                | CommandPayload::SquadAttackMove { squad, .. }
                | CommandPayload::SquadDefend { squad } => Some(*squad),
                _ => None,
            }
        };
        if let Some(CommandOutcome::Rejected { reason }) =
            simulation.command_outcome(envelope.command_id())
        {
            let reason =
                CommandFailReason::from_code(reason).unwrap_or(CommandFailReason::UnknownSquad);
            match squad_of(payload) {
                Some(squad) => self.events.push(BattleEvent::SquadCommandFailed {
                    squad,
                    reason,
                    tick,
                }),
                None => self
                    .events
                    .push(BattleEvent::GroupCommandFailed { reason, tick }),
            }
            return;
        }
        let result: Result<(), CommandFailReason> = match payload {
            CommandPayload::CreateSquad { members, formation } => {
                match FormationTemplate::from_tag(*formation) {
                    Some(template) => self.create_squad(members, template, tick).map(|_| ()),
                    None => Err(CommandFailReason::InvalidFormation),
                }
            }
            CommandPayload::DisbandSquad { squad } => self.disband_squad(*squad, tick),
            CommandPayload::SetSquadMembers { squad, members } => {
                self.set_squad_members(*squad, members.clone(), tick)
            }
            CommandPayload::SetSquadFormation { squad, formation } => {
                match FormationTemplate::from_tag(*formation) {
                    Some(template) => self.set_squad_formation(*squad, template, tick),
                    None => Err(CommandFailReason::InvalidFormation),
                }
            }
            CommandPayload::SquadMove { squad, destination } => {
                self.squad_move(*squad, *destination, false, tick)
            }
            CommandPayload::SquadAttackMove { squad, destination } => {
                self.squad_move(*squad, *destination, true, tick)
            }
            CommandPayload::SquadDefend { squad } => self.squad_defend(*squad, tick),
            _ => Ok(()),
        };
        if let (Err(reason), Some(squad)) = (result, squad_of(payload)) {
            self.events.push(BattleEvent::SquadCommandFailed {
                squad,
                reason,
                tick,
            });
        }
    }

    /// The movement destination implied by the unit's command and target:
    /// hold and fire when the target is in weapon range; otherwise move
    /// (Attack chases the target, AttackMove continues to its destination).
    /// In the autonomous stance (issue #64) a unit without a command chases
    /// its acquired target until it is in weapon range, then holds.
    fn desired_destination(&self, id: EntityId, state: &CombatState) -> Option<(f32, f32)> {
        let position = self.movement.unit(id).map(|unit| unit.position)?;
        let in_range = state.target.and_then(|target| {
            let target_position = self.movement.unit(target)?.position;
            let def = self.def_of(id)?;
            let dx = target_position.0 - position.0;
            let dz = target_position.1 - position.1;
            Some((dx * dx + dz * dz).sqrt() <= def.weapon.range_m)
        });
        let Some(command) = &state.command else {
            // Autonomous stance: chase the acquired target when out of weapon
            // range, hold and fire when in range.
            if self.autonomous && in_range != Some(true) {
                return state
                    .target
                    .and_then(|target| self.movement.unit(target).map(|unit| unit.position));
            }
            return None;
        };
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
            // In the autonomous stance (issue #64) units without an attack
            // command acquire targets as well; otherwise they hold fire.
            if !has_attack_command && !self.autonomous {
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
                self.remove_unit_from_squads(target);
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
        self.squads
            .iter()
            .map(|(id, squad)| (*id, squad.clone()))
            .collect::<Vec<_>>()
            .write_canonical(sink);
        sink.write_u32(self.next_squad_index);
        self.events.write_canonical(sink);
        self.match_over.write_canonical(sink);
        self.autonomous.write_canonical(sink);
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
        let squads: Vec<(SquadId, Squad)> = CanonicalDecode::read_canonical(reader)?;
        self.squads = squads.into_iter().collect();
        self.next_squad_index = reader.read_u32()?;
        self.events = CanonicalDecode::read_canonical(reader)?;
        self.match_over = CanonicalDecode::read_canonical(reader)?;
        self.autonomous = CanonicalDecode::read_canonical(reader)?;
        Ok(())
    }
}
