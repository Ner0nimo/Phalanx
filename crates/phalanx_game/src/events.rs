//! Typed combat events for UI/test observation (issue #13;
//! `docs/vertical-slice-scenario.md` §6; `RtsEngineDoc.md` §6.4). Events are
//! deterministic outputs recorded in authoritative state (and therefore in
//! checkpoint digests and replays).

use phalanx_core::entity::{EntityId, SquadId};
use phalanx_core::hash::{
    CanonicalDecode, CanonicalError, CanonicalReader, CanonicalSerialize, CanonicalSink,
};

/// Why a battle command failed (observable, structured).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandFailReason {
    /// The command's subject is not a live unit (stale IDs never resolve to a
    /// reused entity).
    UnknownSubject,
    /// The attack target is not a live enemy.
    InvalidTarget,
    /// The command references a squad ID that does not exist (issue #110).
    UnknownSquad,
    /// A squad membership command references a non-live unit (issue #110).
    UnknownMember,
    /// A squad creation command listed no members (issue #110).
    EmptySquad,
    /// The formation template tag is unknown (issue #110).
    InvalidFormation,
}

impl CommandFailReason {
    /// The core [`phalanx_core::command::CommandOutcome::Rejected`] reason
    /// code for this reason (issue #43): battle command validators report
    /// failures with these codes, and application maps them back to the typed
    /// reason.
    pub const fn code(self) -> u64 {
        match self {
            Self::UnknownSubject => 1,
            Self::InvalidTarget => 2,
            Self::UnknownSquad => 3,
            Self::UnknownMember => 4,
            Self::EmptySquad => 5,
            Self::InvalidFormation => 6,
        }
    }

    /// Maps a validator reason code back to the typed reason; `None` for codes
    /// outside the battle validator code space.
    pub const fn from_code(code: u64) -> Option<Self> {
        match code {
            1 => Some(Self::UnknownSubject),
            2 => Some(Self::InvalidTarget),
            3 => Some(Self::UnknownSquad),
            4 => Some(Self::UnknownMember),
            5 => Some(Self::EmptySquad),
            6 => Some(Self::InvalidFormation),
            _ => None,
        }
    }
}

impl CanonicalSerialize for CommandFailReason {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u32(match self {
            Self::UnknownSubject => 0,
            Self::InvalidTarget => 1,
            Self::UnknownSquad => 2,
            Self::UnknownMember => 3,
            Self::EmptySquad => 4,
            Self::InvalidFormation => 5,
        });
    }
}

impl CanonicalDecode for CommandFailReason {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        match reader.read_u32()? {
            0 => Ok(Self::UnknownSubject),
            1 => Ok(Self::InvalidTarget),
            2 => Ok(Self::UnknownSquad),
            3 => Ok(Self::UnknownMember),
            4 => Ok(Self::EmptySquad),
            5 => Ok(Self::InvalidFormation),
            other => Err(CanonicalError::UnknownTag(other)),
        }
    }
}

/// A deterministic battle event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BattleEvent {
    /// A unit dealt damage to a target (hitscan).
    DamageDealt {
        /// The firing unit.
        source: EntityId,
        /// The target unit.
        target: EntityId,
        /// Damage applied after armor.
        damage: u32,
        /// The tick the damage was applied.
        tick: u64,
    },
    /// A unit was destroyed and its entity released.
    UnitDestroyed {
        /// The destroyed unit.
        unit: EntityId,
        /// The tick of destruction.
        tick: u64,
    },
    /// The match ended.
    MatchOver {
        /// The winning team.
        winner: u64,
        /// The tick the match ended.
        tick: u64,
    },
    /// A command failed with a structured reason.
    CommandFailed {
        /// The unit whose command failed.
        unit: EntityId,
        /// Why it failed.
        reason: CommandFailReason,
        /// The tick of failure.
        tick: u64,
    },
    /// A squad was created (issue #110).
    SquadCreated {
        /// The new squad.
        squad: SquadId,
        /// Owning team.
        team: u64,
        /// Initial members (ascending entity-ID order).
        members: Vec<EntityId>,
        /// The tick of creation.
        tick: u64,
    },
    /// A squad was disbanded (issue #110).
    SquadDisbanded {
        /// The disbanded squad.
        squad: SquadId,
        /// The tick of disbanding.
        tick: u64,
    },
    /// A squad's member set changed (issue #110).
    SquadMembersChanged {
        /// The squad.
        squad: SquadId,
        /// The new member set (ascending entity-ID order).
        members: Vec<EntityId>,
        /// The tick of change.
        tick: u64,
    },
    /// A squad's movement order completed: every member's order completed
    /// (issue #110).
    SquadOrderCompleted {
        /// The squad.
        squad: SquadId,
        /// The tick of completion.
        tick: u64,
    },
    /// A squad's movement order failed: at least one member's order failed
    /// (issue #110).
    SquadOrderFailed {
        /// The squad.
        squad: SquadId,
        /// The tick of failure.
        tick: u64,
    },
    /// A group (squad) command failed with a structured reason (issue #110).
    SquadCommandFailed {
        /// The squad the command targeted.
        squad: SquadId,
        /// Why it failed.
        reason: CommandFailReason,
        /// The tick of failure.
        tick: u64,
    },
    /// A group command that targets no existing squad (e.g. CreateSquad)
    /// failed with a structured reason (issue #110).
    GroupCommandFailed {
        /// Why it failed.
        reason: CommandFailReason,
        /// The tick of failure.
        tick: u64,
    },
}

