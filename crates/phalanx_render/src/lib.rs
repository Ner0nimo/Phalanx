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
pub mod present;
pub mod window;

pub use backend::{AdapterReport, BackendError, GpuBackend, init_headless};
pub use present::{
    ClearColor, DEFAULT_CLEAR, OffscreenTarget, choose_surface_config, render_clear_to_view,
    verify_clear_pixels,
};
pub use window::{RenderError, run_windowed};
