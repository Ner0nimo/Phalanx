use std::process::ExitCode;

use phalanx_core::command::CommandPayload;
use phalanx_core::hash::Digest;
use phalanx_core::scenario::{SCENARIO_SEED, scenario_commands, setup_scenario};
use phalanx_core::simulation::{SimulationRunner, TickRate};
use phalanx_game::scenario::SCENARIO_SEED as BATTLE_SEED;
use phalanx_game::{
    SPAWN_PAD_A, SPAWN_PAD_B, SharedBattleWorld, TEAM_A, TEAM_B, crossing_assault_commands,
    extract_snapshot, setup_battle, spawn_team,
};
use phalanx_navigation::{Map, crossing_map};
use phalanx_render::{
    BackendError, ClearColor, DEFAULT_CLEAR, DebugPipeline, OBSTACLE_COLOR, OffscreenTarget,
    OrthoProjection, RenderSnapshot, count_color, demo_shapes, init_headless, run_windowed,
    run_windowed_with, team_color, verify_clear_pixels_background,
};

fn main() -> ExitCode {
    match run(std::env::args().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

/// The result of one headless run: the final tick, the checkpoint digest of
/// the full authoritative state, and how many commands were applied.
struct HeadlessResult {
    tick: u64,
    digest: Digest,
    applied_commands: u64,
}

/// Runs the deterministic CI scenario through a real `SimulationRunner`
/// (issue #54): registers the scenario system, submits the scenario command
/// stream (exercising command ingress), advances exactly `ticks` ticks, and
/// reports the canonical checkpoint digest. No window, graphics API, or GPU
/// is involved.
fn headless_run(seed: u64, ticks: u64) -> Result<HeadlessResult, String> {
    let mut runner = SimulationRunner::new(TickRate::DEFAULT, seed);
    setup_scenario(&mut runner);
    for request in scenario_commands() {
        runner
            .submit_command(request)
            .map_err(|error| error.to_string())?;
    }
    runner
        .advance_ticks(ticks)
        .map_err(|error| error.to_string())?;
    Ok(HeadlessResult {
        tick: runner.simulation().tick(),
        digest: runner.canonical_digest(),
        applied_commands: runner.simulation().applied_command_count(),
    })
}

/// The autonomous battle demo (issue #63): the scripted Crossing assault run
/// through a real `SimulationRunner`, extracting a render snapshot per tick.
/// Owns the runner and the shared authoritative world; rendering only ever
/// reads through [`BattleDemo::snapshot`].
struct BattleDemo {
    runner: SimulationRunner<CommandPayload>,
    world: SharedBattleWorld,
    map: Map,
}

impl BattleDemo {
    fn new(seed: u64) -> Result<Self, String> {
        let mut runner = SimulationRunner::new(TickRate::DEFAULT, seed);
        let world = setup_battle(&mut runner);
        let ids_a = spawn_team(runner.simulation_mut(), &world, TEAM_A, SPAWN_PAD_A);
        let ids_b = spawn_team(runner.simulation_mut(), &world, TEAM_B, SPAWN_PAD_B);
        for request in crossing_assault_commands(&ids_a, &ids_b) {
            runner
                .submit_command(request)
                .map_err(|error| error.to_string())?;
        }
        Ok(Self {
            runner,
            world,
            map: crossing_map(),
        })
    }

    /// The top-down battle view over the whole map (world meters
    /// 0..width x 0..depth), adjusted to the view aspect.
    fn view(&self, aspect: f32) -> OrthoProjection {
        OrthoProjection::new(0.0, self.map.width_m, 0.0, self.map.depth_m).for_aspect(aspect)
    }

    /// Extracts the render snapshot of the current tick without stepping.
    fn snapshot(&self) -> RenderSnapshot {
        extract_snapshot(
            &self.world.borrow(),
            &self.map,
            self.runner.simulation().tick(),
        )
    }

    /// Advances one simulation tick and returns the snapshot extracted after
    /// it. Once the match is over the authoritative state is terminal, so the
    /// final snapshot is returned without stepping.
    fn step(&mut self) -> Result<RenderSnapshot, String> {
        if self.world.borrow().match_over.is_none() {
            self.runner
                .advance_ticks(1)
                .map_err(|error| error.to_string())?;
        }
        Ok(self.snapshot())
    }

    /// The winning team once the match is over.
    fn match_winner(&self) -> Option<u64> {
        self.world.borrow().match_over
    }
}

/// The offscreen verification frame size (issue #61).
const OFFSCREEN_SIZE: u32 = 64;

/// The offscreen battle frame size (issue #63): large enough that 1 m unit
/// rectangles rasterize to several pixels on the 200 m map.
const BATTLE_OFFSCREEN_SIZE: u32 = 512;

/// Runs graphical mode (issue #61). `--offscreen` renders `frames` clear
/// frames to an offscreen texture and verifies the readback pixels, so the
/// presentation path is exercised and verifiable without a display; without
/// `--offscreen` a native window presents frames until closed. On machines
/// without a GPU adapter the offscreen probe prints the structured
/// no-adapter result and exits 0 (CI convention from the graphics spike).
/// With `battle` (issue #63) the frames come from the actual simulation:
/// the Crossing assault is stepped once per rendered frame and each frame is
/// drawn from the extracted [`RenderSnapshot`].
fn run_graphical(
    offscreen: bool,
    frames: u64,
    clear: ClearColor,
    battle: bool,
    seed: u64,
) -> Result<(), String> {
    if battle {
        if frames == 0 {
            return Err("--frames must be at least 1 in --battle mode".to_owned());
        }
        return run_battle_graphical(offscreen, frames, clear, seed);
    }
    if !offscreen {
        return run_windowed(clear, "Phalanx graphical mode").map_err(|error| error.to_string());
    }
    match init_headless() {
        Err(BackendError::NoAdapter(message)) => {
            println!("ADAPTER candidate=phalanx_render found=false error=\"{message}\"");
            println!("FRAME offscreen skipped: no GPU adapter (headless probe result)");
            Ok(())
        }
        Err(error) => Err(error.to_string()),
        Ok((backend, report)) => {
            println!("{report}");
            let target = OffscreenTarget::new(&backend.device, OFFSCREEN_SIZE, OFFSCREEN_SIZE)
                .map_err(|error| error.to_string())?;
            // Issue #62: render debug geometry (not just a clear) and verify
            // both the background and every shape color in the readback.
            let mut pipeline = DebugPipeline::new(&backend.device, OffscreenTarget::FORMAT);
            let shapes = demo_shapes();
            let projection = OrthoProjection::CROSSING_VIEW.for_aspect(1.0);
            for _ in 0..frames {
                pipeline.render(
                    &backend.device,
                    &backend.queue,
                    &target.view,
                    clear.to_wgpu(),
                    &projection,
                    &shapes,
                );
            }
            let pixels = target
                .read_back(&backend.device, &backend.queue)
                .map_err(|error| error.to_string())?;
            verify_clear_pixels_background(&pixels, target.width, target.height, clear, &shapes)
                .map_err(|error| format!("offscreen frame verification failed: {error}"))?;
            for shape in &shapes {
                if count_color(&pixels, shape.color()) == 0 {
                    return Err(format!(
                        "offscreen frame verification failed: shape color {:?} not rendered",
                        shape.color()
                    ));
                }
            }
            println!(
                "FRAME offscreen frames={frames} size={}x{} clear=verified shapes=verified pixels ok",
                target.width, target.height
            );
            Ok(())
        }
    }
}

/// Runs graphical battle mode (issue #63). The Crossing assault battle is
/// advanced one simulation tick per rendered frame; every frame is rendered
/// from the actual extracted snapshot (obstacles + team-colored units).
/// Offscreen renders `frames` frames to a texture and verifies the readback
/// (background, obstacle gray, and both team colors while their units are
/// alive); windowed presents per-frame snapshots until the window closes. On
/// machines without a GPU adapter the offscreen probe prints the structured
/// no-adapter result and exits 0.
fn run_battle_graphical(
    offscreen: bool,
    frames: u64,
    clear: ClearColor,
    seed: u64,
) -> Result<(), String> {
    let mut demo = BattleDemo::new(seed)?;
    if !offscreen {
        let mut announced_match = false;
        return run_windowed_with(
            "Phalanx — Crossing assault (graphical battle)",
            move |device, queue, pipeline, view, aspect| {
                let snapshot = match demo.step() {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        eprintln!("error: battle step failed: {error}");
                        demo.snapshot()
                    }
                };
                if snapshot.tick % 20 == 0 {
                    println!(
                        "TICK tick={} entities={}",
                        snapshot.tick,
                        snapshot.entities.len()
                    );
                }
                if let Some(winner) = demo.match_winner() {
                    if !announced_match {
                        announced_match = true;
                        println!("MATCH winner={winner} tick={}", snapshot.tick);
                    }
                }
                let projection = demo.view(aspect);
                pipeline.render(
                    device,
                    queue,
                    view,
                    clear.to_wgpu(),
                    &projection,
                    &snapshot.shapes(),
                );
            },
        )
        .map_err(|error| error.to_string());
    }
    match init_headless() {
        Err(BackendError::NoAdapter(message)) => {
            println!("ADAPTER candidate=phalanx_render found=false error=\"{message}\"");
            println!("FRAME offscreen battle skipped: no GPU adapter (headless probe result)");
            Ok(())
        }
        Err(error) => Err(error.to_string()),
        Ok((backend, report)) => {
            println!("{report}");
            let target = OffscreenTarget::new(
                &backend.device,
                BATTLE_OFFSCREEN_SIZE,
                BATTLE_OFFSCREEN_SIZE,
            )
            .map_err(|error| error.to_string())?;
            let mut pipeline = DebugPipeline::new(&backend.device, OffscreenTarget::FORMAT);
            let mut snapshot = demo.snapshot();
            let mut rendered = 0_u64;
            for _ in 0..frames {
                if demo.match_winner().is_some() {
                    break;
                }
                snapshot = demo.step()?;
                rendered += 1;
                let projection = demo.view(1.0);
                pipeline.render(
                    &backend.device,
                    &backend.queue,
                    &target.view,
                    clear.to_wgpu(),
                    &projection,
                    &snapshot.shapes(),
                );
                println!(
                    "TICK frame={rendered} tick={} entities={}",
                    snapshot.tick,
                    snapshot.entities.len()
                );
                if let Some(winner) = demo.match_winner() {
                    println!("MATCH winner={winner} tick={}", snapshot.tick);
                }
            }
            let pixels = target
                .read_back(&backend.device, &backend.queue)
                .map_err(|error| error.to_string())?;
            let shapes = snapshot.shapes();
            verify_clear_pixels_background(&pixels, target.width, target.height, clear, &shapes)
                .map_err(|error| format!("offscreen battle frame verification failed: {error}"))?;
            if count_color(&pixels, OBSTACLE_COLOR) == 0 {
                return Err(
                    "offscreen battle frame verification failed: no obstacle pixels rendered"
                        .to_owned(),
                );
            }
            for team in [TEAM_A, TEAM_B] {
                if snapshot.entities.iter().any(|entity| entity.team == team)
                    && count_color(&pixels, team_color(team)) == 0
                {
                    return Err(format!(
                        "offscreen battle frame verification failed: team {team} color not rendered"
                    ));
                }
            }
            println!(
                "FRAME offscreen battle frames={rendered} size={}x{} tick={} entities={} background=verified obstacles=verified teams=verified pixels ok",
                target.width,
                target.height,
                snapshot.tick,
                snapshot.entities.len()
            );
            Ok(())
        }
    }
}

fn parse_u64(value: &str) -> Result<u64, String> {
    if let Some(hex) = value.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).map_err(|_| format!("invalid seed: {value}"))
    } else {
        value.parse().map_err(|_| format!("invalid seed: {value}"))
    }
}

