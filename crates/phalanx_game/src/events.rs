//! Typed combat events for UI/test observation (issue #13;
//! `docs/vertical-slice-scenario.md` §6; `RtsEngineDoc.md` §6.4). Events are
//! deterministic outputs recorded in authoritative state (and therefore in
//! checkpoint digests and replays).

use phalanx_core::entity::EntityId;
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
}

impl CanonicalSerialize for CommandFailReason {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u32(match self {
            Self::UnknownSubject => 0,
            Self::InvalidTarget => 1,
        });
    }
}

impl CanonicalDecode for CommandFailReason {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        match reader.read_u32()? {
            0 => Ok(Self::UnknownSubject),
            1 => Ok(Self::InvalidTarget),
            other => Err(CanonicalError::UnknownTag(other)),
        }
    }
}

/// A deterministic battle event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
            other => Err(CanonicalError::UnknownTag(other)),
        }
    }
}
