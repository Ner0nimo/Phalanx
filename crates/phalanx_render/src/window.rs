//! The windowed graphical frame loop (issue #61): a native `winit` window
//! presenting continuously rendered frames through a wgpu surface. The frame
//! contents come from a caller-provided callback ([`run_windowed_with`]): the
//! demo debug scene via [`run_windowed`], or the actual simulation snapshot in
//! battle mode (issue #63).
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

/// A frame callback: renders one frame into `view` (the current surface
/// texture view), given the device, queue, debug pipeline, the view
/// aspect (width/height), and the accumulated input state since the last
/// frame (issues #90/#91/#93).
pub type FrameCallback = dyn FnMut(
    &wgpu::Device,
    &wgpu::Queue,
    &mut DebugPipeline,
    &wgpu::TextureView,
    f32,
    &mut InputState,
);

/// Accumulated input state between frames (issues #90/#91/#93). All
/// positions are normalized screen coordinates (0..=1, origin top-left).
/// The frame callback drains events each frame via [`InputState::take_events`].
#[derive(Default)]
pub struct InputState {
    /// Current mouse position in normalized screen coordinates.
    pub mouse_pos: [f32; 2],
    /// Mouse wheel delta accumulated since the last frame (positive = up/away).
    pub wheel_delta: f32,
    /// Events accumulated since the last frame, drained by the callback.
    events: Vec<InputEvent>,
    /// Keys currently held down (for continuous camera pan).
    pub keys_held: std::collections::HashSet<winit::keyboard::KeyCode>,
}

/// One input event (mouse button, key press).
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    /// A mouse button was pressed or released at the given normalized position.
    MouseButton {
        button: winit::event::MouseButton,
        pressed: bool,
        pos: [f32; 2],
    },
    /// A keyboard key was pressed or released.
    Key {
        key: winit::keyboard::KeyCode,
        pressed: bool,
    },
}

impl InputState {
    /// Records a mouse motion event, updating the current position.
    pub fn mouse_moved(&mut self, sx: f32, sy: f32) {
        self.mouse_pos = [sx, sy];
    }

    /// Records a mouse button event.
    pub fn mouse_button(&mut self, button: winit::event::MouseButton, pressed: bool) {
        self.events.push(InputEvent::MouseButton {
            button,
            pressed,
            pos: self.mouse_pos,
        });
    }

    /// Records a mouse wheel event.
    pub fn mouse_wheel(&mut self, delta: f32) {
        self.wheel_delta += delta;
    }

    /// Records a key event, updating the held-keys set.
    pub fn key(&mut self, key: winit::keyboard::KeyCode, pressed: bool) {
        if pressed {
            self.keys_held.insert(key);
        } else {
            self.keys_held.remove(&key);
        }
        self.events.push(InputEvent::Key { key, pressed });
    }

    /// Drains all accumulated events (called once per frame by the callback).
    pub fn take_events(&mut self) -> Vec<InputEvent> {
        std::mem::take(&mut self.events)
    }

    /// Returns and resets the wheel delta (called once per frame).
    pub fn take_wheel(&mut self) -> f32 {
        std::mem::take(&mut self.wheel_delta)
    }

    /// Whether a key is currently held.
    pub fn key_held(&self, key: winit::keyboard::KeyCode) -> bool {
        self.keys_held.contains(&key)
    }
}

/// Runs the windowed presentation loop until the window is closed, drawing
/// the demo debug scene (issue #62) over a clear-color background.
pub fn run_windowed(clear: ClearColor, title: &str) -> Result<(), RenderError> {
    run_windowed_with(
        title,
        move |device, queue, pipeline, view, aspect, _input| {
            let projection = OrthoProjection::CROSSING_VIEW.for_aspect(aspect);
            pipeline.render(
                device,
                queue,
                view,
                clear.to_wgpu(),
                &projection,
                &demo_shapes(),
            );
        },
    )
}

/// Runs the windowed presentation loop until the window is closed, calling
/// `frame` for every redraw with the current surface view and the accumulated
/// input state (issues #90/#91/#93: input events are collected between frames
/// and drained by the callback each redraw).
pub fn run_windowed_with<F>(title: &str, frame: F) -> Result<(), RenderError>
where
    F: FnMut(
            &wgpu::Device,
            &wgpu::Queue,
            &mut DebugPipeline,
            &wgpu::TextureView,
            f32,
            &mut InputState,
        ) + 'static,
{
    let event_loop = EventLoop::new().map_err(|error| RenderError::EventLoop(error.to_string()))?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = FrameApp {
        title: title.to_owned(),
        frame: Box::new(frame),
        state: None,
        input: InputState::default(),
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

struct FrameApp {
    title: String,
    frame: Box<FrameCallback>,
    state: Option<WindowState>,
    input: InputState,
    fatal: Option<RenderError>,
}

impl FrameApp {
    fn fail(&mut self, event_loop: &ActiveEventLoop, error: RenderError) {
        eprintln!("error: {error}");
        self.fatal = Some(error);
        event_loop.exit();
    }
}

impl ApplicationHandler for FrameApp {
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
            WindowEvent::CursorMoved { position, .. } => {
                // Normalize to 0..=1 (origin top-left).
                let sx = position.x as f32 / state.config.width.max(1) as f32;
                let sy = position.y as f32 / state.config.height.max(1) as f32;
                self.input.mouse_moved(sx, sy);
                state.window.request_redraw();
            }
            WindowEvent::MouseInput {
                button,
                state: button_state,
                ..
            } => {
                self.input
                    .mouse_button(button, button_state == winit::event::ElementState::Pressed);
                state.window.request_redraw();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let delta_f = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, y) => y,
                    winit::event::MouseScrollDelta::PixelDelta(p) => p.y as f32 / 50.0,
                };
                self.input.mouse_wheel(delta_f);
                state.window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let winit::keyboard::PhysicalKey::Code(key) = event.physical_key {
                    let pressed = event.state == winit::event::ElementState::Pressed;
                    self.input.key(key, pressed);
                    state.window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                match state.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(frame)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                        let view = frame
                            .texture
                            .create_view(&wgpu::TextureViewDescriptor::default());
                        // The caller's frame callback draws the scene (demo
                        // debug geometry in `run_windowed`, the actual
                        // simulation snapshot in battle mode, issue #63). The
                        // aspect is recomputed per frame from the current
                        // surface size, so resizing keeps geometry in place.
                        let aspect = state.config.width as f32 / state.config.height.max(1) as f32;
                        (self.frame)(
                            &state.backend.device,
                            &state.backend.queue,
                            &mut state.pipeline,
                            &view,
                            aspect,
                            &mut self.input,
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
