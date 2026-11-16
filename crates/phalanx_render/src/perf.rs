//! In-game performance collector and overlay instrumentation (issue #88).
//!
//! Measurement boundaries (normative, see issue #88): `simulation` is the
//! wall-clock time advancing the `SimulationRunner` during the frame;
//! `snapshot` is `extract_snapshot`; `graphics_cpu` is the CPU-side wgpu
//! command encoding + submission (frame presentation is asynchronous — in
//! windowed mode `queue.present` is issued by the window loop right after the
//! frame callback, inside `total` but not itemized; CPU submission time says
//! nothing about GPU execution time — GPU timestamps are deferred); `total`
//! is the entire application frame and is **measured independently**, never
//! computed by summing the stages. `simulation_steps` records how many sim
//! steps ran in the frame: at the fixed 20 Hz sim rate a frame may contain
//! zero steps, and `simulation = 0` then means "no step ran", not "free".
//!
//! The collector is a small independent performance collector: raw per-frame
//! [`FrameTimings`] are aggregated over a fixed-size sliding window
//! ([`FrameProfiler`] / [`PerfStats`]) and presented through debug-geometry
//! overlay bars ([`overlay_shapes`]) plus structured `PERF` stdout lines. It
//! knows nothing about simulation internals beyond receiving timings.
//!
//! Overhead contract (issue #88): monotonic `Instant` reads only; the
//! sliding window is fixed-size (no heap allocation in the per-frame path
//! after warmup); no string formatting except the periodic `PERF` line
//! (every [`PERF_LOG_EVERY`] frames); the overlay's own rendering cost is
//! inside `total`. Measurement-only: wall clock only, never feeds back into
//! authoritative simulation state — the `--headless` digest is unchanged with
//! the overlay enabled. Numeric FPS/frametime values are stdout-only (there
//! is no text renderer yet); the in-game overlay is bar-based.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::geometry::{DebugShape, OrthoProjection};

/// Default sliding-window size (frames) for the FPS/frametime statistics.
pub const DEFAULT_PERF_WINDOW: usize = 120;

/// How often a structured `PERF` line is emitted (every N frames).
pub const PERF_LOG_EVERY: u64 = 20;

/// Height of the overlay strip in world meters, drawn above the map
/// (world z in `[-OVERLAY_STRIP_M, 0)`).
pub const OVERLAY_STRIP_M: f32 = 14.0;

/// Bar height scale: meters of bar height per millisecond of stage time.
const MS_TO_M: f32 = 0.5;

/// One frame's raw measurements.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameTimings {
    /// The whole frame (independently measured, never a sum of the stages).
    pub total: Duration,
    /// Simulation advancement (`SimulationRunner`) during this frame.
    pub simulation: Duration,
    /// Render-snapshot extraction during this frame.
    pub snapshot: Duration,
    /// CPU-side wgpu encode + submit during this frame.
    pub graphics_cpu: Duration,
    /// How many simulation steps ran during this frame (0 = idle frame at
    /// the fixed sim rate, not "simulation is free").
    pub simulation_steps: u32,
}

/// A frame timer: marks the stages in fixed order (simulation → snapshot →
/// graphics_cpu); `finish` yields the [`FrameTimings`]. `total` is measured
/// from frame start, so the stage marks always sum to at most `total`.
pub struct FrameTimer {
    start: Instant,
    last: Instant,
    simulation: Duration,
    snapshot: Duration,
    graphics_cpu: Duration,
    simulation_steps: u32,
}

