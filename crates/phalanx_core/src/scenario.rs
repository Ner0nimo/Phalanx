//! A small deterministic scenario for fast CI replay runs (issue #7).
//!
//! The scenario drives only authoritative state: the scenario system applies
//! each [`CommandPayload::Diagnostics`] command by allocating an entity and
//! drawing from a dedicated RNG stream, plus one ambient draw per tick, so
//! every tick's canonical state hash depends on the commands and the seed.
//! It is infrastructure validation, not gameplay.

use crate::command::{
    ActorId, CommandEnvelope, CommandPayload, CommandRequest, CommandVerdict, RequestId, SessionId,
};
use crate::replay::ReplayRecorder;
use crate::simulation::{
    Phase, Simulation, SimulationRunner, SimulationSystem, SystemId, TickRate,
};

/// Reason code used by the scenario system's application-time validation
/// (issue #55): a diagnostic payload of `u64::MAX` would overflow
/// `value + 1`, so such commands are rejected instead of panicking or
/// wrapping.
pub const DIAGNOSTIC_VALUE_OVERFLOW: u64 = 1;

/// The master RNG seed of the deterministic CI scenario.
pub const SCENARIO_SEED: u64 = 0x5CEA_1105_5EED;

/// The default number of ticks for the CI scenario.
pub const SCENARIO_TICKS: u64 = 16;

/// The tick-indexed commands of the deterministic CI scenario: eight
/// diagnostic commands targeting even ticks 2..=16.
#[must_use]
pub fn scenario_commands() -> Vec<CommandRequest<CommandPayload>> {
    (0..8_u64)
        .map(|index| CommandRequest {
            request_id: RequestId(index + 1),
            actor: ActorId(0),
            session: SessionId(0),
            tick: 2 * (index + 1),
            subject: None,
            payload: CommandPayload::Diagnostics { value: 100 + index },
        })
        .collect()
}

/// The scenario system: applies commands in [`Phase::ApplyCommands`] by
/// allocating an entity and drawing from the `"combat"` stream, and makes one
/// ambient draw from the `"ambient"` stream every tick.
pub struct ScenarioSystem;

impl SimulationSystem<CommandPayload> for ScenarioSystem {
    fn id(&self) -> SystemId {
        SystemId("scenario")
    }

    fn phase(&self) -> Phase {
        Phase::ApplyCommands
    }

    fn order(&self) -> u64 {
        0
    }

    fn name(&self) -> &'static str {
        "scenario"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn validate_command(
        &mut self,
        _simulation: &Simulation<CommandPayload>,
        envelope: &CommandEnvelope<CommandPayload>,
    ) -> CommandVerdict {
        // Issue #55: a diagnostic payload of u64::MAX would overflow
        // `value + 1`; reject it at application time with a structured reason
        // instead of panicking or wrapping.
        if let CommandPayload::Diagnostics { value: u64::MAX } = envelope.payload() {
            return CommandVerdict::Reject {
                reason: DIAGNOSTIC_VALUE_OVERFLOW,
            };
        }
        CommandVerdict::Accept
    }

    fn run(&mut self, simulation: &mut Simulation<CommandPayload>, _phase: Phase) {
        let _ambient = simulation
            .rng_mut()
            .stream("ambient")
            .expect("declared at setup")
            .next_u64();
        let values: Vec<u64> = simulation
            .commands_for_current_tick()
            .iter()
            .map(|command| match command.payload() {
                CommandPayload::Diagnostics { value } => *value,
                _ => 0,
            })
            .collect();
        for value in values {
            let _entity = simulation
                .entities_mut()
                .allocate()
                .expect("entity slot space is sufficient for the scenario");
            let stream = simulation
                .rng_mut()
                .stream("combat")
                .expect("declared at setup");
            // Checked arithmetic: u64::MAX is rejected at validation, so this
            // cannot overflow or panic; the None arm is non-panicking defense.
            let _draw = match value.checked_add(1) {
                Some(bound) => stream.next_u64_below(bound).expect("bound > 0"),
                None => stream.next_u64(),
            };
        }
    }
}

/// Registers the scenario system set on a runner.
pub fn setup_scenario(runner: &mut SimulationRunner<CommandPayload>) {
    runner
        .register_system(ScenarioSystem)
        .expect("scenario system id is unique");
    // Systems declare their RNG streams at setup (issue #47); the registry is
    // finalized on the first tick, after which unknown names are errors.
    runner
        .simulation_mut()
        .rng_mut()
        .declare("combat")
        .expect("declare combat stream");
    runner
        .simulation_mut()
        .rng_mut()
        .declare("ambient")
        .expect("declare ambient stream");
}