impl CanonicalSerialize for BattleEvent {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        match self {
            Self::DamageDealt {
                source,
                target,
                damage,
                tick,
            } => {
                sink.write_u32(0);
                source.write_canonical(sink);
                target.write_canonical(sink);
                sink.write_u32(*damage);
                sink.write_u64(*tick);
            }
            Self::UnitDestroyed { unit, tick } => {
                sink.write_u32(1);
                unit.write_canonical(sink);
                sink.write_u64(*tick);
            }
            Self::MatchOver { winner, tick } => {
                sink.write_u32(2);
                sink.write_u64(*winner);
                sink.write_u64(*tick);
            }
            Self::CommandFailed { unit, reason, tick } => {
                sink.write_u32(3);
                unit.write_canonical(sink);
                reason.write_canonical(sink);
                sink.write_u64(*tick);
            }
            Self::SquadCreated {
                squad,
                team,
                members,
                tick,
            } => {
                sink.write_u32(4);
                squad.write_canonical(sink);
                sink.write_u64(*team);
                members.write_canonical(sink);
                sink.write_u64(*tick);
            }
            Self::SquadDisbanded { squad, tick } => {
                sink.write_u32(5);
                squad.write_canonical(sink);
                sink.write_u64(*tick);
            }
            Self::SquadMembersChanged {
                squad,
                members,
                tick,
            } => {
                sink.write_u32(6);
                squad.write_canonical(sink);
                members.write_canonical(sink);
                sink.write_u64(*tick);
            }
            Self::SquadOrderCompleted { squad, tick } => {
                sink.write_u32(7);
                squad.write_canonical(sink);
                sink.write_u64(*tick);
            }
            Self::SquadOrderFailed { squad, tick } => {
                sink.write_u32(8);
                squad.write_canonical(sink);
                sink.write_u64(*tick);
            }
            Self::SquadCommandFailed {
                squad,
                reason,
                tick,
            } => {
                sink.write_u32(9);
                squad.write_canonical(sink);
                reason.write_canonical(sink);
                sink.write_u64(*tick);
            }
            Self::GroupCommandFailed { reason, tick } => {
                sink.write_u32(10);
                reason.write_canonical(sink);
                sink.write_u64(*tick);
            }
        }
    }
}

impl CanonicalDecode for BattleEvent {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        match reader.read_u32()? {
            0 => Ok(Self::DamageDealt {
                source: CanonicalDecode::read_canonical(reader)?,
                target: CanonicalDecode::read_canonical(reader)?,
                damage: reader.read_u32()?,
                tick: reader.read_u64()?,
            }),
            1 => Ok(Self::UnitDestroyed {
                unit: CanonicalDecode::read_canonical(reader)?,
                tick: reader.read_u64()?,
            }),
            2 => Ok(Self::MatchOver {
                winner: reader.read_u64()?,
                tick: reader.read_u64()?,
            }),
            3 => Ok(Self::CommandFailed {
                unit: CanonicalDecode::read_canonical(reader)?,
                reason: CanonicalDecode::read_canonical(reader)?,
                tick: reader.read_u64()?,
            }),
            4 => Ok(Self::SquadCreated {
                squad: CanonicalDecode::read_canonical(reader)?,
                team: reader.read_u64()?,
                members: CanonicalDecode::read_canonical(reader)?,
                tick: reader.read_u64()?,
            }),
            5 => Ok(Self::SquadDisbanded {
                squad: CanonicalDecode::read_canonical(reader)?,
                tick: reader.read_u64()?,
            }),
            6 => Ok(Self::SquadMembersChanged {
                squad: CanonicalDecode::read_canonical(reader)?,
                members: CanonicalDecode::read_canonical(reader)?,
                tick: reader.read_u64()?,
            }),
            7 => Ok(Self::SquadOrderCompleted {
                squad: CanonicalDecode::read_canonical(reader)?,
                tick: reader.read_u64()?,
            }),
            8 => Ok(Self::SquadOrderFailed {
                squad: CanonicalDecode::read_canonical(reader)?,
                tick: reader.read_u64()?,
            }),
            9 => Ok(Self::SquadCommandFailed {
                squad: CanonicalDecode::read_canonical(reader)?,
                reason: CanonicalDecode::read_canonical(reader)?,
                tick: reader.read_u64()?,
            }),
            10 => Ok(Self::GroupCommandFailed {
                reason: CanonicalDecode::read_canonical(reader)?,
                tick: reader.read_u64()?,
            }),
            other => Err(CanonicalError::UnknownTag(other)),
        }
    }
}
