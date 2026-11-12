//! Versioned, tick-indexed simulation command envelope (ADR-0001 Decision §3,
//! `RtsEngineDoc.md` §6.1 and §12.2).
//!
//! Commands are the only way external actors (players, AI, agents, replay
//! tools) change authoritative simulation state. An envelope carries a
//! request ID (for duplicate detection), a command ID (assigned by the
//! simulation in submission order), the target tick, the actor/source, and a
//! typed payload. Commands are applied only at their target tick's boundary,
//! and same-tick commands run in ascending command-ID order, so command
//! application is tick-indexed and order-stable. Invalid, late, and duplicate
//! commands are rejected with structured errors; they are never silently
//! retargeted.

use crate::hash::{CanonicalHasher, CanonicalSerialize};

/// The current command envelope schema version. Replays and savegames record
/// it so older envelopes can be rejected or migrated explicitly.
pub const ENVELOPE_VERSION: u32 = 1;

/// Identifies a client request. Retrying the same request ID is detected as a
/// duplicate instead of being applied twice.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RequestId(pub u64);

/// Identifies an accepted command. Assigned by the simulation as a monotonic
/// sequence in submission order; defines the deterministic application order
/// of commands that target the same tick.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CommandId(pub u64);

/// Identifies the actor/source that issued the command (player, AI, agent,
/// or replay tool).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ActorId(pub u64);

/// The initial command payload set. Kept deliberately small: this milestone
/// defines the envelope contract, not the full editor/gameplay API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandPayload {
    /// Diagnostic payload used by infrastructure tests and the replay harness
    /// until gameplay commands are defined. Not a gameplay command.
    Diagnostics { value: u64 },
}

/// A command submitted to the simulation, before validation and ID
/// assignment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandRequest<P> {
    /// Client request ID, used for duplicate detection.
    pub request_id: RequestId,
    /// Actor/source issuing the command.
    pub actor: ActorId,
    /// The tick at whose boundary the command must be applied.
    pub tick: u64,
    /// Typed payload.
    pub payload: P,
}

/// A validated command stored in authoritative simulation state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandEnvelope<P> {
    version: u32,
    request_id: RequestId,
    command_id: CommandId,
    actor: ActorId,
    tick: u64,
    payload: P,
}

impl<P> CommandEnvelope<P> {
    /// Creates an envelope, rejecting unsupported schema versions.
    pub fn new(
        version: u32,
        request_id: RequestId,
        command_id: CommandId,
        actor: ActorId,
        tick: u64,
        payload: P,
    ) -> Result<Self, CommandSubmitError> {
        if version != ENVELOPE_VERSION {
            return Err(CommandSubmitError::UnsupportedVersion {
                found: version,
                expected: ENVELOPE_VERSION,
            });
        }
        Ok(Self {
            version,
            request_id,
            command_id,
            actor,
            tick,
            payload,
        })
    }

    /// Returns the envelope schema version.
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// Returns the client request ID.
    #[must_use]
    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }

    /// Returns the simulation-assigned command ID.
    #[must_use]
    pub const fn command_id(&self) -> CommandId {
        self.command_id
    }

    /// Returns the actor/source that issued the command.
    #[must_use]
    pub const fn actor(&self) -> ActorId {
        self.actor
    }

    /// Returns the tick at whose boundary the command is applied.
    #[must_use]
    pub const fn tick(&self) -> u64 {
        self.tick
    }

    /// Returns the typed payload.
    #[must_use]
    pub const fn payload(&self) -> &P {
        &self.payload
    }
}

/// Structured reasons a command submission is rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandSubmitError {
    /// The envelope schema version is not supported.
    UnsupportedVersion { found: u32, expected: u32 },
    /// The target tick is the current or a past tick; the simulation never
    /// applies commands retroactively or silently retargets them.
    LateTick { requested: u64, current: u64 },
    /// The request ID was already submitted; duplicates are rejected rather
    /// than applied twice.
    DuplicateRequest { request_id: RequestId },
    /// The command ID sequence is exhausted.
    IdSpaceExhausted,
}

impl std::fmt::Display for CommandSubmitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersion { found, expected } => write!(
                formatter,
                "unsupported command envelope version {found} (expected {expected})"
            ),
            Self::LateTick { requested, current } => write!(
                formatter,
                "command targets tick {requested}, but the simulation is already at tick {current}"
            ),
            Self::DuplicateRequest { request_id } => {
                write!(formatter, "duplicate command request {}", request_id.0)
            }
            Self::IdSpaceExhausted => formatter.write_str("command ID space exhausted"),
        }
    }
}

impl std::error::Error for CommandSubmitError {}

impl CanonicalSerialize for RequestId {
    fn write_canonical(&self, hasher: &mut CanonicalHasher) {
        hasher.write_u64(self.0);
    }
}

impl CanonicalSerialize for CommandId {
    fn write_canonical(&self, hasher: &mut CanonicalHasher) {
        hasher.write_u64(self.0);
    }
}

impl CanonicalSerialize for ActorId {
    fn write_canonical(&self, hasher: &mut CanonicalHasher) {
        hasher.write_u64(self.0);
    }
}

impl CanonicalSerialize for CommandPayload {
    fn write_canonical(&self, hasher: &mut CanonicalHasher) {
        match self {
            Self::Diagnostics { value } => {
                hasher.write_u32(0);
                hasher.write_u64(*value);
            }
        }
    }
}

impl<P: CanonicalSerialize> CanonicalSerialize for CommandEnvelope<P> {
    fn write_canonical(&self, hasher: &mut CanonicalHasher) {
        hasher.write_u32(self.version);
        self.request_id.write_canonical(hasher);
        self.command_id.write_canonical(hasher);
        self.actor.write_canonical(hasher);
        hasher.write_u64(self.tick);
        self.payload.write_canonical(hasher);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trips_its_fields() {
        let envelope = CommandEnvelope::new(
            ENVELOPE_VERSION,
            RequestId(1),
            CommandId(2),
            ActorId(3),
            4,
            CommandPayload::Diagnostics { value: 5 },
        )
        .expect("current version is accepted");

        assert_eq!(envelope.version(), ENVELOPE_VERSION);
        assert_eq!(envelope.request_id(), RequestId(1));
        assert_eq!(envelope.command_id(), CommandId(2));
        assert_eq!(envelope.actor(), ActorId(3));
        assert_eq!(envelope.tick(), 4);
        assert_eq!(
            envelope.payload(),
            &CommandPayload::Diagnostics { value: 5 }
        );
    }

    #[test]
    fn unsupported_envelope_version_is_rejected() {
        assert_eq!(
            CommandEnvelope::new(0, RequestId(1), CommandId(1), ActorId(0), 1, ()),
            Err(CommandSubmitError::UnsupportedVersion {
                found: 0,
                expected: ENVELOPE_VERSION
            })
        );
        assert_eq!(
            CommandEnvelope::new(
                ENVELOPE_VERSION + 1,
                RequestId(1),
                CommandId(1),
                ActorId(0),
                1,
                ()
            ),
            Err(CommandSubmitError::UnsupportedVersion {
                found: ENVELOPE_VERSION + 1,
                expected: ENVELOPE_VERSION
            })
        );
    }

    #[test]
    fn request_and_command_ids_order_deterministically() {
        assert!(CommandId(1) < CommandId(2));
        assert!(RequestId(1) < RequestId(2));
        assert!(ActorId(1) < ActorId(2));
    }
}
