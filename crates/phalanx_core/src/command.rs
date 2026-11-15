//! Versioned, tick-indexed simulation command envelope (ADR-0001 Decision §3,
//! `RtsEngineDoc.md` §6.1 and §12.2).
//!
//! Commands are the only way external actors (players, AI, agents, replay
//! tools) change authoritative simulation state. An envelope carries a
//! request ID (for duplicate detection), a command ID (assigned by the
//! simulation in submission order), the target tick, the actor/source and
//! session, and a typed payload. Commands are applied only at their target
//! tick's boundary, and same-tick commands run in ascending command-ID
//! order, so command application is tick-indexed and order-stable. Invalid,
//! late, and duplicate commands are rejected with structured errors; they
//! are never silently retargeted.
//!
//! Request identity is scoped to `(ActorId, SessionId, RequestId)` (issue
//! #41): producers generate local monotonic request IDs per session, so
//! independent actors — or the same actor in a new session — may reuse the
//! same local IDs without collision, while a retry within the same
//! actor+session is always rejected as a duplicate. The envelope schema
//! version (currently 2, which added the session field) and the simulation
//! schema version make the identity semantics explicit and versioned for
//! replay compatibility.
//!
//! The command lifecycle has explicit, ordered stages (issue #43):
//! 1. **Ingress** (`SimulationRunner::submit_command`): envelope schema,
//!    request identity/dedup, look-ahead and queue limits — structural errors
//!    are rejected before queue insertion.
//! 2. **Authorization and state-independent validation**: validator systems
//!    may reject on policy alone (e.g. actor vs. subject ownership).
//! 3. **Application-time validation** (`SimulationSystem::validate_command`):
//!    checked against the authoritative world at the command's target tick,
//!    in deterministic system execution order; the first rejection wins.
//! 4. **Deterministic result**: every queued command gets a canonical
//!    [`CommandOutcome`] (`Applied` or `Rejected { reason }`) recorded in the
//!    durable transcript, so replays reproduce accepted and rejected commands
//!    alike.

use crate::entity::EntityId;
use crate::hash::{
    CanonicalDecode, CanonicalError, CanonicalReader, CanonicalSerialize, CanonicalSink,
};

/// The current command envelope schema version. Replays and savegames record
/// it so older envelopes can be rejected or migrated explicitly. Version 2
/// added the session field to the request identity (issue #41); version 1
/// envelopes are rejected as unsupported.
pub const ENVELOPE_VERSION: u32 = 2;

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

/// Identifies the producer session. Request identity is
/// `(ActorId, SessionId, RequestId)` (issue #41): the same actor starting a
/// new session (reconnect, new match, replay tool) may reuse local request
/// IDs, while retries within one actor+session are duplicates.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionId(pub u64);

/// The initial command payload set (`docs/vertical-slice-scenario.md` §4).
/// Coordinates are `f32` meters, encoded as IEEE-754 LE bits (deterministic;
/// see ADR-0001 numeric policy).
#[derive(Clone, Debug, PartialEq)]
pub enum CommandPayload {
    /// Diagnostic payload used by infrastructure tests and the replay harness.
    /// Not a gameplay command.
    Diagnostics { value: u64 },
    /// Path to the destination and stop.
    Move { destination: (f32, f32) },
    /// Path to the destination; engage enemies that enter weapon range.
    AttackMove { destination: (f32, f32) },
    /// Engage the target entity until it dies or a new command replaces it.
    Attack { target: EntityId },
}

/// A command submitted to the simulation, before validation and ID
/// assignment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandRequest<P> {
    /// Client request ID, used for duplicate detection within the
    /// (actor, session) scope.
    pub request_id: RequestId,
    /// Actor/source issuing the command.
    pub actor: ActorId,
    /// Producer session; request IDs are monotonic per (actor, session).
    pub session: SessionId,
    /// The tick at whose boundary the command must be applied.
    pub tick: u64,
    /// The unit/entity the command applies to, when the payload is per-unit
    /// (`None` for diagnostics).
    pub subject: Option<EntityId>,
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
    session: SessionId,
    tick: u64,
    subject: Option<EntityId>,
    payload: P,
}

impl<P> CommandEnvelope<P> {
    /// Creates an envelope, rejecting unsupported schema versions.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        version: u32,
        request_id: RequestId,
        command_id: CommandId,
        actor: ActorId,
        session: SessionId,
        tick: u64,
        subject: Option<EntityId>,
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
            session,
            tick,
            subject,
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