impl FrameTimer {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            start: now,
            last: now,
            simulation: Duration::ZERO,
            snapshot: Duration::ZERO,
            graphics_cpu: Duration::ZERO,
            simulation_steps: 0,
        }
    }

    /// Marks the end of the simulation stage (call right after advancing the
    /// simulation for this frame), recording how many steps actually ran.
    pub fn mark_simulation(&mut self, steps: u32) {
        let now = Instant::now();
        self.simulation = now - self.last;
        self.last = now;
        self.simulation_steps = steps;
    }

    /// Marks the end of the snapshot-extraction stage.
    pub fn mark_snapshot(&mut self) {
        let now = Instant::now();
        self.snapshot = now - self.last;
        self.last = now;
    }

    /// Marks the end of the CPU-side graphics stage (call right after the
    /// wgpu encode/submit for this frame).
    pub fn mark_graphics_cpu(&mut self) {
        let now = Instant::now();
        self.graphics_cpu = now - self.last;
        self.last = now;
    }

    /// Finishes the frame.
    #[must_use]
    pub fn finish(self) -> FrameTimings {
        FrameTimings {
            total: self.start.elapsed(),
            simulation: self.simulation,
            snapshot: self.snapshot,
            graphics_cpu: self.graphics_cpu,
            simulation_steps: self.simulation_steps,
        }
    }
}

/// Aggregated statistics over the sliding window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PerfStats {
    /// Frames currently in the window.
    pub frames: usize,
    /// Average FPS over the window (`frames / total window time`).
    pub avg_fps: f64,
    /// Worst (maximum) total frametime in the window.
    pub worst_total: Duration,
    /// Average total frametime (independently measured per frame).
    pub avg_total: Duration,
    /// Average simulation-stage time.
    pub avg_simulation: Duration,
    /// Average snapshot-stage time.
    pub avg_snapshot: Duration,
    /// Average CPU-side graphics-stage time.
    pub avg_graphics_cpu: Duration,
    /// Average simulation steps per frame over the window.
    pub avg_simulation_steps: f64,
}

impl PerfStats {
    fn empty() -> Self {
        Self {
            frames: 0,
            avg_fps: 0.0,
            worst_total: Duration::ZERO,
            avg_total: Duration::ZERO,
            avg_simulation: Duration::ZERO,
            avg_snapshot: Duration::ZERO,
            avg_graphics_cpu: Duration::ZERO,
            avg_simulation_steps: 0.0,
        }
    }
}

/// The sliding-window FPS/frametime profiler. Presentation-side only; wall
/// clock only; fixed-size window (no per-frame allocation after warmup);
/// never touches simulation state.
pub struct FrameProfiler {
    window: VecDeque<FrameTimings>,
    capacity: usize,
    frame: u64,
    last: Option<FrameTimings>,
}

impl FrameProfiler {
    /// Creates a profiler with the given sliding-window capacity (frames).
    #[must_use]
    pub fn new(window_capacity: usize) -> Self {
        Self {
            window: VecDeque::with_capacity(window_capacity.max(1)),
            capacity: window_capacity.max(1),
            frame: 0,
            last: None,
        }
    }

    /// Starts timing a new frame.
    #[must_use]
    pub fn begin_frame(&self) -> FrameTimer {
        FrameTimer::new()
    }

    /// Records a finished frame, evicting the oldest when the window is full.
    /// No allocation once the window has reached capacity.
    pub fn record(&mut self, timings: FrameTimings) {
        if self.window.len() == self.capacity {
            self.window.pop_front();
        }
        self.window.push_back(timings);
        self.last = Some(timings);
        self.frame += 1;
    }

