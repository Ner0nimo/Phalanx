//! The windowed graphical frame loop (issue #61): a native `winit` window
//! presenting a continuously rendered clear-color frame through a wgpu
//! surface.
//!
//! Behavior contract:
//! - adapter name/backend/driver are logged at startup (plus OS/arch/build
//!   profile via [`crate::AdapterReport`]);
//! - the surface is configured from its advertised capabilities
//!   ([`choose_surface_config`]) and reconfigured on resize, so presentation
//!   survives window resizing;
//! - `Lost`/`Outdated` surface errors reconfigure and retry, `Timeout` just
//!   re-requests a redraw, anything else exits with a structured error;
//! - closing the window exits cleanly;
//! - on a machine without a display, window creation fails with a structured
//!   [`RenderError`] that points at the offscreen mode — the `--headless`
//!   runtime path never goes through here.

use std::fmt;
use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::Window;

use crate::backend::{AdapterReport, init_from_adapter, instance_descriptor};
use crate::geometry::{DebugPipeline, OrthoProjection, demo_shapes};
use crate::present::{ClearColor, choose_surface_config};

/// Structured errors of the windowed presentation path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenderError {
    /// The OS event loop could not be created (e.g. no display on a headless
    /// machine) or exited with an error.
    EventLoop(String),
    /// Window creation failed.
    Window(String),
    /// No GPU adapter compatible with the window surface.
    NoAdapter(String),
    /// Surface creation or configuration failed.
    Surface(String),
    /// Device creation failed.
    Device(String),
}

impl fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EventLoop(error) => write!(
                formatter,
                "event loop error: {error} (windowed mode needs a display; use --graphical --offscreen on headless machines)"
            ),
            Self::Window(error) => write!(formatter, "window creation failed: {error}"),
            Self::NoAdapter(error) => {
                write!(formatter, "no GPU adapter for the window surface: {error}")
            }
            Self::Surface(error) => write!(formatter, "surface error: {error}"),
            Self::Device(error) => write!(formatter, "device creation failed: {error}"),
        }
    }
}

impl std::error::Error for RenderError {}

impl From<crate::BackendError> for RenderError {
    fn from(error: crate::BackendError) -> Self {
        match error {
            crate::BackendError::NoAdapter(error) => Self::NoAdapter(error),
            crate::BackendError::Device(error) => Self::Device(error),
        }
    }
}