    /// Returns the producer session of the request.
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the tick at whose boundary the command is applied.
    #[must_use]
    pub const fn tick(&self) -> u64 {
        self.tick
    }

    /// Returns the unit/entity the command applies to, if any.
    #[must_use]
    pub const fn subject(&self) -> Option<EntityId> {
        self.subject
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
    /// The request ID was already submitted (at or below the actor's accepted
    /// high-water mark); duplicates are rejected rather than applied twice.
    DuplicateRequest { request_id: RequestId },
    /// The target tick is further in the future than the configured look-ahead
    /// limit.
    LookAheadExceeded {
        requested: u64,
        current: u64,
        max_lookahead_ticks: u64,
    },
    /// The actor already has the maximum number of queued commands.
    ActorPendingFull {
        actor: ActorId,
        count: u32,
        limit: u32,
    },
    /// The simulation already has the maximum total number of queued commands.
    PendingQueueFull { count: u32, limit: u32 },
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
            Self::LookAheadExceeded {
                requested,
                current,
                max_lookahead_ticks,
            } => write!(
                formatter,
                "command targets tick {requested}, but the look-ahead limit at tick {current} is {max_lookahead_ticks} ticks"
            ),
            Self::ActorPendingFull {
                actor,
                count,
                limit,
            } => write!(
                formatter,
                "actor {} already has {count} queued commands (limit {limit})",
                actor.0
            ),
            Self::PendingQueueFull { count, limit } => {
                write!(formatter, "pending command queue is full ({count}/{limit})")
            }
            Self::IdSpaceExhausted => formatter.write_str("command ID space exhausted"),
        }
    }
}

impl std::error::Error for CommandSubmitError {}

/// A validator's verdict on one queued command at application time
/// (lifecycle stage 3, issue #43).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandVerdict {
    /// Accept; the command is applied by a system during
    /// `Phase::ApplyCommands`.
    Accept,
    /// Reject with a validator-defined reason code (the validator documents
    /// its code space); recorded canonically as [`CommandOutcome::Rejected`].
    Reject { reason: u64 },
}

/// The deterministic outcome of a queued command, decided at its target
/// tick's boundary (lifecycle stage 4, issue #43). Recorded in the durable
/// canonical transcript, so replays reproduce accepted and rejected commands
/// alike.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandOutcome {
    /// No validator rejected it; applied by a system.
    Applied,
    /// Rejected at application time with a validator-defined reason code.
    Rejected { reason: u64 },
}

impl CanonicalSerialize for CommandOutcome {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        match self {
            Self::Applied => sink.write_u32(0),
            Self::Rejected { reason } => {
                sink.write_u32(1);
                sink.write_u64(*reason);
            }
        }
    }
}

impl CanonicalDecode for CommandOutcome {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        match reader.read_u32()? {
            0 => Ok(Self::Applied),
            1 => Ok(Self::Rejected {
                reason: reader.read_u64()?,
            }),
            other => Err(CanonicalError::UnknownTag(other)),
        }
    }
}

/// Validated limits for command ingress (issue #39): maximum target-tick
/// look-ahead, pending-queue saturation per actor and in total, and the
/// in-state applied-command transcript window. Keeping these bounded is what
/// keeps memory and per-tick canonical-hash cost from growing with match
/// duration; the full applied stream stays available in replay recordings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandLimits {
    /// Maximum distance of a command's target tick from the current tick.
    pub max_lookahead_ticks: u64,
    /// Maximum queued (pending) commands per actor.
    pub max_pending_per_actor: u32,
    /// Maximum total queued (pending) commands across all actors.
    pub max_pending_total: u32,
    /// Maximum applied-command transcript entries kept in canonical state.
    pub max_applied_history: u32,
}

impl CommandLimits {
    /// Slice defaults: 30 s of look-ahead at 20 Hz, 64 queued commands per
    /// actor, 1024 queued in total, 4096 applied entries kept in state.
    pub const DEFAULT: Self = Self {
        max_lookahead_ticks: 600,
        max_pending_per_actor: 64,
        max_pending_total: 1024,
        max_applied_history: 4096,
    };

