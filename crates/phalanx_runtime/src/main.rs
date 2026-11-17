use std::process::ExitCode;

use phalanx_core::command::{ActorId, CommandPayload, CommandRequest, RequestId, SessionId};
use phalanx_core::entity::EntityId;
use phalanx_core::hash::Digest;
use phalanx_core::scenario::{SCENARIO_SEED, scenario_commands, setup_scenario};
use phalanx_core::simulation::{SimulationRunner, TickRate};
use phalanx_game::scenario::SCENARIO_SEED as BATTLE_SEED;
use phalanx_game::{
    AUTONOMOUS_PAD_A, AUTONOMOUS_PAD_B, SharedBattleWorld, TEAM_A, TEAM_B,
    extract_snapshot_selected, setup_autonomous_battle, spawn_team,
};
use phalanx_navigation::{Map, crossing_map};
use phalanx_render::{
    AMBIENT, BackendError, BattleTemporalFrame, Camera3D, ClearColor, DEFAULT_CLEAR,
    DEFAULT_PERF_WINDOW, DEFAULT_RENDER_SCALE, DIFFUSE, DebugPipeline, Discontinuity,
    FrameProfiler, FrameRenderer, FrameTimer, GROUND_COLOR, InputEvent, JitterSequence, KeyCode,
    MouseButton, OBSTACLE_COLOR, OffscreenTarget, OrthoProjection, PerfStats, RenderSnapshot,
    TemporalRenderConfig, UNIT_HEIGHT_M, build_hud_geometry, build_scene_geometry, count_color,
    count_color_family, demo_shapes, init_headless, lit_color, overlay_hud_vertices, run_windowed,
    run_windowed_with, team_color, verify_clear_pixels_background, verify_depth_ordering,
    verify_pixels_in_set, write_ppm,
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
    /// Next request ID for player-issued commands (issue #93).
    next_request_id: u64,
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
            next_request_id: 1,
        })
    }

    /// The current simulation tick.
    fn tick(&self) -> u64 {
        self.runner.simulation().tick()
    }

    /// Extracts the render snapshot of the current tick without stepping,
    /// marking `selected` entity indices (issues #90/#92).
    fn snapshot_selected(&self, selected: &[u32]) -> RenderSnapshot {
        extract_snapshot_selected(&self.world.borrow(), &self.map, self.tick(), selected)
    }

    /// Extracts the render snapshot of the current tick without stepping.
    fn snapshot(&self) -> RenderSnapshot {
        self.snapshot_selected(&[])
    }

    /// Advances up to `n` simulation ticks and returns how many actually ran.
    /// Once the match is over the authoritative state is terminal, so no
    /// further ticks are stepped (returns 0).
    fn advance(&mut self, n: u64) -> Result<u64, String> {
        if n > 0 && self.world.borrow().match_over.is_none() {
            self.runner
                .advance_ticks(n)
                .map_err(|error| error.to_string())?;
            return Ok(n);
        }
        Ok(0)
    }

    /// The winning team once the match is over.
    fn match_winner(&self) -> Option<u64> {
        self.world.borrow().match_over
    }

    /// Submits a player command for `subject` at the next tick (issue #93).
    /// The player is `ActorId(1)` (Team A), session 0.
    fn submit_player_command(
        &mut self,
        subject: EntityId,
        payload: CommandPayload,
    ) -> Result<(), String> {
        let target_tick = self.runner.simulation().tick() + 1;
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        self.runner
            .submit_command(CommandRequest {
                request_id: RequestId(request_id),
                actor: ActorId(TEAM_A),
                session: SessionId(0),
                tick: target_tick,
                subject: Some(subject),
                payload,
            })
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

/// The interactive camera (issue #91 pan/zoom, issue #104 yaw): center
/// position in world meters, zoom as the visible world height in meters, and a
/// yaw angle around the view center. Pure presentation state; the depth-tested
/// 3D perspective camera is derived from it via [`Camera::camera3d`].
struct Camera {
    /// Center of the view in world coordinates (x, z).
    center: [f32; 2],
    /// Visible world height in meters (zoom: smaller = more zoomed in).
    view_height_m: f32,
    /// Yaw around the view center in radians (Q/E rotate).
    yaw: f32,
    /// Map bounds for clamping.
    map_width_m: f32,
    map_depth_m: f32,
}

impl Camera {
    fn new(map_width_m: f32, map_depth_m: f32) -> Self {
        Self {
            center: [map_width_m / 2.0, map_depth_m / 2.0],
            view_height_m: map_depth_m,
            yaw: 0.0,
            map_width_m,
            map_depth_m,
        }
    }

    /// Pans by a screen-space delta (normalized 0..=1) converted to world meters.
    fn pan_screen(&mut self, dx_screen: f32, dy_screen: f32, aspect: f32) {
        let world_dx = dx_screen * self.view_height_m * aspect;
        let world_dy = dy_screen * self.view_height_m;
        self.center[0] = (self.center[0] + world_dx).clamp(0.0, self.map_width_m);
        self.center[1] = (self.center[1] + world_dy).clamp(0.0, self.map_depth_m);
    }

    /// Zooms by a factor (wheel delta): positive zooms in (smaller view).
    fn zoom(&mut self, wheel_delta: f32) {
        let factor = 1.0 - wheel_delta * 0.1;
        self.view_height_m = (self.view_height_m * factor).clamp(20.0, self.map_depth_m);
    }

    /// Rotates the view around the center (yaw in radians).
    fn rotate(&mut self, delta_rad: f32) {
        self.yaw += delta_rad;
    }

    /// Derives the perspective 3D camera (issue #104) from the pan/zoom/yaw
    /// state, preserving the existing interaction semantics.
    fn camera3d(&self) -> Camera3D {
        Camera3D::from_view(self.center, self.view_height_m, self.yaw)
    }
}

/// Player selection state (issue #90): selected entity indices and an
/// in-progress drag box (normalized screen coordinates).
#[derive(Default)]
struct SelectionState {
    /// Currently selected entity indices.
    selected: Vec<u32>,
    /// Drag-box start position (normalized screen), if a left-drag is in progress.
    drag_start: Option<[f32; 2]>,
}

/// The default battle tick budget in `--battle` mode (slice spec §8: the
/// match must end within 2000 ticks).
const BATTLE_MAX_TICKS: u64 = 2000;

/// The offscreen verification frame size (issue #61).
const OFFSCREEN_SIZE: u32 = 64;

/// The offscreen battle frame size (issue #63): large enough that 1 m unit
/// rectangles rasterize to several pixels on the 200 m map.
const BATTLE_OFFSCREEN_SIZE: u32 = 512;

/// Parsed graphical-mode options (issue #61, extended by #63/#64/#88/#97).
struct GraphicalOptions {
    offscreen: bool,
    frames: u64,
    ticks: u64,
    clear: ClearColor,
    battle: bool,
    seed: u64,
    screenshot: Option<String>,
    perf_overlay: bool,
    render_scale: f32,
}

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
fn run_graphical(options: GraphicalOptions) -> Result<(), String> {
    if options.battle {
        if options.frames == 0 {
            return Err("--frames must be at least 1 in --battle mode".to_owned());
        }
        if options.screenshot.is_some() && !options.offscreen {
            return Err("--screenshot requires --offscreen in --battle mode".to_owned());
        }
        return run_battle_graphical(options);
    }
    let GraphicalOptions {
        offscreen,
        frames,
        clear,
        ..
    } = options;
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
    profiler: &mut FrameProfiler,
    mut on_frame: impl FnMut(&RenderSnapshot, &mut FrameTimer, &PerfStats),
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
        // Issue #88: per-stage frame timing (simulation / snapshot stages here,
        // graphics stage inside on_frame around the render call).
        let mut timer = profiler.begin_frame();
        let steps_done = demo.advance(steps)?;
        timer.mark_simulation(steps_done.min(u64::from(u32::MAX)) as u32);
        let snapshot = demo.snapshot();
        timer.mark_snapshot();
        frames += 1;
        println!(
            "TICK frame={frames} tick={} entities={}",
            snapshot.tick,
            snapshot.entities.len()
        );
        let stats = profiler.stats();
        on_frame(&snapshot, &mut timer, &stats);
        profiler.record(timer.finish());
        if profiler.perf_line_due() {
            println!("{}", profiler.perf_line());
        }
    }
    // Always emit a final PERF line at the terminal state, so short runs
    // (few frames) still produce structured output for CI.
    if profiler.frame() > 0 {
        println!("{}", profiler.perf_line());
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
fn run_battle_graphical(options: GraphicalOptions) -> Result<(), String> {
    let GraphicalOptions {
        offscreen,
        frames,
        ticks,
        clear,
        battle: _,
        seed,
        screenshot,
        perf_overlay,
        render_scale,
    } = options;
    let mut demo = BattleDemo::new(seed)?;
    if !offscreen {
        // Windowed: wall-clock 20 Hz stepping inside the frame callback; the
        // render rate stays independent of the simulation rate. Issue #88:
        // every frame is timed per stage and aggregated in the profiler.
        //
        // Fractional tick accumulator: at 60 FPS each frame is ~16.7 ms,
        // which is 0.33 ticks at 20 Hz — truncating per-frame would advance
        // zero ticks, so we accumulate the fractional remainder across frames.
        let mut last = std::time::Instant::now();
        let mut tick_accumulator = 0.0_f64;
        let mut last_logged = 0_u64;
        let mut announced_match = false;
        let mut step_error: Option<String> = None;
        let mut profiler = FrameProfiler::new(DEFAULT_PERF_WINDOW);
        let mut camera = Camera::new(demo.map.width_m, demo.map.depth_m);
        let mut selection = SelectionState::default();
        // Issue #97: per-frame camera jitter (Halton) and temporal history.
        // Any camera change is a discontinuity that resets temporal history.
        let mut jitter_seq = JitterSequence::new();
        let mut last_camera = (camera.center, camera.view_height_m, camera.yaw);
        return run_windowed_with(
            "Phalanx — autonomous battle (graphical)",
            move |device, queue, renderer, view, aspect, input| {
                let mut timer = profiler.begin_frame();

                // --- Input handling (issues #90/#91/#93) ---
                let events = input.take_events();
                let wheel = input.take_wheel();

                // Camera zoom via mouse wheel (issue #91).
                if wheel != 0.0 {
                    camera.zoom(wheel);
                }
                // Camera pan via held keys (WASD / arrows, issue #91).
                let pan_speed = 0.02; // fraction of view per frame
                let mut pan_dx = 0.0_f32;
                let mut pan_dy = 0.0_f32;
                if input.key_held(KeyCode::KeyA) || input.key_held(KeyCode::ArrowLeft) {
                    pan_dx -= pan_speed;
                }
                if input.key_held(KeyCode::KeyD) || input.key_held(KeyCode::ArrowRight) {
                    pan_dx += pan_speed;
                }
                if input.key_held(KeyCode::KeyW) || input.key_held(KeyCode::ArrowUp) {
                    pan_dy -= pan_speed;
                }
                if input.key_held(KeyCode::KeyS) || input.key_held(KeyCode::ArrowDown) {
                    pan_dy += pan_speed;
                }
                if pan_dx != 0.0 || pan_dy != 0.0 {
                    camera.pan_screen(pan_dx, pan_dy, aspect);
                }
                // Camera rotation around the view center (3D view, issue #104).
                if input.key_held(KeyCode::KeyQ) {
                    camera.rotate(-0.04);
                }
                if input.key_held(KeyCode::KeyE) {
                    camera.rotate(0.04);
                }
                // The perspective 3D camera used for picking and rendering
                // this frame (recomputed per frame, so resize stays correct).
                let camera3d = camera.camera3d();
                // Camera moved/zoomed/rotated this frame -> temporal history
                // reset (no ghosting across a camera cut, issue #97).
                let current_camera = (camera.center, camera.view_height_m, camera.yaw);
                if current_camera != last_camera {
                    renderer.note_discontinuity(Discontinuity::CameraCut);
                    last_camera = current_camera;
                }
                let (render_w, render_h) = renderer.scene_size();
                let jitter = jitter_seq.next(render_w, render_h);

                // Process input events for selection and commands.
                for event in &events {
                    match event {
                        InputEvent::MouseButton {
                            button: MouseButton::Left,
                            pressed,
                            pos,
                        } => {
                            if *pressed {
                                selection.drag_start = Some(*pos);
                            } else if let Some(start) = selection.drag_start.take() {
                                // Left release: click-select or box-select (issue #90).
                                let drag_dist = ((pos[0] - start[0]).powi(2)
                                    + (pos[1] - start[1]).powi(2))
                                .sqrt();
                                let Some(world_pos) =
                                    camera3d.screen_to_ground(aspect, pos[0], pos[1])
                                else {
                                    continue;
                                };
                                let snapshot = demo.snapshot();
                                if drag_dist < 0.02 {
                                    // Click: select nearest own unit within threshold.
                                    let threshold_world = camera.view_height_m * 0.02;
                                    let mut best: Option<(f32, u32, u64)> = None;
                                    for entity in &snapshot.entities {
                                        if entity.team != TEAM_A {
                                            continue;
                                        }
                                        let dx = entity.position[0] - world_pos[0];
                                        let dz = entity.position[1] - world_pos[1];
                                        let dist = (dx * dx + dz * dz).sqrt();
                                        if dist <= entity.radius_m.max(threshold_world) {
                                            match best {
                                                Some((bd, _, _)) if dist >= bd => {}
                                                _ => {
                                                    best = Some((
                                                        dist,
                                                        entity.entity_index,
                                                        entity.team,
                                                    ))
                                                }
                                            }
                                        }
                                    }
                                    selection.selected = match best {
                                        Some((_, idx, _)) => vec![idx],
                                        None => Vec::new(),
                                    };
                                } else {
                                    // Box select: all own units inside the drag rect.
                                    let (Some(start_world), Some(end_world)) = (
                                        camera3d.screen_to_ground(aspect, start[0], start[1]),
                                        camera3d.screen_to_ground(aspect, pos[0], pos[1]),
                                    ) else {
                                        continue;
                                    };
                                    let min_x = start_world[0].min(end_world[0]);
                                    let max_x = start_world[0].max(end_world[0]);
                                    let min_z = start_world[1].min(end_world[1]);
                                    let max_z = start_world[1].max(end_world[1]);
                                    selection.selected = snapshot
                                        .entities
                                        .iter()
                                        .filter(|e| {
                                            e.team == TEAM_A
                                                && e.position[0] >= min_x
                                                && e.position[0] <= max_x
                                                && e.position[1] >= min_z
                                                && e.position[1] <= max_z
                                        })
                                        .map(|e| e.entity_index)
                                        .collect();
                                }
                            }
                        }
                        InputEvent::MouseButton {
                            button: MouseButton::Right,
                            pressed: true,
                            pos,
                        } => {
                            // Right-click: issue Move/AttackMove/Attack to selected units (issue #93).
                            // Shift+right-click appends to the command queue (issue #94).
                            if selection.selected.is_empty() {
                                continue;
                            }
                            let append = input.key_held(KeyCode::ShiftLeft)
                                || input.key_held(KeyCode::ShiftRight);
                            let Some(world_pos) = camera3d.screen_to_ground(aspect, pos[0], pos[1])
                            else {
                                continue;
                            };
                            let snapshot = demo.snapshot();
                            // Check if an enemy unit is near the click point → Attack.
                            let click_threshold = camera.view_height_m * 0.02;
                            let enemy = snapshot.entities.iter().find(|e| {
                                if e.team == TEAM_A {
                                    return false;
                                }
                                let dx = e.position[0] - world_pos[0];
                                let dz = e.position[1] - world_pos[1];
                                (dx * dx + dz * dz).sqrt() <= e.radius_m.max(click_threshold)
                            });
                            let payload = match enemy {
                                Some(target) => {
                                    // Attack the clicked enemy: find its EntityId.
                                    let world = demo.world.borrow();
                                    let target_id = world
                                        .combat
                                        .keys()
                                        .find(|id| id.index() == target.entity_index)
                                        .copied();
                                    drop(world);
                                    match target_id {
                                        Some(tid) => CommandPayload::Attack { target: tid },
                                        None => CommandPayload::AttackMove {
                                            destination: (world_pos[0], world_pos[1]),
                                        },
                                    }
                                }
                                None => CommandPayload::AttackMove {
                                    destination: (world_pos[0], world_pos[1]),
                                },
                            };
                            // Submit the command for each selected unit.
                            let selected_ids: Vec<EntityId> = {
                                let world = demo.world.borrow();
                                selection
                                    .selected
                                    .iter()
                                    .filter_map(|&idx| {
                                        world.combat.keys().find(|id| id.index() == idx).copied()
                                    })
                                    .collect()
                            };
                            for id in selected_ids {
                                if append {
                                    // Shift+right-click: append to queue (issue #94).
                                    let cmd = match &payload {
                                        CommandPayload::Move { destination } => {
                                            phalanx_game::BattleCommand::Move {
                                                destination: *destination,
                                            }
                                        }
                                        CommandPayload::AttackMove { destination } => {
                                            phalanx_game::BattleCommand::AttackMove {
                                                destination: *destination,
                                            }
                                        }
                                        CommandPayload::Attack { target } => {
                                            phalanx_game::BattleCommand::Attack { target: *target }
                                        }
                                        CommandPayload::Diagnostics { .. } => continue,
                                    };
                                    demo.world.borrow_mut().queue_command(id, cmd);
                                } else {
                                    let _ = demo.submit_player_command(id, payload.clone());
                                }
                            }
                        }
                        InputEvent::Key {
                            key: KeyCode::Escape,
                            pressed: true,
                        } => {
                            selection.selected.clear();
                        }
                        InputEvent::Key {
                            key: KeyCode::Delete,
                            pressed: true,
                        }
                        | InputEvent::Key {
                            key: KeyCode::Backspace,
                            pressed: true,
                        } => {
                            // Stop key: cancel active commands for selected units (issue #94).
                            let selected_ids: Vec<EntityId> = {
                                let world = demo.world.borrow();
                                selection
                                    .selected
                                    .iter()
                                    .filter_map(|&idx| {
                                        world.combat.keys().find(|id| id.index() == idx).copied()
                                    })
                                    .collect()
                            };
                            for id in selected_ids {
                                demo.world.borrow_mut().cancel_commands(id);
                            }
                        }
                        _ => {}
                    }
                }

                // --- Simulation stepping (issue #95 fix: fractional accumulator) ---
                let now = std::time::Instant::now();
                let elapsed = now.saturating_duration_since(last);
                last = now;
                let tps = f64::from(TickRate::DEFAULT.ticks_per_second());
                tick_accumulator += (elapsed.as_secs_f64() * tps).min(100.0);
                let due = tick_accumulator as u64;
                tick_accumulator -= due as f64;
                let mut steps_done = 0_u64;
                if due > 0 && step_error.is_none() {
                    match demo.advance(due) {
                        Ok(done) => steps_done = done,
                        Err(error) => {
                            step_error = Some(error.clone());
                            eprintln!("error: battle step failed: {error}");
                        }
                    }
                }
                timer.mark_simulation(steps_done.min(u64::from(u32::MAX)) as u32);
                let snapshot = demo.snapshot_selected(&selection.selected);
                timer.mark_snapshot();
                if snapshot.tick != last_logged && snapshot.tick % 20 == 0 {
                    last_logged = snapshot.tick;
                    println!(
                        "TICK tick={} entities={} selected={}",
                        snapshot.tick,
                        snapshot.entities.len(),
                        selection.selected.len()
                    );
                }
                if let Some(winner) = demo.match_winner() {
                    if !announced_match {
                        announced_match = true;
                        println!("MATCH winner={winner} tick={}", snapshot.tick);
                    }
                }
                // --- Depth-tested 3D battle rendering (issue #104) ---
                let vp = camera3d.view_projection(aspect);
                let scene = build_scene_geometry(&snapshot, demo.map.width_m, demo.map.depth_m);
                let drag_world = selection.drag_start.and_then(|start| {
                    let from = camera3d.screen_to_ground(aspect, start[0], start[1])?;
                    let to = camera3d.screen_to_ground(
                        aspect,
                        input.mouse_pos[0],
                        input.mouse_pos[1],
                    )?;
                    Some((from, to))
                });
                let world_hud = build_hud_geometry(&snapshot, camera.view_height_m, drag_world);
                let screen_hud = if perf_overlay {
                    overlay_hud_vertices(&profiler.stats(), demo.map.width_m)
                } else {
                    Vec::new()
                };
                renderer.render_battle_temporal(
                    device,
                    queue,
                    view,
                    clear.to_wgpu(),
                    &BattleTemporalFrame {
                        vp: &vp,
                        scene: &scene,
                        world_hud: &world_hud,
                        screen_hud: &screen_hud,
                        jitter,
                        exposure: 1.0,
                    },
                );
                timer.mark_graphics_cpu();
                profiler.record(timer.finish());
                // Windowed: PERF lines only with the overlay flag; offscreen
                // (run_battle_to_end) always emits them for CI.
                if perf_overlay && profiler.perf_line_due() {
                    println!("{}", profiler.perf_line());
                }
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
            let mut profiler = FrameProfiler::new(DEFAULT_PERF_WINDOW);
            run_battle_to_end(&mut demo, ticks, 100, &mut profiler, |_, _, _| {})?;
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
            // Issues #104/#97: the offscreen battle renders through the
            // temporal 3D path (scene at render resolution into scene-color +
            // motion + depth, then the bilinear-fallback present pass to the
            // output target), then a final scene-only frame is verified
            // against the color, depth, and motion readbacks.
            let render_config = TemporalRenderConfig::new(render_scale);
            let mut renderer = FrameRenderer::new(
                &backend.device,
                &backend.queue,
                OffscreenTarget::FORMAT,
                BATTLE_OFFSCREEN_SIZE,
                BATTLE_OFFSCREEN_SIZE,
                render_config,
            );
            let (render_w, render_h) = renderer.scene_size();
            let map_width_m = demo.map.width_m;
            let map_depth_m = demo.map.depth_m;
            let camera = Camera::new(map_width_m, map_depth_m);
            let camera3d = camera.camera3d();
            let vp = camera3d.view_projection(1.0);
            let clear_wgpu = clear.to_wgpu();
            let mut profiler = FrameProfiler::new(DEFAULT_PERF_WINDOW);
            let mut jitter_seq = JitterSequence::new();
            let snapshot = run_battle_to_end(
                &mut demo,
                ticks,
                chunk,
                &mut profiler,
                |snapshot, timer, stats| {
                    let scene = build_scene_geometry(snapshot, map_width_m, map_depth_m);
                    let world_hud = build_hud_geometry(snapshot, camera.view_height_m, None);
                    let screen_hud = if perf_overlay {
                        overlay_hud_vertices(stats, map_width_m)
                    } else {
                        Vec::new()
                    };
                    let jitter = jitter_seq.next(render_w, render_h);
                    renderer.render_battle_temporal(
                        &backend.device,
                        &backend.queue,
                        &target.view,
                        clear_wgpu,
                        &BattleTemporalFrame {
                            vp: &vp,
                            scene: &scene,
                            world_hud: &world_hud,
                            screen_hud: &screen_hud,
                            jitter,
                            exposure: 1.0,
                        },
                    );
                    timer.mark_graphics_cpu();
                },
            )?;
            let winner = demo.match_winner().ok_or("battle ended without a winner")?;
            let loser = if winner == TEAM_A { TEAM_B } else { TEAM_A };
            // Final verification frame: 3D scene only (no HUD), so every pixel
            // must be the clear color or a lit scene color.
            let scene = build_scene_geometry(&snapshot, map_width_m, map_depth_m);
            let final_jitter = jitter_seq.next(render_w, render_h);
            renderer.render_battle_temporal(
                &backend.device,
                &backend.queue,
                &target.view,
                clear_wgpu,
                &BattleTemporalFrame {
                    vp: &vp,
                    scene: &scene,
                    world_hud: &[],
                    screen_hud: &[],
                    jitter: final_jitter,
                    exposure: 1.0,
                },
            );
            if renderer.last_jitter() != final_jitter {
                return Err(
                    "offscreen battle frame verification failed: applied jitter != reported jitter"
                        .to_owned(),
                );
            }
            let pixels = target
                .read_back(&backend.device, &backend.queue)
                .map_err(|error| error.to_string())?;
            let depths = renderer
                .read_back_depth(&backend.device, &backend.queue)
                .map_err(|error| error.to_string())?;
            let motion = renderer
                .read_back_motion(&backend.device, &backend.queue)
                .map_err(|error| error.to_string())?;
            if depths.len() != (render_w * render_h) as usize
                || motion.len() != (render_w * render_h) as usize
            {
                return Err(
                    "offscreen battle frame verification failed: depth/motion readback size mismatch"
                        .to_owned(),
                );
            }
            if !motion.iter().all(|m| m[0].is_finite() && m[1].is_finite()) {
                return Err(
                    "offscreen battle frame verification failed: non-finite motion vector"
                        .to_owned(),
                );
            }
            let mut allowed: Vec<[f32; 4]> = vec![[
                clear.0[0] as f32,
                clear.0[1] as f32,
                clear.0[2] as f32,
                clear.0[3] as f32,
            ]];
            for base in [
                GROUND_COLOR,
                OBSTACLE_COLOR,
                team_color(TEAM_A),
                team_color(TEAM_B),
            ] {
                allowed.push(lit_color(base, AMBIENT + DIFFUSE));
                allowed.push(lit_color(base, AMBIENT));
            }
            verify_pixels_in_set(&pixels, clear, &allowed).map_err(|error| {
                format!("offscreen 3D battle frame verification failed: {error}")
            })?;
            // Depth: geometry was rendered, and the depth buffer orders near
            // geometry before far geometry. The ordering is checked against
            // the CPU projection (a surviving unit's box top vs. ground farther
            // from the camera) — absolute depth deltas shrink with camera
            // distance, so a fixed threshold would be flaky across GPUs.
            let size = BATTLE_OFFSCREEN_SIZE as usize;
            let rendered = depths.iter().filter(|depth| **depth < 1.0).count();
            if rendered <= size * size / 4 {
                return Err(
                    "offscreen 3D battle frame verification failed: no rendered depth in frame"
                        .to_owned(),
                );
            }
            let first = snapshot
                .entities
                .first()
                .ok_or("offscreen 3D battle frame verification failed: no surviving units")?;
            let (ex, ez) = (first.position[0], first.position[1]);
            let away_x = camera3d.eye[0] - ex;
            let away_z = camera3d.eye[2] - ez;
            let away_len = (away_x * away_x + away_z * away_z).sqrt().max(1.0);
            let far_ground = [
                (ex - away_x / away_len * 25.0).clamp(10.0, map_width_m - 10.0),
                0.0,
                (ez - away_z / away_len * 25.0).clamp(10.0, map_depth_m - 10.0),
            ];
            verify_depth_ordering(
                &depths,
                render_w,
                render_h,
                &camera3d,
                1.0,
                [ex, UNIT_HEIGHT_M, ez],
                far_ground,
            )
            .map_err(|error| format!("offscreen 3D battle frame verification failed: {error}"))?;
            if count_color(&pixels, GROUND_COLOR) == 0 {
                return Err(
                    "offscreen 3D battle frame verification failed: no terrain pixels rendered"
                        .to_owned(),
                );
            }
            if count_color(&pixels, OBSTACLE_COLOR) == 0 {
                return Err(
                    "offscreen 3D battle frame verification failed: no obstacle pixels rendered"
                        .to_owned(),
                );
            }
            if !snapshot.entities.iter().all(|entity| entity.team == winner) {
                return Err(
                    "offscreen battle frame verification failed: final snapshot still contains loser units"
                        .to_owned(),
                );
            }
            // Team presence is checked on either lit variant of the team color
            // (box top = factor 1.0, sides = AMBIENT), robust to which face of
            // a small unit box covers the rasterized pixels.
            if count_color_family(&pixels, team_color(winner)) == 0 {
                return Err(format!(
                    "offscreen battle frame verification failed: winner team {winner} color not rendered"
                ));
            }
            if count_color_family(&pixels, team_color(loser)) != 0 {
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
                "FRAME offscreen battle3d size={}x{} render={}x{} tick={} entities={} winner={winner} pixels=verified terrain=verified obstacles=verified depth=verified motion=finite winner=verified loser=absent",
                target.width,
                target.height,
                render_w,
                render_h,
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
    let mut perf_overlay = false;
    let mut frames = 3_u64;
    let mut clear = DEFAULT_CLEAR;
    let mut ticks: Option<u64> = None;
    let mut seed: Option<u64> = None;
    let mut screenshot: Option<String> = None;
    let mut render_scale = DEFAULT_RENDER_SCALE;
    let mut index = 0;

    while index < arguments.len() {
        match arguments[index].as_str() {
            "--headless" => headless = true,
            "--graphical" => graphical = true,
            "--offscreen" => offscreen = true,
            "--battle" => battle = true,
            "--perf-overlay" => perf_overlay = true,
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
            "--render-scale" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or("--render-scale requires a number in [0.25, 1.0]")?;
                let scale: f32 = value
                    .parse()
                    .map_err(|_| format!("invalid render scale: {value}"))?;
                if !(0.25..=1.0).contains(&scale) {
                    return Err(format!(
                        "--render-scale must be in [0.25, 1.0], got {scale} (issue #97 contract)"
                    ));
                }
                render_scale = scale;
            }
            "--help" | "-h" => {
                println!(
                    "Phalanx runtime (bootstrap)\n\nUSAGE:\n    phalanx_runtime --headless [--ticks COUNT] [--seed SEED]\n    phalanx_runtime --graphical --battle [--perf-overlay] [--clear RRGGBB] [--seed SEED] [--render-scale S]\n    phalanx_runtime --graphical --offscreen --battle [--perf-overlay] [--frames N] [--ticks N] [--clear RRGGBB] [--seed SEED] [--render-scale S] [--screenshot PATH]\n\n--headless runs the deterministic CPU-only scenario through SimulationRunner (no graphics initialization). --graphical presents wgpu frames in a native window (phalanx_render); on headless machines use --graphical --offscreen, which renders frames to an offscreen texture and verifies the pixels. --battle (graphical only, issues #64/#104) runs the first autonomous battle: two teams spawn on the Crossing map with no unit commands and fight to MatchOver through the real SimulationRunner, rendered as a depth-tested 3D scene (ground, obstacle and unit boxes, perspective camera with pan/zoom/rotate, HUD overlay); windowed steps the sim at its fixed 20 Hz wall-clock rate per rendered frame, offscreen runs to the terminal state (default --ticks 2000) and verifies the final frame's color and depth readbacks (terrain, obstacles, depth discrimination, winner color present, loser color absent); --screenshot writes the final offscreen frame as a PPM image. --perf-overlay (battle mode) shows the in-game FPS/frametime overlay: per-stage bars (simulation, snapshot, graphics, total) drawn over the battlefield plus a structured PERF line every 20 frames (issue #88). --render-scale S (battle mode, issue #97) sets the render resolution as a fraction of the output resolution (0.25..=1.0, default 1.0): the 3D scene renders into scene-color + motion-vector + depth attachments at render resolution and is presented to the output via the bilinear-fallback upscale, with per-frame camera jitter and temporal-history reset on discontinuities."
                );
                return Ok(());
            }
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
        index += 1;
    }

    if graphical {
        if perf_overlay && !battle {
            return Err(
                "--perf-overlay requires --battle in graphical mode (see --help)".to_owned(),
            );
        }
        // Battle mode defaults to the slice scenario seed and the 2000-tick
        // battle budget; headless defaults to the core scenario seed and 8
        // ticks. Explicit --seed/--ticks win in both modes.
        let seed = seed.unwrap_or(if battle { BATTLE_SEED } else { SCENARIO_SEED });
        let ticks = ticks.unwrap_or(if battle { BATTLE_MAX_TICKS } else { 8 });
        return run_graphical(GraphicalOptions {
            offscreen,
            frames,
            ticks,
            clear,
            battle,
            seed,
            screenshot,
            perf_overlay,
            render_scale,
        });
    }
    if battle {
        return Err("--battle requires --graphical (see --help)".to_owned());
    }
    if perf_overlay {
        return Err("--perf-overlay requires --graphical --battle (see --help)".to_owned());
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
    fn perf_overlay_requires_graphical_battle_mode() {
        let error = run(["--headless", "--perf-overlay"]
            .into_iter()
            .map(str::to_owned))
        .expect_err("--perf-overlay without --graphical must be rejected");
        assert!(error.contains("--perf-overlay"));
        let error = run(["--graphical", "--offscreen", "--perf-overlay"]
            .into_iter()
            .map(str::to_owned))
        .expect_err("--perf-overlay without --battle must be rejected");
        assert!(error.contains("--battle"));
    }

    #[test]
    fn graphical_offscreen_battle_with_perf_overlay_runs_without_a_display() {
        // With a GPU adapter this runs the battle to MatchOver with the
        // overlay bars rendered and verified; without one the CPU-only probe
        // still runs the sim and emits structured PERF lines (sim/snapshot
        // stages; graphics_ms=0 since nothing is rendered). Both exit 0.
        assert!(
            run([
                "--graphical",
                "--offscreen",
                "--battle",
                "--perf-overlay",
                "--frames",
                "4"
            ]
            .into_iter()
            .map(str::to_owned))
            .is_ok()
        );
    }

    #[test]
    fn offscreen_battle_with_render_scale_runs_without_a_display() {
        // With a GPU adapter this renders the battle at half render
        // resolution and presents via the fallback upscale, verifying the
        // color/depth/motion readbacks; without one the CPU-only probe still
        // runs the sim and exits 0 (issue #97).
        assert!(
            run([
                "--graphical",
                "--offscreen",
                "--battle",
                "--frames",
                "4",
                "--render-scale",
                "0.5"
            ]
            .into_iter()
            .map(str::to_owned))
            .is_ok()
        );
    }

    #[test]
    fn render_scale_out_of_range_is_rejected() {
        let error = run([
            "--graphical",
            "--offscreen",
            "--battle",
            "--render-scale",
            "2.0",
        ]
        .into_iter()
        .map(str::to_owned))
        .expect_err("render scale above 1.0 must be rejected");
        assert!(error.contains("--render-scale"));
        let error = run([
            "--graphical",
            "--offscreen",
            "--battle",
            "--render-scale",
            "abc",
        ]
        .into_iter()
        .map(str::to_owned))
        .expect_err("non-numeric render scale must be rejected");
        assert!(error.contains("invalid render scale"));
    }

    #[test]
    fn battle_profiler_records_per_stage_timings_and_emits_perf_lines() {
        use super::{BATTLE_MAX_TICKS, BATTLE_SEED, BattleDemo, FrameProfiler};
        use phalanx_render::PERF_LOG_EVERY;

        let mut demo = BattleDemo::new(BATTLE_SEED).expect("battle setup");
        let mut profiler = FrameProfiler::new(super::DEFAULT_PERF_WINDOW);
        // Run enough frames to cross the PERF log interval; the on_frame
        // callback marks the (empty) graphics stage like the no-GPU probe.
        let final_snapshot = super::run_battle_to_end(
            &mut demo,
            BATTLE_MAX_TICKS,
            25,
            &mut profiler,
            |_, timer, _| timer.mark_graphics_cpu(),
        )
        .expect("terminal");
        assert!(profiler.frame() >= PERF_LOG_EVERY, "frames recorded");
        let stats = profiler.stats();
        assert!(stats.frames > 0);
        assert!(stats.avg_fps > 0.0);
        assert!(stats.worst_total >= stats.avg_total, "worst >= avg total");
        assert!(
            stats.avg_simulation + stats.avg_snapshot + stats.avg_graphics_cpu <= stats.avg_total,
            "stage averages must not exceed the total"
        );
        let line = profiler.perf_line();
        assert!(line.starts_with("PERF frame="), "line: {line}");
        assert!(
            !line.contains("graphics_ms="),
            "ambiguous name must not appear"
        );
        for field in [
            "fps=",
            "worst_ms=",
            "total_ms=",
            "sim_ms=",
            "snapshot_ms=",
            "graphics_cpu_ms=",
            "sim_steps=",
        ] {
            assert!(line.contains(field), "missing {field} in {line}");
        }
        // The battle still reached its terminal state through the profiler path.
        assert!(demo.match_winner().is_some());
        assert!(!final_snapshot.entities.is_empty());
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
        let mut profiler = super::FrameProfiler::new(super::DEFAULT_PERF_WINDOW);
        let final_snapshot = super::run_battle_to_end(
            &mut demo,
            BATTLE_MAX_TICKS,
            100,
            &mut profiler,
            |_, _, _| {},
        )
        .expect("terminal");
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
        let mut twin_profiler = super::FrameProfiler::new(super::DEFAULT_PERF_WINDOW);
        super::run_battle_to_end(
            &mut twin,
            BATTLE_MAX_TICKS,
            100,
            &mut twin_profiler,
            |_, _, _| {},
        )
        .expect("terminal");
        assert_eq!(
            demo.runner.canonical_digest(),
            twin.runner.canonical_digest()
        );
        assert_eq!(winner, twin.match_winner().expect("winner"));
    }

    #[test]
    fn battle_camera3d_picks_the_view_center_at_screen_center() {
        use super::Camera;
        // The 3D camera preserves the 2D picking contract: the screen center
        // maps to the camera's world center on the ground plane, at any aspect.
        let camera = Camera::new(200.0, 200.0);
        let camera3d = camera.camera3d();
        for aspect in [0.8, 1.0, 1.7] {
            let ground = camera3d
                .screen_to_ground(aspect, 0.5, 0.5)
                .expect("screen center hits the ground plane");
            assert!(
                (ground[0] - 100.0).abs() < 1e-3 && (ground[1] - 100.0).abs() < 1e-3,
                "aspect={aspect} ground={ground:?}"
            );
        }
        // Zooming in keeps the center pinned and shrinks the picked span.
        let mut zoomed = Camera::new(200.0, 200.0);
        zoomed.zoom(5.0);
        let camera3d = zoomed.camera3d();
        let center = camera3d.screen_to_ground(1.0, 0.5, 0.5).expect("center");
        let edge = camera3d.screen_to_ground(1.0, 1.0, 0.5).expect("edge");
        let full = camera
            .camera3d()
            .screen_to_ground(1.0, 1.0, 0.5)
            .expect("edge");
        let full_center = camera
            .camera3d()
            .screen_to_ground(1.0, 0.5, 0.5)
            .expect("c");
        assert!((center[0] - 100.0).abs() < 1e-3);
        assert!(
            (edge[0] - center[0]).abs() < (full[0] - full_center[0]).abs(),
            "zoomed-in view covers less world"
        );
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