/// Runs the windowed presentation loop until the window is closed: a native
/// window showing a continuously presented clear-color frame.
pub fn run_windowed(clear: ClearColor, title: &str) -> Result<(), RenderError> {
    let event_loop = EventLoop::new().map_err(|error| RenderError::EventLoop(error.to_string()))?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = ClearApp {
        clear,
        title: title.to_owned(),
        state: None,
        fatal: None,
    };
    event_loop
        .run_app(&mut app)
        .map_err(|error| RenderError::EventLoop(error.to_string()))?;
    match app.fatal {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

struct WindowState {
    window: Arc<Window>,
    backend: crate::GpuBackend,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    pipeline: DebugPipeline,
}

struct ClearApp {
    clear: ClearColor,
    title: String,
    state: Option<WindowState>,
    fatal: Option<RenderError>,
}

impl ClearApp {
    fn fail(&mut self, event_loop: &ActiveEventLoop, error: RenderError) {
        eprintln!("error: {error}");
        self.fatal = Some(error);
        event_loop.exit();
    }
}

impl ApplicationHandler for ClearApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let window = match event_loop.create_window(
            Window::default_attributes()
                .with_title(&self.title)
                .with_inner_size(winit::dpi::PhysicalSize::new(800, 600)),
        ) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                self.fail(event_loop, RenderError::Window(error.to_string()));
                return;
            }
        };
        let instance = wgpu::Instance::new(instance_descriptor());
        let surface = match instance.create_surface(window.clone()) {
            Ok(surface) => surface,
            Err(error) => {
                self.fail(event_loop, RenderError::Surface(error.to_string()));
                return;
            }
        };
        let adapter =
            match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                apply_limit_buckets: false,
            })) {
                Ok(adapter) => adapter,
                Err(error) => {
                    self.fail(event_loop, RenderError::NoAdapter(error.to_string()));
                    return;
                }
            };
        let (backend, report): (crate::GpuBackend, AdapterReport) =
            match init_from_adapter(instance, adapter) {
                Ok(pair) => pair,
                Err(error) => {
                    self.fail(event_loop, RenderError::from(error));
                    return;
                }
            };
        let capabilities = surface.get_capabilities(&backend.adapter);
        let size = window.inner_size();
        let config = choose_surface_config(&capabilities, size.width.max(1), size.height.max(1));
        surface.configure(&backend.device, &config);
        let pipeline = DebugPipeline::new(&backend.device, config.format);
        // Startup log: adapter name, backend, device info (issue #61).
        println!("{report}");
        println!(
            "SURFACE configured format={:?} present_mode={:?} size={}x{} alpha={:?}",
            config.format, config.present_mode, config.width, config.height, config.alpha_mode
        );
        self.state = Some(WindowState {
            window,
            backend,
            surface,
            config,
            pipeline,
        });
        self.state
            .as_ref()
            .expect("just set")
            .window
            .request_redraw();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        let Some(state) = &mut self.state else {
            return;
        };
        match event {
            WindowEvent::Resized(size) => {
                // Resize must not break presentation: reconfigure the surface
                // with the new size and keep presenting.
                if size.width > 0 && size.height > 0 {
                    state.config.width = size.width;
                    state.config.height = size.height;
                    state
                        .surface
                        .configure(&state.backend.device, &state.config);
                    state.window.request_redraw();
                }
            }
            WindowEvent::CloseRequested => {
                // Clean exit when the window is closed.
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
                match state.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(frame)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                        let view = frame
                            .texture
                            .create_view(&wgpu::TextureViewDescriptor::default());
                        // Debug geometry over the clear color (issue #62). The
                        // projection is recomputed per frame from the current
                        // surface size, so resizing keeps geometry in place.
                        let aspect = state.config.width as f32 / state.config.height.max(1) as f32;
                        let projection = OrthoProjection::CROSSING_VIEW.for_aspect(aspect);
                        state.pipeline.render(
                            &state.backend.device,
                            &state.backend.queue,
                            &view,
                            self.clear.to_wgpu(),
                            &projection,
                            &demo_shapes(),
                        );
                        state.backend.queue.present(frame);
                        // Continuously present frames.
                        state.window.request_redraw();
                    }
                    // Outdated: the surface changed (e.g. after resize) —
                    // reconfigure and retry. Lost: recreate the surface from
                    // the window, reconfigure, and retry.
                    wgpu::CurrentSurfaceTexture::Outdated => {
                        state
                            .surface
                            .configure(&state.backend.device, &state.config);
                        state.window.request_redraw();
                    }
                    wgpu::CurrentSurfaceTexture::Lost => {
                        match state.backend.instance.create_surface(state.window.clone()) {
                            Ok(surface) => {
                                state.surface = surface;
                                state
                                    .surface
                                    .configure(&state.backend.device, &state.config);
                            }
                            Err(error) => {
                                self.fail(event_loop, RenderError::Surface(error.to_string()));
                                return;
                            }
                        }
                        state.window.request_redraw();
                    }
                    // Timeout / occluded: skip this frame and ask again.
                    wgpu::CurrentSurfaceTexture::Timeout
                    | wgpu::CurrentSurfaceTexture::Occluded => {
                        state.window.request_redraw();
                    }
                    wgpu::CurrentSurfaceTexture::Validation => {
                        self.fail(
                            event_loop,
                            RenderError::Surface("validation error acquiring frame".to_owned()),
                        );
                    }
                }
            }
            _ => {}
        }
    }
}