    /// Supported upper bound for [`CommandLimits::max_lookahead_ticks`]
    /// (3 minutes at 20 Hz).
    pub const MAX_LOOKAHEAD_TICKS: u64 = 3600;
    /// Supported upper bound for [`CommandLimits::max_pending_per_actor`].
    pub const MAX_PENDING_PER_ACTOR: u32 = 1024;
    /// Supported upper bound for [`CommandLimits::max_pending_total`].
    pub const MAX_PENDING_TOTAL: u32 = 65_536;
    /// Supported upper bound for [`CommandLimits::max_applied_history`].
    pub const MAX_APPLIED_HISTORY: u32 = 65_536;

    /// Validates the limits against the supported bounds.
    pub fn validate(&self) -> Result<(), InvalidCommandLimits> {
        if self.max_lookahead_ticks == 0 || self.max_lookahead_ticks > Self::MAX_LOOKAHEAD_TICKS {
            return Err(InvalidCommandLimits::LookAheadOutOfRange {
                found: self.max_lookahead_ticks,
                min: 1,
                max: Self::MAX_LOOKAHEAD_TICKS,
            });
        }
        if self.max_pending_per_actor == 0
            || self.max_pending_per_actor > Self::MAX_PENDING_PER_ACTOR
        {
            return Err(InvalidCommandLimits::PendingPerActorOutOfRange {
                found: self.max_pending_per_actor,
                min: 1,
                max: Self::MAX_PENDING_PER_ACTOR,
            });
        }
        if self.max_pending_total < self.max_pending_per_actor {
            return Err(InvalidCommandLimits::PendingTotalBelowPerActor {
                per_actor: self.max_pending_per_actor,
                total: self.max_pending_total,
            });
        }
        if self.max_pending_total > Self::MAX_PENDING_TOTAL {
            return Err(InvalidCommandLimits::PendingTotalOutOfRange {
                found: self.max_pending_total,
                max: Self::MAX_PENDING_TOTAL,
            });
        }
        if self.max_applied_history == 0 || self.max_applied_history > Self::MAX_APPLIED_HISTORY {
            return Err(InvalidCommandLimits::AppliedHistoryOutOfRange {
                found: self.max_applied_history,
                min: 1,
                max: Self::MAX_APPLIED_HISTORY,
            });
        }
        Ok(())
    }
}

/// A [`CommandLimits`] value outside the supported bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidCommandLimits {
    /// `max_lookahead_ticks` outside `1..=CommandLimits::MAX_LOOKAHEAD_TICKS`.
    LookAheadOutOfRange { found: u64, min: u64, max: u64 },
    /// `max_pending_per_actor` outside `1..=CommandLimits::MAX_PENDING_PER_ACTOR`.
    PendingPerActorOutOfRange { found: u32, min: u32, max: u32 },
    /// `max_pending_total` is smaller than `max_pending_per_actor`.
    PendingTotalBelowPerActor { per_actor: u32, total: u32 },
    /// `max_pending_total` above `CommandLimits::MAX_PENDING_TOTAL`.
    PendingTotalOutOfRange { found: u32, max: u32 },
    /// `max_applied_history` outside `1..=CommandLimits::MAX_APPLIED_HISTORY`.
    AppliedHistoryOutOfRange { found: u32, min: u32, max: u32 },
}

impl std::fmt::Display for InvalidCommandLimits {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LookAheadOutOfRange { found, min, max } => write!(
                formatter,
                "max_lookahead_ticks {found} outside {min}..={max}"
            ),
            Self::PendingPerActorOutOfRange { found, min, max } => write!(
                formatter,
                "max_pending_per_actor {found} outside {min}..={max}"
            ),
            Self::PendingTotalBelowPerActor { per_actor, total } => write!(
                formatter,
                "max_pending_total {total} is smaller than max_pending_per_actor {per_actor}"
            ),
            Self::PendingTotalOutOfRange { found, max } => {
                write!(formatter, "max_pending_total {found} above {max}")
            }
            Self::AppliedHistoryOutOfRange { found, min, max } => {
                write!(
                    formatter,
                    "max_applied_history {found} outside {min}..={max}"
                )
            }
        }
    }
}

impl std::error::Error for InvalidCommandLimits {}

/// Failure to install new [`CommandLimits`] on a runner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandLimitsError {
    /// The limits are outside the supported bounds.
    Invalid(InvalidCommandLimits),
    /// Limits can only be changed before the first tick; after that the
    /// running configuration is fixed for determinism.
    LockedAfterFirstTick,
}

