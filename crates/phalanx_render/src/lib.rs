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
pub mod geometry;
pub mod present;
pub mod snapshot;
pub mod window;

pub use backend::{AdapterReport, BackendError, GpuBackend, init_headless};
pub use geometry::{DebugPipeline, DebugShape, OrthoProjection, Vertex, count_color, demo_shapes};
pub use present::{
    ClearColor, DEFAULT_CLEAR, OffscreenTarget, choose_surface_config, render_clear_to_view,
    verify_clear_pixels, verify_clear_pixels_background,
};
pub use snapshot::{
    EntityCategory, OBSTACLE_COLOR, ObstacleShape, RenderEntity, RenderSnapshot, RenderSpace,
    team_color,
};
pub use window::{FrameCallback, RenderError, run_windowed, run_windowed_with};