fn run(args: impl Iterator<Item = String>) -> Result<(), String> {
    let arguments: Vec<String> = args.collect();
    let mut headless = false;
    let mut graphical = false;
    let mut offscreen = false;
    let mut battle = false;
    let mut frames = 3_u64;
    let mut clear = DEFAULT_CLEAR;
    let mut ticks = 8_u64;
    let mut seed: Option<u64> = None;
    let mut index = 0;

    while index < arguments.len() {
        match arguments[index].as_str() {
            "--headless" => headless = true,
            "--graphical" => graphical = true,
            "--offscreen" => offscreen = true,
            "--battle" => battle = true,
            "--frames" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or("--frames requires a non-negative integer")?;
                frames = value
                    .parse()
                    .map_err(|_| format!("invalid frame count: {value}"))?;
            }
            "--clear" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or("--clear requires RRGGBB hex (e.g. 0x18304a)")?;
                clear = ClearColor::parse(value)?;
            }
            "--ticks" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or("--ticks requires a non-negative integer")?;
                ticks = value
                    .parse()
                    .map_err(|_| format!("invalid tick count: {value}"))?;
            }
            "--seed" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or("--seed requires an unsigned integer")?;
                seed = Some(parse_u64(value)?);
            }
            "--help" | "-h" => {
                println!(
                    "Phalanx runtime (bootstrap)\n\nUSAGE:\n    phalanx_runtime --headless [--ticks COUNT] [--seed SEED]\n    phalanx_runtime --graphical [--battle] [--clear RRGGBB] [--seed SEED]\n    phalanx_runtime --graphical --offscreen [--battle] [--frames N] [--clear RRGGBB] [--seed SEED]\n\n--headless runs the deterministic CPU-only scenario through SimulationRunner (no graphics initialization). --graphical presents wgpu frames in a native window (phalanx_render); on headless machines use --graphical --offscreen, which renders frames to an offscreen texture and verifies the pixels. --battle (graphical only) runs the scripted Crossing assault through the real SimulationRunner: one simulation tick per rendered frame, each frame drawn from the extracted render snapshot of the authoritative state."
                );
                return Ok(());
            }
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
        index += 1;
    }

    if graphical {
        // Battle mode defaults to the slice scenario seed; headless defaults
        // to the core scenario seed. An explicit --seed wins in both modes.
        let seed = seed.unwrap_or(if battle { BATTLE_SEED } else { SCENARIO_SEED });
        return run_graphical(offscreen, frames, clear, battle, seed);
    }
    if battle {
        return Err("--battle requires --graphical (see --help)".to_owned());
    }
    if !headless {
        return Err(
            "this runtime supports --headless or --graphical [--offscreen] (see --help)".to_owned(),
        );
    }

    let seed = seed.unwrap_or(SCENARIO_SEED);
    let result = headless_run(seed, ticks)?;
    let hex: String = result
        .digest
        .value()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    println!(
        "Phalanx headless run: ticks={}, seed=0x{seed:016x}, digest=0x{hex}, applied_commands={}",
        result.tick, result.applied_commands
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::run;

    #[test]
    fn headless_runtime_advances_without_graphics_initialization() {
        assert!(
            run(["--headless", "--ticks", "5"]
                .into_iter()
                .map(str::to_owned))
            .is_ok()
        );
    }

    #[test]
    fn runtime_requires_explicit_headless_mode() {
        let error = run(std::iter::empty()).expect_err("missing mode must be rejected");
        assert!(error.contains("--headless"));
    }

    #[test]
    fn runtime_rejects_invalid_tick_count() {
        let error = run(["--headless", "--ticks", "many"]
            .into_iter()
            .map(str::to_owned))
        .expect_err("invalid tick count must be rejected");
        assert!(error.contains("invalid tick count"));
    }

    #[test]
    fn graphical_offscreen_mode_runs_without_a_display() {
        // With a GPU adapter this renders and verifies clear frames; without
        // one it prints the structured no-adapter probe result. Both exit 0.
        assert!(
            run(["--graphical", "--offscreen", "--frames", "2"]
                .into_iter()
                .map(str::to_owned))
            .is_ok()
        );
    }

    #[test]
    fn graphical_mode_rejects_invalid_clear_color() {
        let error = run(["--graphical", "--offscreen", "--clear", "nope"]
            .into_iter()
            .map(str::to_owned))
        .expect_err("invalid clear color must be rejected");
        assert!(error.contains("invalid clear color"));
    }

    #[test]
    fn offscreen_requires_graphical_mode() {
        let error = run(["--offscreen"].into_iter().map(str::to_owned))
            .expect_err("--offscreen without --graphical must be rejected");
        assert!(error.contains("--graphical"));
    }

    #[test]
    fn battle_requires_graphical_mode() {
        let error = run(["--battle", "--headless"].into_iter().map(str::to_owned))
            .expect_err("--battle without --graphical must be rejected");
        assert!(error.contains("--graphical"));
    }

    #[test]
    fn graphical_offscreen_battle_mode_runs_without_a_display() {
        // With a GPU adapter this steps the battle per frame and verifies the
        // snapshot pixels (background, obstacles, both team colors); without
        // one it prints the structured no-adapter probe result. Both exit 0.
        assert!(
            run(["--graphical", "--offscreen", "--battle", "--frames", "4"]
                .into_iter()
                .map(str::to_owned))
            .is_ok()
        );
    }

    #[test]
    fn battle_offscreen_rejects_zero_frames() {
        let error = run(["--graphical", "--offscreen", "--battle", "--frames", "0"]
            .into_iter()
            .map(str::to_owned))
        .expect_err("zero frames in battle mode must be rejected");
        assert!(error.contains("--frames"));
    }

    #[test]
    fn battle_demo_steps_the_real_simulation_and_snapshots_track_it() {
        use super::{BATTLE_SEED, BattleDemo};

        let mut demo = BattleDemo::new(BATTLE_SEED).expect("battle setup");
        let initial = demo.snapshot();
        assert_eq!(initial.tick, 0);
        assert_eq!(initial.entities.len(), 12, "both teams spawned");
        for _ in 0..30 {
            demo.step().expect("step");
        }
        let later = demo.snapshot();
        assert_eq!(later.tick, 30);
        assert_eq!(later.entities.len(), 12, "no deaths before first contact");
        assert_ne!(
            initial.entities, later.entities,
            "the snapshot must reflect live simulation progress (units moved)"
        );
        // Deterministic: an identical seed reproduces identical state.
        let mut twin = BattleDemo::new(BATTLE_SEED).expect("battle setup");
        for _ in 0..30 {
            twin.step().expect("step");
        }
        assert_eq!(
            demo.runner.canonical_digest(),
            twin.runner.canonical_digest(),
            "same seed + same commands = identical authoritative state"
        );
        assert_eq!(later, twin.snapshot());
    }

    #[test]
    fn headless_run_uses_the_real_runner_and_is_deterministic() {
        use super::{SCENARIO_SEED, headless_run, scenario_commands};

        // 16 ticks: all 8 scenario commands (target ticks 2..=16) are applied.
        let first = headless_run(SCENARIO_SEED, 16).expect("headless run");
        let second = headless_run(SCENARIO_SEED, 16).expect("headless run");
        assert_eq!(first.tick, 16);
        assert_eq!(first.digest, second.digest, "identical seed and inputs");
        assert_eq!(
            first.applied_commands,
            scenario_commands().len() as u64,
            "the scenario command stream went through command ingress"
        );
        let other = headless_run(SCENARIO_SEED + 1, 16).expect("headless run");
        assert_ne!(first.digest, other.digest, "a different seed diverges");
    }
}
