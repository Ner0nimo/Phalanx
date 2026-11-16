//! Deterministic system execution context and structured system outputs
//! (issue #37).
//!
//! Every [`crate::simulation::SimulationSystem`] hook receives a single
//! authoritative [`TickContext`] (current tick + authoritative time-step
//! configuration) instead of capturing its own time/config values, and returns
//! a structured [`SystemOutput`] / [`SystemError`] instead of `()`:
//!
//! - Typed events and internal intents leave a system only through
//!   [`SystemOutput`], which the runner applies deterministically right after
//!   the hook: events land in the canonical system-event transcript of the
//!   [`crate::simulation::Simulation`], and intents go through the standard
//!   command ingress (explicit target tick, stable arbitration-key ordering at
//!   application, issue #42) — never through a bespoke mutation path.
//! - Failures are structured [`SystemError`]s with documented semantics: a
//!   non-fatal `Event` failure is recorded as a canonical transcript entry and
//!   the tick continues; a `Fatal` failure terminates the run deterministically
//!   (recorded canonically, so replays reproduce it). Panics are never the
//!   normal error path.

use crate::command::CommandRequest;
use crate::hash::{
    CanonicalDecode, CanonicalError, CanonicalReader, CanonicalSerialize, CanonicalSink,
};
use crate::simulation::{SystemId, TickRate};

/// The bounded window of the canonical system-event transcript kept in
/// simulation state (issue #37). The full emission count is preserved
/// separately (`emitted_event_count`), so checkpoint hash cost stays
/// independent of match duration, exactly like the applied-command
/// transcript (issue #39).
pub const MAX_EVENT_TRANSCRIPT: usize = 4096;

/// Reason code recorded when a system-emitted intent is rejected by command
/// ingress (issue #37): the intent violated the ingress contract (for example
/// a target tick at or before the current tick), which terminates the run
/// deterministically instead of silently dropping the intent.
pub const SYSTEM_ERROR_INTENT_REJECTED: u32 = 1;

/// The authoritative, deterministic execution context handed to every system
/// hook of one tick (issue #37).
///
/// This is the single source of tick/time/configuration for systems: the
/// current tick and the runner's configured [`TickRate`] come from the
/// simulation's canonical [`crate::simulation::SimulationConfig`], so no
/// system can capture a duplicated or untracked copy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TickContext {
    tick: u64,
    tick_rate: TickRate,
    dt_seconds: f32,
}

impl TickContext {
    /// Builds the context for `tick` at the given authoritative tick rate.
    #[must_use]
    pub fn new(tick: u64, tick_rate: TickRate) -> Self {
        Self {
            tick,
            tick_rate,
            // IEEE-754 division is exactly rounded, so this is deterministic
            // on every platform; it configures simulation time only and is
            // never used for wall-clock scheduling.
            dt_seconds: 1.0 / tick_rate.ticks_per_second() as f32,
        }
    }

    /// The authoritative tick currently executing.
    #[must_use]
    pub const fn tick(&self) -> u64 {
        self.tick
    }

    /// The authoritative fixed tick rate from the simulation config.
    #[must_use]
    pub const fn tick_rate(&self) -> TickRate {
        self.tick_rate
    }

    /// Simulation seconds per tick (`1 / ticks_per_second`), exactly rounded.
    #[must_use]
    pub const fn dt_seconds(&self) -> f32 {
        self.dt_seconds
    }
}

/// An internal state-changing intent emitted by a system (issue #37).
///
/// Intents are applied by the runner through the standard ingress path, so
/// they get the same validation, limits, dedup, and canonical transcript as
/// externally submitted commands — there is no bespoke mutation path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SystemIntent<P> {
    /// Emit a command through the standard command ingress. The target tick
    /// is `request.tick` and must be strictly greater than the current tick
    /// (the same rule as external ingress); within the target tick, commands
    /// apply in the stable arbitration-key order (issue #42), which is fully
    /// deterministic and independent of producer arrival order.
    Command(CommandRequest<P>),
}

/// The structured output of one system hook execution (issue #37).
///
/// The default (empty) output is what stateless hooks return.
pub struct SystemOutput<P, E = ()> {
    events: Vec<E>,
    intents: Vec<SystemIntent<P>>,
}