    /// The total number of frames recorded so far.
    #[must_use]
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Whether a structured `PERF` line is due (every [`PERF_LOG_EVERY`]
    /// frames).
    #[must_use]
    pub fn perf_line_due(&self) -> bool {
        self.frame > 0 && self.frame % PERF_LOG_EVERY == 0
    }

    /// Aggregated statistics over the current window.
    #[must_use]
    pub fn stats(&self) -> PerfStats {
        if self.window.is_empty() {
            return PerfStats::empty();
        }
        let frames = self.window.len();
        let sum_total: Duration = self.window.iter().map(|t| t.total).sum();
        let avg = |stage: fn(&FrameTimings) -> Duration| -> Duration {
            let sum: Duration = self.window.iter().map(stage).sum();
            sum / frames as u32
        };
        let sum_steps: u64 = self
            .window
            .iter()
            .map(|t| u64::from(t.simulation_steps))
            .sum();
        PerfStats {
            frames,
            avg_fps: frames as f64 / sum_total.as_secs_f64(),
            worst_total: self
                .window
                .iter()
                .map(|t| t.total)
                .max()
                .unwrap_or(Duration::ZERO),
            avg_total: sum_total / frames as u32,
            avg_simulation: avg(|t| t.simulation),
            avg_snapshot: avg(|t| t.snapshot),
            avg_graphics_cpu: avg(|t| t.graphics_cpu),
            avg_simulation_steps: sum_steps as f64 / frames as f64,
        }
    }

    /// The structured log line, e.g.
    /// `PERF frame=120 fps=58.3 worst_ms=21.004 total_ms=17.123 sim_ms=0.412 snapshot_ms=0.031 graphics_cpu_ms=16.680 sim_steps=1`
    /// (`sim_steps` is the step count of the most recently recorded frame).
    #[must_use]
    pub fn perf_line(&self) -> String {
        let stats = self.stats();
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let sim_steps = self.last.map(|t| t.simulation_steps).unwrap_or(0);
        format!(
            "PERF frame={} fps={:.1} worst_ms={:.3} total_ms={:.3} sim_ms={:.3} snapshot_ms={:.3} graphics_cpu_ms={:.3} sim_steps={}",
            self.frame,
            stats.avg_fps,
            ms(stats.worst_total),
            ms(stats.avg_total),
            ms(stats.avg_simulation),
            ms(stats.avg_snapshot),
            ms(stats.avg_graphics_cpu),
            sim_steps,
        )
    }
}

/// Overlay stage colors (distinct from team colors and obstacle gray).
pub const SIMULATION_COLOR: [f32; 4] = [0.30, 0.80, 0.30, 1.0];
/// Snapshot-stage bar color.
pub const SNAPSHOT_COLOR: [f32; 4] = [0.90, 0.80, 0.20, 1.0];
/// CPU-graphics-stage bar color.
pub const GRAPHICS_CPU_COLOR: [f32; 4] = [0.30, 0.60, 0.95, 1.0];
/// Total-frametime bar color.
pub const TOTAL_COLOR: [f32; 4] = [0.95, 0.40, 0.30, 1.0];
/// Overlay strip background color.
pub const OVERLAY_BG_COLOR: [f32; 4] = [0.05, 0.05, 0.08, 1.0];

/// The projection used when the overlay is enabled: the map plus the overlay
/// strip above it (world z in `[-OVERLAY_STRIP_M, depth)`).
#[must_use]
pub fn overlay_projection(map_width_m: f32, map_depth_m: f32, aspect: f32) -> OrthoProjection {
    OrthoProjection::new(0.0, map_width_m, -OVERLAY_STRIP_M, map_depth_m).for_aspect(aspect)
}

/// The overlay shapes for the current stats: a background strip plus one bar
/// per stage (simulation, snapshot, graphics_cpu, total), bar height
/// proportional to the stage's average frametime (clamped to the strip).
/// Numeric values (FPS etc.) are printed as `PERF` lines — there is no text
/// renderer yet (issue #88).
#[must_use]
pub fn overlay_shapes(stats: &PerfStats, map_width_m: f32) -> Vec<DebugShape> {
    let mut shapes = Vec::with_capacity(5);
    shapes.push(DebugShape::Rect {
        center: [map_width_m / 2.0, -OVERLAY_STRIP_M / 2.0],
        half_size: [map_width_m / 2.0, OVERLAY_STRIP_M / 2.0],
        color: OVERLAY_BG_COLOR,
    });
    let stages = [
        (stats.avg_simulation, SIMULATION_COLOR),
        (stats.avg_snapshot, SNAPSHOT_COLOR),
        (stats.avg_graphics_cpu, GRAPHICS_CPU_COLOR),
        (stats.avg_total, TOTAL_COLOR),
    ];
    let margin = 10.0_f32;
    let slot = (map_width_m - 2.0 * margin) / stages.len() as f32;
    let half_w = (slot * 0.8) / 2.0;
    let max_h = OVERLAY_STRIP_M - 1.0;
    for (index, (avg, color)) in stages.iter().enumerate() {
        let height = ((avg.as_secs_f64() * 1000.0) as f32 * MS_TO_M).clamp(0.2, max_h);
        let center_x = margin + slot * (index as f32 + 0.5);
        shapes.push(DebugShape::Rect {
            center: [center_x, -height / 2.0],
            half_size: [half_w, height / 2.0],
            color: *color,
        });
    }
    shapes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timings(total_ms: u64, sim_ms: u64, snapshot_ms: u64, graphics_cpu_ms: u64) -> FrameTimings {
        FrameTimings {
            total: Duration::from_millis(total_ms),
            simulation: Duration::from_millis(sim_ms),
            snapshot: Duration::from_millis(snapshot_ms),
            graphics_cpu: Duration::from_millis(graphics_cpu_ms),
            simulation_steps: 1,
        }
    }

    #[test]
    fn profiler_stats_compute_window_aggregates() {
        let mut profiler = FrameProfiler::new(120);
        profiler.record(timings(10, 2, 1, 7));
        profiler.record(timings(20, 4, 2, 14));
        profiler.record(timings(30, 6, 3, 21));
        let stats = profiler.stats();
        assert_eq!(stats.frames, 3);
        assert_eq!(stats.avg_total, Duration::from_millis(20));
        assert_eq!(stats.worst_total, Duration::from_millis(30));
        assert_eq!(stats.avg_simulation, Duration::from_millis(4));
        assert_eq!(stats.avg_snapshot, Duration::from_millis(2));
        assert_eq!(stats.avg_graphics_cpu, Duration::from_millis(14));
        assert!((stats.avg_simulation_steps - 1.0).abs() < 1e-9);
        // 3 frames over 60 ms total = 50 fps.
        assert!((stats.avg_fps - 50.0).abs() < 1e-9, "fps={}", stats.avg_fps);
        assert_eq!(profiler.frame(), 3);
    }

    #[test]
    fn profiler_window_is_bounded_and_allocation_free_at_capacity() {
        let mut profiler = FrameProfiler::new(4);
        for i in 0..10 {
            profiler.record(timings(10 + i, 1, 1, 1));
        }
        assert_eq!(profiler.frame(), 10, "total frames recorded");
        let stats = profiler.stats();
        assert_eq!(stats.frames, 4, "window capped at capacity");
        // Only the last 4 frames (totals 16..=19 ms) remain: avg 17.5 ms.
        assert_eq!(stats.avg_total, Duration::from_micros(17_500));
        assert_eq!(stats.worst_total, Duration::from_millis(19));
    }

    #[test]
    fn profiler_empty_stats_are_zeroed() {
        let profiler = FrameProfiler::new(8);
        let stats = profiler.stats();
        assert_eq!(stats.frames, 0);
        assert_eq!(stats.avg_fps, 0.0);
        assert!(!profiler.perf_line_due());
        assert!(profiler.perf_line().starts_with("PERF frame=0 "));
        assert!(profiler.perf_line().contains("sim_steps=0"));
    }

    #[test]
    fn perf_line_has_the_structured_stage_fields() {
        let mut profiler = FrameProfiler::new(120);
        for _ in 0..PERF_LOG_EVERY {
            profiler.record(timings(16, 1, 1, 14));
        }
        assert!(profiler.perf_line_due());
        let line = profiler.perf_line();
        assert!(line.starts_with("PERF frame=20 fps=62.5"), "line: {line}");
        for field in [
            "worst_ms=",
            "total_ms=",
            "sim_ms=",
            "snapshot_ms=",
            "graphics_cpu_ms=",
            "sim_steps=",
        ] {
            assert!(line.contains(field), "missing {field} in {line}");
        }
        assert!(
            !line.contains("graphics_ms="),
            "must not use the ambiguous name"
        );
    }

    #[test]
    fn perf_line_reports_the_last_frames_simulation_steps() {
        let mut profiler = FrameProfiler::new(120);
        let mut t = timings(16, 1, 1, 14);
        t.simulation_steps = 3;
        profiler.record(t);
        let mut idle = timings(8, 0, 0, 8);
        idle.simulation_steps = 0; // idle frame at the 20 Hz sim rate
        profiler.record(idle);
        let line = profiler.perf_line();
        assert!(line.ends_with("sim_steps=0"), "last frame was idle: {line}");
    }

    #[test]
    fn frame_timer_stages_sum_to_at_most_total() {
        let mut timer = FrameProfiler::new(4).begin_frame();
        timer.mark_simulation(2);
        timer.mark_snapshot();
        timer.mark_graphics_cpu();
        let timings = timer.finish();
        assert_eq!(timings.simulation_steps, 2);
        assert!(timings.simulation <= timings.total);
        assert!(timings.snapshot <= timings.total);
        assert!(timings.graphics_cpu <= timings.total);
        assert!(timings.simulation + timings.snapshot + timings.graphics_cpu <= timings.total);
    }

    #[test]
    fn overlay_projection_places_the_strip_above_the_map() {
        let projection = overlay_projection(200.0, 200.0, 1.0);
        // Strip top edge (z = -OVERLAY_STRIP_M) is the top of the view.
        let top = projection.world_to_clip(100.0, -OVERLAY_STRIP_M);
        assert!((top[1] - 1.0).abs() < 1e-6, "strip top: {top:?}");
        // Map bottom (z = 200) is the bottom of the view.
        let bottom = projection.world_to_clip(100.0, 200.0);
        assert!((bottom[1] + 1.0).abs() < 1e-6, "map bottom: {bottom:?}");
        // Map center stays centered horizontally.
        let center = projection.world_to_clip(100.0, 93.0);
        assert!(center[0].abs() < 1e-6, "centered: {center:?}");
    }

    #[test]
    fn overlay_shapes_draw_a_strip_and_one_bar_per_stage() {
        let stats = PerfStats {
            frames: 10,
            avg_fps: 60.0,
            worst_total: Duration::from_millis(30),
            avg_total: Duration::from_millis(16),
            avg_simulation: Duration::from_millis(2),
            avg_snapshot: Duration::from_millis(1),
            avg_graphics_cpu: Duration::from_millis(13),
            avg_simulation_steps: 1.0,
        };
        let shapes = overlay_shapes(&stats, 200.0);
        assert_eq!(shapes.len(), 5, "background strip + 4 stage bars");
        match shapes[0] {
            DebugShape::Rect {
                color, half_size, ..
            } => {
                assert_eq!(color, OVERLAY_BG_COLOR);
                assert_eq!(half_size, [100.0, OVERLAY_STRIP_M / 2.0]);
            }
            _ => panic!("strip must be a rect"),
        }
        let bar_height = |shape: &DebugShape| match shape {
            DebugShape::Rect { half_size, .. } => half_size[1] * 2.0,
            _ => panic!("bar must be a rect"),
        };
        // Bar heights are proportional to the stage averages (0.5 m per ms).
        assert!((bar_height(&shapes[1]) - 1.0).abs() < 1e-6, "sim 2ms -> 1m");
        assert!(
            (bar_height(&shapes[3]) - 6.5).abs() < 1e-6,
            "graphics_cpu 13ms -> 6.5m"
        );
        assert!(
            (bar_height(&shapes[4]) - 8.0).abs() < 1e-6,
            "total 16ms -> 8m"
        );
        // Colors are the stage colors in order.
        assert_eq!(shapes[1].color(), SIMULATION_COLOR);
        assert_eq!(shapes[2].color(), SNAPSHOT_COLOR);
        assert_eq!(shapes[3].color(), GRAPHICS_CPU_COLOR);
        assert_eq!(shapes[4].color(), TOTAL_COLOR);
    }

    #[test]
    fn overlay_bar_heights_are_clamped_to_the_strip() {
        let stats = PerfStats {
            frames: 1,
            avg_fps: 1.0,
            worst_total: Duration::from_secs(10),
            avg_total: Duration::from_secs(10),
            avg_simulation: Duration::from_secs(10),
            avg_snapshot: Duration::ZERO,
            avg_graphics_cpu: Duration::ZERO,
            avg_simulation_steps: 20.0,
        };
        let shapes = overlay_shapes(&stats, 200.0);
        for shape in &shapes[1..] {
            if let DebugShape::Rect { half_size, .. } = shape {
                assert!(half_size[1] * 2.0 <= OVERLAY_STRIP_M - 1.0 + 1e-6);
            }
        }
    }
}