/// Records the deterministic CI scenario for `ticks` ticks. Only commands
/// within the recorded horizon (`tick <= ticks`) are submitted, so the
/// recording's commands are consistent with its declared end tick (issue
/// #55).
pub fn record_scenario(ticks: u64) -> crate::replay::ReplayRecording<CommandPayload> {
    let mut recorder = ReplayRecorder::new(TickRate::DEFAULT, SCENARIO_SEED);
    setup_scenario(recorder.runner_mut());
    for request in scenario_commands() {
        if request.tick > ticks {
            break;
        }
        recorder
            .submit_command(request)
            .expect("scenario commands are valid");
    }
    recorder
        .advance_ticks(ticks)
        .expect("scenario tick counter does not overflow");
    recorder.finish()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::replay::{REPLAY_FORMAT_VERSION, replay_recording};

    #[test]
    fn scenario_commands_target_even_ticks_in_order() {
        let commands = scenario_commands();
        assert_eq!(commands.len(), 8);
        for (index, request) in commands.iter().enumerate() {
            assert_eq!(request.tick, 2 * (index as u64 + 1));
            assert_eq!(request.request_id, RequestId(index as u64 + 1));
        }
    }

    #[test]
    fn scenario_is_deterministic_across_recordings() {
        let first = record_scenario(SCENARIO_TICKS);
        let second = record_scenario(SCENARIO_TICKS);
        assert_eq!(first.format_version(), REPLAY_FORMAT_VERSION);
        assert_eq!(first.tick_hashes(), second.tick_hashes());
        assert_eq!(first.commands(), second.commands());
    }

    #[test]
    fn scenario_hashes_change_every_tick() {
        let recording = record_scenario(SCENARIO_TICKS);
        let hashes = recording.tick_hashes();
        assert_eq!(hashes.len(), SCENARIO_TICKS as usize);
        for pair in hashes.windows(2) {
            assert_ne!(pair[0], pair[1], "each tick must advance the state");
        }
    }

    #[test]
    fn scenario_replays_without_divergence() {
        let recording = record_scenario(SCENARIO_TICKS);
        let report = replay_recording(&recording, SCENARIO_TICKS, &BTreeMap::new(), setup_scenario)
            .expect("replay should succeed");
        assert_eq!(report.first_divergent_tick(), None);
    }

    #[test]
    fn max_diagnostic_payload_is_rejected_not_panicked() {
        use crate::command::CommandOutcome;
        let mut runner = SimulationRunner::new(TickRate::DEFAULT, SCENARIO_SEED);
        setup_scenario(&mut runner);
        runner
            .submit_command(CommandRequest {
                request_id: RequestId(1),
                actor: ActorId(0),
                session: SessionId(0),
                tick: 1,
                subject: None,
                payload: CommandPayload::Diagnostics { value: u64::MAX },
            })
            .expect("structurally valid");
        runner
            .submit_command(CommandRequest {
                request_id: RequestId(2),
                actor: ActorId(0),
                session: SessionId(0),
                tick: 1,
                subject: None,
                payload: CommandPayload::Diagnostics {
                    value: u64::MAX - 1,
                },
            })
            .expect("structurally valid");
        runner.advance_ticks(1).expect("tick 1 runs without panic");
        let outcomes = runner.simulation().applied_outcomes();
        assert_eq!(
            runner
                .simulation()
                .command_outcome(crate::command::CommandId(0)),
            Some(CommandOutcome::Rejected {
                reason: DIAGNOSTIC_VALUE_OVERFLOW
            }),
            "u64::MAX is rejected at application time with a structured reason"
        );
        assert_eq!(
            runner
                .simulation()
                .command_outcome(crate::command::CommandId(1)),
            Some(CommandOutcome::Applied),
            "u64::MAX - 1 applies fine (no wrap, no panic)"
        );
        assert_eq!(outcomes.len(), 2);
    }

    #[test]
    fn short_recordings_submit_only_commands_within_the_horizon() {
        for ticks in [0_u64, 4, 16] {
            let recording = record_scenario(ticks);
            assert_eq!(recording.end_tick(), ticks);
            assert!(
                recording.commands().iter().all(|c| c.tick() <= ticks),
                "all recorded commands are within the recorded horizon ({ticks} ticks)"
            );
            let report = replay_recording(&recording, ticks, &BTreeMap::new(), setup_scenario)
                .expect("replay verifies");
            assert!(!report.diverged());
        }
    }
}