impl<P, E> SystemOutput<P, E> {
    /// Creates an empty output.
    #[must_use]
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            intents: Vec::new(),
        }
    }

    /// Emits a typed event for later phases / observers. Events are recorded
    /// in the canonical system-event transcript of the simulation, in
    /// emission order.
    pub fn emit_event(&mut self, event: E) {
        self.events.push(event);
    }

    /// Emits an internal intent (e.g. a command) to be applied through the
    /// standard ingress right after this hook returns.
    pub fn emit_intent(&mut self, intent: SystemIntent<P>) {
        self.intents.push(intent);
    }

    /// The events emitted by this hook, in emission order.
    #[must_use]
    pub fn events(&self) -> &[E] {
        &self.events
    }

    /// The intents emitted by this hook, in emission order.
    #[must_use]
    pub fn intents(&self) -> &[SystemIntent<P>] {
        &self.intents
    }

    /// Returns whether the hook emitted nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty() && self.intents.is_empty()
    }

    /// Consumes the output into its events and intents (used by the runner
    /// when applying the output deterministically).
    #[must_use]
    pub fn into_parts(self) -> (Vec<E>, Vec<SystemIntent<P>>) {
        (self.events, self.intents)
    }
}

impl<P, E> Default for SystemOutput<P, E> {
    fn default() -> Self {
        Self::new()
    }
}

/// A structured system failure (issue #37). Panics are not the normal error
/// path: a hook reports a deterministic failure with a machine-readable code.
///
/// Semantics:
/// - [`SystemError::Event`] is non-fatal: the runner records it as a
///   `Failure` entry in the canonical system-event transcript and the tick
///   continues.
/// - [`SystemError::Fatal`] terminates the run deterministically: it is
///   recorded canonically (transcript + termination record, both part of the
///   checkpoint digest), `SimulationRunner::step_tick` returns
///   [`crate::simulation::StepError::Terminated`], and further ticks are
///   refused. Replays reproduce the termination because it is canonical state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SystemError {
    /// Non-fatal deterministic failure: becomes a canonical transcript
    /// `Failure` event; the tick continues.
    Event {
        /// The system that failed.
        system: SystemId,
        /// Machine-readable reason code.
        code: u32,
        /// Static human-readable description.
        message: &'static str,
    },
    /// Fatal deterministic failure: terminates the run (see above).
    Fatal {
        /// The system that failed.
        system: SystemId,
        /// Machine-readable reason code.
        code: u32,
        /// Static human-readable description.
        message: &'static str,
    },
}

impl SystemError {
    /// The system that reported the failure.
    #[must_use]
    pub const fn system(&self) -> SystemId {
        match self {
            Self::Event { system, .. } | Self::Fatal { system, .. } => *system,
        }
    }

    /// The machine-readable reason code.
    #[must_use]
    pub const fn code(&self) -> u32 {
        match self {
            Self::Event { code, .. } | Self::Fatal { code, .. } => *code,
        }
    }

    /// The static human-readable description.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        match self {
            Self::Event { message, .. } | Self::Fatal { message, .. } => message,
        }
    }

    /// Returns whether this failure terminates the run.
    #[must_use]
    pub const fn is_fatal(&self) -> bool {
        matches!(self, Self::Fatal { .. })
    }
}

impl std::fmt::Display for SystemError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = if self.is_fatal() { "fatal" } else { "event" };
        write!(
            formatter,
            "{kind} system error in {}: code {} ({})",
            self.system().0,
            self.code(),
            self.message()
        )
    }
}

impl std::error::Error for SystemError {}

/// The kind of one canonical system-event transcript entry (issue #37).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SystemEventKind<E> {
    /// A typed event emitted by a system through [`SystemOutput::emit_event`].
    Output(E),
    /// A system failure recorded by the runner (non-fatal or the fatal one
    /// that terminated the run).
    Failure {
        /// Machine-readable reason code (see [`SystemError::code`]).
        code: u32,
    },
}