impl std::fmt::Display for CommandLimitsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(error) => write!(formatter, "invalid command limits: {error}"),
            Self::LockedAfterFirstTick => {
                formatter.write_str("command limits are locked after the first tick")
            }
        }
    }
}

impl std::error::Error for CommandLimitsError {}

impl CanonicalSerialize for RequestId {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u64(self.0);
    }
}

impl CanonicalDecode for RequestId {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        Ok(Self(reader.read_u64()?))
    }
}

impl CanonicalSerialize for CommandId {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u64(self.0);
    }
}

impl CanonicalDecode for CommandId {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        Ok(Self(reader.read_u64()?))
    }
}

impl CanonicalSerialize for ActorId {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u64(self.0);
    }
}

impl CanonicalDecode for ActorId {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        Ok(Self(reader.read_u64()?))
    }
}

impl CanonicalSerialize for SessionId {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u64(self.0);
    }
}

impl CanonicalDecode for SessionId {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        Ok(Self(reader.read_u64()?))
    }
}

impl CanonicalSerialize for CommandPayload {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        match self {
            Self::Diagnostics { value } => {
                sink.write_u32(0);
                sink.write_u64(*value);
            }
            Self::Move { destination } => {
                sink.write_u32(1);
                destination.write_canonical(sink);
            }
            Self::AttackMove { destination } => {
                sink.write_u32(2);
                destination.write_canonical(sink);
            }
            Self::Attack { target } => {
                sink.write_u32(3);
                target.write_canonical(sink);
            }
        }
    }
}

impl CanonicalDecode for CommandPayload {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        match reader.read_u32()? {
            0 => Ok(Self::Diagnostics {
                value: reader.read_u64()?,
            }),
            1 => Ok(Self::Move {
                destination: CanonicalDecode::read_canonical(reader)?,
            }),
            2 => Ok(Self::AttackMove {
                destination: CanonicalDecode::read_canonical(reader)?,
            }),
            3 => Ok(Self::Attack {
                target: CanonicalDecode::read_canonical(reader)?,
            }),
            other => Err(CanonicalError::UnknownTag(other)),
        }
    }
}

impl<P: CanonicalSerialize> CanonicalSerialize for CommandEnvelope<P> {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u32(self.version);
        self.request_id.write_canonical(sink);
        self.command_id.write_canonical(sink);
        self.actor.write_canonical(sink);
        self.session.write_canonical(sink);
        sink.write_u64(self.tick);
        self.subject.write_canonical(sink);
        self.payload.write_canonical(sink);
    }
}

impl<P: CanonicalDecode> CanonicalDecode for CommandEnvelope<P> {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        let version = reader.read_u32()?;
        if version != ENVELOPE_VERSION {
            return Err(CanonicalError::UnsupportedVersion {
                found: version,
                expected: ENVELOPE_VERSION,
            });
        }
        let request_id = RequestId::read_canonical(reader)?;
        let command_id = CommandId::read_canonical(reader)?;
        let actor = ActorId::read_canonical(reader)?;
        let session = SessionId::read_canonical(reader)?;
        let tick = reader.read_u64()?;
        let subject = CanonicalDecode::read_canonical(reader)?;
        let payload = P::read_canonical(reader)?;
        Ok(Self {
            version,
            request_id,
            command_id,
            actor,
            session,
            tick,
            subject,
            payload,
        })
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
            SessionId(9),
            4,
            None,
            CommandPayload::Diagnostics { value: 5 },
        )
        .expect("current version is accepted");

        assert_eq!(envelope.version(), ENVELOPE_VERSION);
        assert_eq!(envelope.request_id(), RequestId(1));
        assert_eq!(envelope.command_id(), CommandId(2));
        assert_eq!(envelope.actor(), ActorId(3));
        assert_eq!(envelope.session(), SessionId(9));
        assert_eq!(envelope.tick(), 4);
        assert_eq!(
            envelope.payload(),
            &CommandPayload::Diagnostics { value: 5 }
        );
    }

    #[test]
    fn unsupported_envelope_version_is_rejected() {
        assert_eq!(
            CommandEnvelope::new(
                0,
                RequestId(1),
                CommandId(1),
                ActorId(0),
                SessionId(0),
                1,
                None,
                ()
            ),
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
                SessionId(0),
                1,
                None,
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
