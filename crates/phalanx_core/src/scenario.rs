//! A small deterministic scenario for fast CI replay runs (issue #7).
//!
//! The scenario drives only authoritative state: the scenario system applies
//! each [`CommandPayload::Diagnostics`] command by allocating an entity and
//! drawing from a dedicated RNG stream, plus one ambient draw per tick, so
//! every tick's canonical state hash depends on the commands and the seed.
//! It is infrastructure validation, not gameplay.

use crate::command::{ActorId, CommandPayload, CommandRequest, RequestId};
use crate::replay::ReplayRecorder;
use crate::simulation::{
    Phase, Simulation, SimulationRunner, SimulationSystem, SystemId, TickRate,
};

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
            tick: 2 * (index + 1),
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

    fn run(&mut self, simulation: &mut Simulation<CommandPayload>, _phase: Phase) {
        let _ambient = simulation.rng_mut().stream("ambient").next_u64();
        let values: Vec<u64> = simulation
            .commands_for_current_tick()
            .iter()
            .map(|command| {
                let CommandPayload::Diagnostics { value } = command.payload();
                *value
            })
            .collect();
        for value in values {
            let _entity = simulation
                .entities_mut()
                .allocate()
                .expect("entity slot space is sufficient for the scenario");
            let _draw = simulation
                .rng_mut()
                .stream("combat")
                .next_u64_below(value + 1)
                .expect("value + 1 is non-zero");
        }
    }
}

/// Registers the scenario system set on a runner.
pub fn setup_scenario(runner: &mut SimulationRunner<CommandPayload>) {
    runner
        .register_system(ScenarioSystem)
        .expect("scenario system id is unique");
}

/// Records the deterministic CI scenario for `ticks` ticks.
pub fn record_scenario(ticks: u64) -> crate::replay::ReplayRecording<CommandPayload> {
    let mut recorder = ReplayRecorder::new(TickRate::DEFAULT, SCENARIO_SEED);
    setup_scenario(recorder.runner_mut());
    for request in scenario_commands() {
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
        let report = replay_recording(&recording, SCENARIO_TICKS, setup_scenario)
            .expect("replay should succeed");
        assert_eq!(report.first_divergent_tick(), None);
    }
}