/// One entry of the canonical system-event transcript (issue #37): a typed
/// event emitted by a system or a recorded system failure, in emission order.
///
/// The transcript is authoritative simulation state: it is part of the
/// canonical encoding (bounded to [`MAX_EVENT_TRANSCRIPT`] entries), so
/// checkpoint digests, snapshots, and replays all observe the exact event and
/// failure ordering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemEvent<E> {
    tick: u64,
    /// Index into the runner's execution-ordered system list (the finalized
    /// system manifest); stable for the whole run.
    system_index: u32,
    kind: SystemEventKind<E>,
}

impl<E> SystemEvent<E> {
    /// Creates an `Output` transcript entry (used by the simulation when
    /// recording a system's emitted events).
    pub(crate) const fn output(tick: u64, system_index: u32, event: E) -> Self {
        Self {
            tick,
            system_index,
            kind: SystemEventKind::Output(event),
        }
    }

    /// Creates a `Failure` transcript entry (used by the simulation when
    /// recording a system error).
    pub(crate) const fn failure(tick: u64, system_index: u32, code: u32) -> Self {
        Self {
            tick,
            system_index,
            kind: SystemEventKind::Failure { code },
        }
    }

    /// The tick during which the event was emitted.
    #[must_use]
    pub const fn tick(&self) -> u64 {
        self.tick
    }

    /// Index of the emitting/failing system in the execution-ordered system
    /// list.
    #[must_use]
    pub const fn system_index(&self) -> u32 {
        self.system_index
    }

    /// The event kind (typed output or failure code).
    #[must_use]
    pub const fn kind(&self) -> &SystemEventKind<E> {
        &self.kind
    }
}

impl<E: CanonicalSerialize> CanonicalSerialize for SystemEventKind<E> {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        match self {
            Self::Output(event) => {
                sink.write_u32(0);
                event.write_canonical(sink);
            }
            Self::Failure { code } => {
                sink.write_u32(1);
                sink.write_u32(*code);
            }
        }
    }
}

impl<E: CanonicalDecode> CanonicalDecode for SystemEventKind<E> {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        match reader.read_u32()? {
            0 => Ok(Self::Output(E::read_canonical(reader)?)),
            1 => Ok(Self::Failure {
                code: reader.read_u32()?,
            }),
            _ => Err(CanonicalError::InvalidState("unknown system event kind")),
        }
    }
}

impl<E: CanonicalSerialize> CanonicalSerialize for SystemEvent<E> {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u64(self.tick);
        sink.write_u32(self.system_index);
        self.kind.write_canonical(sink);
    }
}

impl<E: CanonicalDecode> CanonicalDecode for SystemEvent<E> {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        let tick = reader.read_u64()?;
        let system_index = reader.read_u32()?;
        let kind = SystemEventKind::read_canonical(reader)?;
        Ok(Self {
            tick,
            system_index,
            kind,
        })
    }
}

/// The canonical record of a fatal system error that terminated the run
/// (issue #37). Part of the simulation's canonical state, so snapshots and
/// replays reproduce the termination exactly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SystemTermination {
    tick: u64,
    system_index: u32,
    code: u32,
}

impl SystemTermination {
    /// Creates a termination record.
    #[must_use]
    pub const fn new(tick: u64, system_index: u32, code: u32) -> Self {
        Self {
            tick,
            system_index,
            code,
        }
    }

    /// The tick at which the run terminated.
    #[must_use]
    pub const fn tick(&self) -> u64 {
        self.tick
    }

    /// Index of the failing system in the execution-ordered system list.
    #[must_use]
    pub const fn system_index(&self) -> u32 {
        self.system_index
    }

    /// The machine-readable reason code of the fatal error.
    #[must_use]
    pub const fn code(&self) -> u32 {
        self.code
    }
}

impl CanonicalSerialize for SystemTermination {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u64(self.tick);
        sink.write_u32(self.system_index);
        sink.write_u32(self.code);
    }
}

impl CanonicalDecode for SystemTermination {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        let tick = reader.read_u64()?;
        let system_index = reader.read_u32()?;
        let code = reader.read_u32()?;
        Ok(Self::new(tick, system_index, code))
    }
}
