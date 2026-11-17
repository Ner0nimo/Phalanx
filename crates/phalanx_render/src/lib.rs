//! Phalanx rendering presentation path (issue #61).
//!
//! This crate owns the *presentation* side only: GPU backend initialization,
//! surface configuration and frame presentation (windowed via `winit`, or
//! offscreen to a texture for headless verification), and clear-color frame
//! rendering. It never owns authoritative simulation state — the simulation
//! stays CPU-only in `phalanx_core` and the `--headless` runtime path never
//! touches this crate's GPU initialization.
//!
//! Everything is safe Rust (the workspace forbids `unsafe`); `wgpu` and
//! `winit` are safe APIs. When no GPU adapter exists (headless CI runners),
//! initialization returns a structured [`BackendError::NoAdapter`] instead of
//! panicking, so headless environments get a verifiable probe result.

#![forbid(unsafe_code)]

pub mod backend;
pub mod frame;
pub mod geometry;
pub mod math;
pub mod perf;
pub mod present;
pub mod scene3d;
pub mod snapshot;
pub mod window;

pub use backend::{AdapterReport, BackendError, GpuBackend, init_headless};
pub use frame::{BattleFrame, FrameRenderer, create_depth_target, read_back_depth};
pub use geometry::{DebugPipeline, DebugShape, OrthoProjection, Vertex, count_color, demo_shapes};
pub use math::Mat4;
pub use perf::{
    DEFAULT_PERF_WINDOW, FrameProfiler, FrameTimer, FrameTimings, OVERLAY_STRIP_M, PERF_LOG_EVERY,
    PerfStats, overlay_projection, overlay_shapes,
};
pub use present::{
    ClearColor, DEFAULT_CLEAR, OffscreenTarget, PresentError, choose_surface_config,
    render_clear_to_view, verify_clear_pixels, verify_clear_pixels_background,
    verify_pixels_in_set, write_ppm,
};
pub use scene3d::{
    AMBIENT, CAMERA_FOV_Y_RAD, CAMERA_PITCH_RAD, Camera3D, DEPTH_FORMAT, DIFFUSE, GROUND_COLOR,
    HudVertex, OBSTACLE_HEIGHT_M, ScenePipelines, SceneVertex, UNIT_HEIGHT_M, build_hud_geometry,
    build_scene_geometry, count_color_family, lit_color, overlay_hud_vertices, sample_depth_window,
    verify_depth_ordering, world_to_screen,
};
pub use snapshot::{
    EntityCategory, OBSTACLE_COLOR, ObstacleShape, RenderEntity, RenderSnapshot, RenderSpace,
    team_color,
};
pub use window::{
    FrameCallback, InputEvent, InputState, RenderError, run_windowed, run_windowed_with,
};

/// Re-export winit input types so downstream crates (runtime) can match on
/// them without a direct winit dependency.
pub use winit::event::MouseButton;
pub use winit::keyboard::KeyCode;
