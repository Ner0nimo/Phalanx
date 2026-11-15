use std::process::ExitCode;

use phalanx_core::command::CommandPayload;
use phalanx_core::hash::Digest;
use phalanx_core::scenario::{SCENARIO_SEED, scenario_commands, setup_scenario};
use phalanx_core::simulation::{SimulationRunner, TickRate};
use phalanx_game::scenario::SCENARIO_SEED as BATTLE_SEED;
use phalanx_game::{
    AUTONOMOUS_PAD_A, AUTONOMOUS_PAD_B, SharedBattleWorld, TEAM_A, TEAM_B, extract_snapshot,
    setup_autonomous_battle, spawn_team,
};
use phalanx_navigation::{Map, crossing_map};
use phalanx_render::{
    BackendError, ClearColor, DEFAULT_CLEAR, DebugPipeline, OBSTACLE_COLOR, OffscreenTarget,
    OrthoProjection, RenderSnapshot, count_color, demo_shapes, init_headless, run_windowed,
    run_windowed_with, team_color, verify_clear_pixels_background, write_ppm,
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

/// The autonomous battle demo (issue #64): two teams spawned 20 m apart on
/// the Crossing map in the autonomous stance, with NO per-unit commands
/// submitted — units detect, approach, engage, and fight until `MatchOver`,
/// all through a real `SimulationRunner`. Rendering only ever reads through
/// [`BattleDemo::snapshot`].
struct BattleDemo {
    runner: SimulationRunner<CommandPayload>,
    world: SharedBattleWorld,
    map: Map,
}

impl BattleDemo {
    fn new(seed: u64) -> Result<Self, String> {
        let mut runner = SimulationRunner::new(TickRate::DEFAULT, seed);
        let world = setup_autonomous_battle(&mut runner);
        spawn_team(runner.simulation_mut(), &world, TEAM_A, AUTONOMOUS_PAD_A);
        spawn_team(runner.simulation_mut(), &world, TEAM_B, AUTONOMOUS_PAD_B);
        Ok(Self {
            runner,
            world,
            map: crossing_map(),
        })
    }

    /// The current simulation tick.
    fn tick(&self) -> u64 {
        self.runner.simulation().tick()
    }

    /// The top-down battle view over the whole map (world meters
    /// 0..width x 0..depth), adjusted to the view aspect.
    fn view(&self, aspect: f32) -> OrthoProjection {
        OrthoProjection::new(0.0, self.map.width_m, 0.0, self.map.depth_m).for_aspect(aspect)
    }

    /// Extracts the render snapshot of the current tick without stepping.
    fn snapshot(&self) -> RenderSnapshot {
        extract_snapshot(&self.world.borrow(), &self.map, self.tick())
    }

    /// Advances up to `n` simulation ticks. Once the match is over the
    /// authoritative state is terminal, so no further ticks are stepped.
    fn advance(&mut self, n: u64) -> Result<(), String> {
        if n > 0 && self.world.borrow().match_over.is_none() {
            self.runner
                .advance_ticks(n)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// The winning team once the match is over.
    fn match_winner(&self) -> Option<u64> {
        self.world.borrow().match_over
    }
}

/// The default battle tick budget in `--battle` mode (slice spec §8: the
/// match must end within 2000 ticks).
const BATTLE_MAX_TICKS: u64 = 2000;

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
/// With `battle` (issue #64) the frames come from the actual simulation:
/// the autonomous battle runs through a real `SimulationRunner` up to
/// `ticks` ticks (until `MatchOver`) and each frame is drawn from the
/// extracted [`RenderSnapshot`].
fn run_graphical(
    offscreen: bool,
    frames: u64,
    ticks: u64,
    clear: ClearColor,
    battle: bool,
    seed: u64,
    screenshot: Option<String>,
) -> Result<(), String> {
    if battle {
        if frames == 0 {
            return Err("--frames must be at least 1 in --battle mode".to_owned());
        }
        if screenshot.is_some() && !offscreen {
            return Err("--screenshot requires --offscreen in --battle mode".to_owned());
        }
        return run_battle_graphical(offscreen, frames, ticks, clear, seed, screenshot);
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

/// Advances `demo` in chunks of `chunk` ticks until the match reaches its
/// terminal state or `max_ticks` is exhausted, calling `on_frame` with the
/// snapshot after every chunk and logging progress. Returns the final
/// snapshot; a battle that does not terminate within `max_ticks` is a
/// structured error, not a silent pass (issue #64 acceptance criterion).
fn run_battle_to_end(
    demo: &mut BattleDemo,
    max_ticks: u64,
    chunk: u64,
    mut on_frame: impl FnMut(&RenderSnapshot),
) -> Result<RenderSnapshot, String> {
    let mut frames = 0_u64;
    loop {
        if let Some(winner) = demo.match_winner() {
            println!("MATCH winner={winner} tick={}", demo.tick());
            break;
        }
        if demo.tick() >= max_ticks {
            return Err(format!(
                "battle did not reach a terminal state within {max_ticks} ticks"
            ));
        }
        let steps = chunk.min(max_ticks - demo.tick());
        demo.advance(steps)?;
        frames += 1;
        let snapshot = demo.snapshot();
        println!(
            "TICK frame={frames} tick={} entities={}",
            snapshot.tick,
            snapshot.entities.len()
        );
        on_frame(&snapshot);
    }
    Ok(demo.snapshot())
}

/// Runs graphical battle mode (issue #64): the first autonomous battle in
/// graphical mode. Two teams spawn 20 m apart on the Crossing map in the
/// autonomous stance and NO per-unit commands are submitted — units detect,
/// approach, engage, and fight until `MatchOver`, all through a real
/// `SimulationRunner`, and every rendered frame is drawn from the actual
/// extracted [`RenderSnapshot`] (obstacles + team-colored units).
///
/// Windowed: the simulation advances at its fixed 20 Hz wall-clock rate while
/// frames render at display rate (the two rates are independent), presenting
/// until the window closes. Offscreen: the battle runs to its terminal state
/// (bounded by `ticks`), `frames` frames are rendered spread across the run,
/// and the final readback is verified: background, obstacle gray, winner team
/// color present, loser team color absent (all its units were destroyed).
/// `screenshot` writes the final frame as a PPM file (completion evidence). On
/// machines without a GPU adapter the offscreen path still runs the actual
/// simulation to its terminal state (CPU-only probe) and exits 0.
fn run_battle_graphical(
    offscreen: bool,
    frames: u64,
    ticks: u64,
    clear: ClearColor,
    seed: u64,
    screenshot: Option<String>,
) -> Result<(), String> {
    let mut demo = BattleDemo::new(seed)?;
    if !offscreen {
        // Windowed: wall-clock 20 Hz stepping inside the frame callback; the
        // render rate stays independent of the simulation rate.
        let mut last = std::time::Instant::now();
        let mut last_logged = 0_u64;
        let mut announced_match = false;
        let mut step_error: Option<String> = None;
        return run_windowed_with(
            "Phalanx — autonomous battle (graphical)",
            move |device, queue, pipeline, view, aspect| {
                let now = std::time::Instant::now();
                let elapsed = now.saturating_duration_since(last);
                last = now;
                let tps = f64::from(TickRate::DEFAULT.ticks_per_second());
                let due = (elapsed.as_secs_f64() * tps).min(100.0) as u64;
                if due > 0 && step_error.is_none() {
                    if let Err(error) = demo.advance(due) {
                        step_error = Some(error.clone());
                        eprintln!("error: battle step failed: {error}");
                    }
                }
                let snapshot = demo.snapshot();
                if snapshot.tick != last_logged && snapshot.tick % 20 == 0 {
                    last_logged = snapshot.tick;
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
    let chunk = (ticks / frames).max(1);
    match init_headless() {
        Err(BackendError::NoAdapter(message)) => {
            println!("ADAPTER candidate=phalanx_render found=false error=\"{message}\"");
            // GPU-less probe: still run the actual simulation to its terminal
            // state, so CI verifies the autonomous battle end-to-end without a
            // GPU (pixels are verified on GPU machines below).
            run_battle_to_end(&mut demo, ticks, 100, |_| {})?;
            println!("FRAME offscreen battle skipped: no GPU adapter (simulation probe result)");
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
            let projection = demo.view(1.0);
            let clear_wgpu = clear.to_wgpu();
            let snapshot = run_battle_to_end(&mut demo, ticks, chunk, |snapshot| {
                pipeline.render(
                    &backend.device,
                    &backend.queue,
                    &target.view,
                    clear_wgpu,
                    &projection,
                    &snapshot.shapes(),
                );
            })?;
            let winner = demo.match_winner().ok_or("battle ended without a winner")?;
            let loser = if winner == TEAM_A { TEAM_B } else { TEAM_A };
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
            if !snapshot.entities.iter().all(|entity| entity.team == winner) {
                return Err(
                    "offscreen battle frame verification failed: final snapshot still contains loser units"
                        .to_owned(),
                );
            }
            if count_color(&pixels, team_color(winner)) == 0 {
                return Err(format!(
                    "offscreen battle frame verification failed: winner team {winner} color not rendered"
                ));
            }
            if count_color(&pixels, team_color(loser)) != 0 {
                return Err(format!(
                    "offscreen battle frame verification failed: destroyed team {loser} still rendered"
                ));
            }
            if let Some(path) = &screenshot {
                write_ppm(
                    std::path::Path::new(path),
                    &pixels,
                    target.width,
                    target.height,
                )
                .map_err(|error| format!("screenshot write failed: {error}"))?;
                println!(
                    "SCREENSHOT path={path} size={}x{} tick={}",
                    target.width, target.height, snapshot.tick
                );
            }
            println!(
                "FRAME offscreen battle size={}x{} tick={} entities={} winner={winner} background=verified obstacles=verified winner=verified loser=absent pixels ok",
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
    let mut ticks: Option<u64> = None;
    let mut seed: Option<u64> = None;
    let mut screenshot: Option<String> = None;
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
                ticks = Some(
                    value
                        .parse()
                        .map_err(|_| format!("invalid tick count: {value}"))?,
                );
            }
            "--screenshot" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or("--screenshot requires a file path")?;
                screenshot = Some(value.clone());
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
                    "Phalanx runtime (bootstrap)\n\nUSAGE:\n    phalanx_runtime --headless [--ticks COUNT] [--seed SEED]\n    phalanx_runtime --graphical [--battle] [--clear RRGGBB] [--seed SEED]\n    phalanx_runtime --graphical --offscreen [--battle] [--frames N] [--ticks N] [--clear RRGGBB] [--seed SEED] [--screenshot PATH]\n\n--headless runs the deterministic CPU-only scenario through SimulationRunner (no graphics initialization). --graphical presents wgpu frames in a native window (phalanx_render); on headless machines use --graphical --offscreen, which renders frames to an offscreen texture and verifies the pixels. --battle (graphical only, issue #64) runs the first autonomous battle: two teams spawn on the Crossing map with no unit commands and fight to MatchOver through the real SimulationRunner; windowed steps the sim at its fixed 20 Hz wall-clock rate per rendered frame, offscreen runs to the terminal state (default --ticks 2000) and verifies the final frame pixels (background, obstacles, winner color present, loser color absent); --screenshot writes the final offscreen frame as a PPM image."
                );
                return Ok(());
            }
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
        index += 1;
    }

    if graphical {
        // Battle mode defaults to the slice scenario seed and the 2000-tick
        // battle budget; headless defaults to the core scenario seed and 8
        // ticks. Explicit --seed/--ticks win in both modes.
        let seed = seed.unwrap_or(if battle { BATTLE_SEED } else { SCENARIO_SEED });
        let ticks = ticks.unwrap_or(if battle { BATTLE_MAX_TICKS } else { 8 });
        return run_graphical(offscreen, frames, ticks, clear, battle, seed, screenshot);
    }
    if battle {
        return Err("--battle requires --graphical (see --help)".to_owned());
    }
    if screenshot.is_some() {
        return Err(
            "--screenshot requires --graphical --offscreen --battle (see --help)".to_owned(),
        );
    }
    if !headless {
        return Err(
            "this runtime supports --headless or --graphical [--offscreen] (see --help)".to_owned(),
        );
    }

    let seed = seed.unwrap_or(SCENARIO_SEED);
    let result = headless_run(seed, ticks.unwrap_or(8))?;
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
        // With a GPU adapter this runs the battle to its terminal state,
        // renders snapshot frames, and verifies the final pixels; without one
        // it still runs the actual simulation to MatchOver (CPU-only probe)
        // and exits 0.
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
    fn screenshot_requires_battle_offscreen_graphical() {
        let error = run(["--headless", "--screenshot", "out.ppm"]
            .into_iter()
            .map(str::to_owned))
        .expect_err("--screenshot outside battle mode must be rejected");
        assert!(error.contains("--screenshot"));
        let error = run(["--graphical", "--battle", "--screenshot", "out.ppm"]
            .into_iter()
            .map(str::to_owned))
        .expect_err("--screenshot without --offscreen must be rejected");
        assert!(error.contains("--offscreen"));
    }

    #[test]
    fn battle_demo_steps_the_real_simulation_and_snapshots_track_it() {
        use super::{BATTLE_SEED, BattleDemo};

        let mut demo = BattleDemo::new(BATTLE_SEED).expect("battle setup");
        assert_eq!(
            demo.runner.simulation().applied_command_count(),
            0,
            "the autonomous battle runs without any unit commands"
        );
        let initial = demo.snapshot();
        assert_eq!(initial.tick, 0);
        assert_eq!(initial.entities.len(), 12, "both teams spawned");
        for _ in 0..30 {
            demo.advance(1).expect("step");
        }
        let later = demo.snapshot();
        assert_eq!(later.tick, 30);
        assert_eq!(later.entities.len(), 12, "no deaths this early");
        assert_ne!(
            initial.entities, later.entities,
            "the snapshot must reflect live simulation progress (units moved)"
        );
        // Deterministic: an identical seed reproduces identical state.
        let mut twin = BattleDemo::new(BATTLE_SEED).expect("battle setup");
        for _ in 0..30 {
            twin.advance(1).expect("step");
        }
        assert_eq!(
            demo.runner.canonical_digest(),
            twin.runner.canonical_digest(),
            "same seed + no commands = identical authoritative state"
        );
        assert_eq!(later, twin.snapshot());
    }

    #[test]
    fn battle_demo_reaches_match_over_autonomously_and_destroyed_units_vanish() {
        use super::{BATTLE_MAX_TICKS, BATTLE_SEED, BattleDemo, TEAM_A, TEAM_B};

        let mut demo = BattleDemo::new(BATTLE_SEED).expect("battle setup");
        let final_snapshot =
            super::run_battle_to_end(&mut demo, BATTLE_MAX_TICKS, 100, |_| {}).expect("terminal");
        let winner = demo.match_winner().expect("battle reaches MatchOver");
        assert!(winner == TEAM_A || winner == TEAM_B);
        // Only the winner's units remain, and the snapshot shows exactly them.
        assert!(!final_snapshot.entities.is_empty());
        assert!(
            final_snapshot
                .entities
                .iter()
                .all(|entity| entity.team == winner),
            "destroyed units disappear from the rendered battlefield"
        );
        assert!(final_snapshot.entities.len() < 12, "units were destroyed");
        // Deterministic outcome for a fixed seed.
        let mut twin = BattleDemo::new(BATTLE_SEED).expect("battle setup");
        super::run_battle_to_end(&mut twin, BATTLE_MAX_TICKS, 100, |_| {}).expect("terminal");
        assert_eq!(
            demo.runner.canonical_digest(),
            twin.runner.canonical_digest()
        );
        assert_eq!(winner, twin.match_winner().expect("winner"));
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
