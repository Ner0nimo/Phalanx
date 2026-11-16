//! Minimal debug-geometry render pipeline (issue #62): colored triangles and
//! rectangles drawn through a real wgpu render pipeline with WGSL shaders,
//! plus a minimal orthographic projection suitable for a top-down RTS
//! battlefield.
//!
//! Design notes:
//! - One vertex format (`position: vec2<f32>` in clip space + `color:
//!   vec4<f32>`), one vertex buffer per frame, one draw call for all shapes:
//!   multiple differently-colored objects per frame by construction.
//! - [`OrthoProjection`] maps world/map coordinates (meters, x right, z
//!   down) to wgpu clip space; it is a pure CPU function, so it is unit-tested
//!   without a GPU, and recomputed per frame from the current view aspect so
//!   geometry stays correctly positioned when the window is resized.
//! - No CPU framebuffer rendering or screenshot blitting anywhere: pixels only
//!   leave the GPU through the offscreen readback used for verification.

/// One debug vertex: clip-space position + RGBA color.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vertex {
    /// Clip-space position (already projected).
    pub position: [f32; 2],
    /// Linear RGBA color.
    pub color: [f32; 4],
}

/// The vertex attributes: Float32x2 at location 0, Float32x4 at location 1.
const VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; 2] =
    wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4];

impl Vertex {
    /// The vertex buffer layout: Float32x2 at location 0, Float32x4 at 1.
    pub fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &VERTEX_ATTRIBUTES,
        }
    }

    /// Encodes vertices as little-endian bytes (safe; matches the layout:
    /// two f32 then four f32, no padding — size_of::<Vertex>() == 24).
    fn to_bytes(vertices: &[Vertex]) -> Vec<u8> {
        assert_eq!(std::mem::size_of::<Vertex>(), 24, "no padding allowed");
        let mut out = Vec::with_capacity(vertices.len() * 24);
        for vertex in vertices {
            out.extend_from_slice(&vertex.position[0].to_le_bytes());
            out.extend_from_slice(&vertex.position[1].to_le_bytes());
            for channel in vertex.color {
                out.extend_from_slice(&channel.to_le_bytes());
            }
        }
        out
    }
}

const SHADER_WGSL: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs(@location(0) in_position: vec2<f32>, @location(1) in_color: vec4<f32>) -> VertexOutput {
    var out: VertexOutput;
    out.position = vec4<f32>(in_position, 0.0, 1.0);
    out.color = in_color;
    return out;
}

@fragment
fn fs(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

/// A debug shape in world coordinates (meters, x right / z down).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DebugShape {
    /// An equilateral-ish triangle centered at `center` with the given
    /// circumradius.
    Triangle {
        /// Center in world coordinates.
        center: [f32; 2],
        /// Circumradius in meters.
        radius: f32,
        /// Linear RGBA color.
        color: [f32; 4],
    },
    /// An axis-aligned rectangle centered at `center`.
    Rect {
        /// Center in world coordinates.
        center: [f32; 2],
        /// Half extents (x, z) in meters.
        half_size: [f32; 2],
        /// Linear RGBA color.
        color: [f32; 4],
    },
}

impl DebugShape {
    /// The shape's color.
    #[must_use]
    pub fn color(&self) -> [f32; 4] {
        match self {
            Self::Triangle { color, .. } | Self::Rect { color, .. } => *color,
        }
    }

    /// The shape's outline vertices in world coordinates (triangles: 3,
    /// rectangles: 6 as two triangles).
    #[must_use]
    pub fn world_vertices(&self) -> Vec<[f32; 2]> {
        match *self {
            Self::Triangle { center, radius, .. } => {
                // Point-up triangle in world space (z down): top vertex at -z.
                vec![
                    [center[0], center[1] - radius],
                    [center[0] - radius * 0.866_025_4, center[1] + radius * 0.5],
                    [center[0] + radius * 0.866_025_4, center[1] + radius * 0.5],
                ]
            }
            Self::Rect {
                center, half_size, ..
            } => {
                let (cx, cz) = (center[0], center[1]);
                let (hx, hz) = (half_size[0], half_size[1]);
                vec![
                    [cx - hx, cz - hz],
                    [cx + hx, cz - hz],
                    [cx + hx, cz + hz],
                    [cx - hx, cz - hz],
                    [cx + hx, cz + hz],
                    [cx - hx, cz + hz],
                ]
            }
        }
    }
}

/// The demo scene drawn in graphical mode: a background clear plus three
/// distinctly colored shapes (issue #62: multiple objects, different colors,
/// one frame).
#[must_use]
pub fn demo_shapes() -> Vec<DebugShape> {
    vec![
        DebugShape::Triangle {
            center: [-40.0, -30.0],
            radius: 12.0,
            color: [0.95, 0.25, 0.25, 1.0],
        },
        DebugShape::Rect {
            center: [30.0, -20.0],
            half_size: [14.0, 10.0],
            color: [0.25, 0.75, 0.30, 1.0],
        },
        DebugShape::Rect {
            center: [0.0, 45.0],
            half_size: [20.0, 8.0],
            color: [0.30, 0.45, 0.95, 1.0],
        },
    ]
}

/// A minimal orthographic projection for a top-down RTS battlefield: maps a
/// world rectangle (meters; x right, z down) onto wgpu clip space (x right,
/// y up), flipping z so map-down is screen-down. Pure and unit-testable.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OrthoProjection {
    left: f32,
    right: f32,
    bottom: f32,
    top: f32,
}

impl OrthoProjection {
    /// The default battlefield view: the 200x200 m Crossing map centered at
    /// the origin (x and z in [-100, 100]).
    pub const CROSSING_VIEW: Self = Self {
        left: -100.0,
        right: 100.0,
        bottom: -100.0,
        top: 100.0,
    };

    /// Creates a projection over a world rectangle.
    #[must_use]
    pub const fn new(left: f32, right: f32, bottom: f32, top: f32) -> Self {
        Self {
            left,
            right,
            bottom,
            top,
        }
    }

    /// Returns the same world rectangle adjusted to the given view aspect
    /// (width/height), expanding horizontally when the view is wider — so
    /// geometry keeps its shape when the window is resized.
    #[must_use]
    pub fn for_aspect(self, aspect: f32) -> Self {
        let world_aspect = (self.right - self.left) / (self.top - self.bottom);
        if aspect > world_aspect {
            let half_h = (self.top - self.bottom) / 2.0;
            let half_w = half_h * aspect;
            let mid_x = (self.left + self.right) / 2.0;
            Self {
                left: mid_x - half_w,
                right: mid_x + half_w,
                ..self
            }
        } else {
            let half_w = (self.right - self.left) / 2.0;
            let half_h = half_w / aspect;
            let mid_z = (self.bottom + self.top) / 2.0;
            Self {
                bottom: mid_z - half_h,
                top: mid_z + half_h,
                ..self
            }
        }
    }

    /// Maps a world coordinate to clip space.
    #[must_use]
    pub fn world_to_clip(&self, x: f32, z: f32) -> [f32; 2] {
        let cx = 2.0 * (x - self.left) / (self.right - self.left) - 1.0;
        // Flip z: world +z (map down) maps to clip -y (screen down).
        let cy = 1.0 - 2.0 * (z - self.bottom) / (self.top - self.bottom);
        [cx, cy]
    }
}

/// The debug-geometry render pipeline: WGSL vertex+fragment shaders, one
/// vertex buffer, one draw call for all shapes in a frame.
pub struct DebugPipeline {
    pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    vertex_capacity: usize,
}

impl DebugPipeline {
    /// Creates the pipeline targeting `format` (the surface format windowed,
    /// `OffscreenTarget::FORMAT` offscreen).
    #[must_use]
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("phalanx-debug-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_WGSL.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("phalanx-debug-layout"),
            bind_group_layouts: &[],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("phalanx-debug-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(Vertex::layout())],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("phalanx-debug-vertices"),
            size: 4096,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            vertex_buffer,
            vertex_capacity: 4096 / std::mem::size_of::<Vertex>(),
        }
    }

    /// Renders one frame: clear `view` to `clear`, then draw all `shapes`
    /// (projected with `projection`) in a single draw call.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        clear: wgpu::Color,
        projection: &OrthoProjection,
        shapes: &[DebugShape],
    ) {
        // Project all shape vertices to clip space on the CPU (pure
        // projection, recomputed per frame → resize-safe).
        let mut vertices: Vec<Vertex> = Vec::new();
        for shape in shapes {
            for world in shape.world_vertices() {
                let clip = projection.world_to_clip(world[0], world[1]);
                vertices.push(Vertex {
                    position: clip,
                    color: shape.color(),
                });
            }
        }
        if vertices.len() > self.vertex_capacity {
            self.vertex_capacity = vertices.len().next_power_of_two();
            self.vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("phalanx-debug-vertices"),
                size: (self.vertex_capacity * std::mem::size_of::<Vertex>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        queue.write_buffer(&self.vertex_buffer, 0, &Vertex::to_bytes(&vertices));

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("phalanx-debug-encoder"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("phalanx-debug-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            pass.draw(0..vertices.len() as u32, 0..1);
        }
        queue.submit([encoder.finish()]);
    }
}

/// Counts pixels in an RGBA8 readback matching `color` (per-channel tolerance
/// 1 for unorm rounding; alpha must be 255). Used to verify on-GPU rendering.
#[must_use]
pub fn count_color(pixels: &[u8], color: [f32; 4]) -> usize {
    let expected = [
        (color[0] * 255.0).round() as i32,
        (color[1] * 255.0).round() as i32,
        (color[2] * 255.0).round() as i32,
        255,
    ];
    pixels
        .chunks_exact(4)
        .filter(|chunk| {
            let found = [
                chunk[0] as i32,
                chunk[1] as i32,
                chunk[2] as i32,
                chunk[3] as i32,
            ];
            (0..4).all(|c| (found[c] - expected[c]).abs() <= 1)
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ortho_projection_maps_world_rect_to_clip_corners() {
        let p = OrthoProjection::CROSSING_VIEW;
        assert_eq!(p.world_to_clip(-100.0, -100.0), [-1.0, 1.0]);
        assert_eq!(p.world_to_clip(100.0, -100.0), [1.0, 1.0]);
        assert_eq!(p.world_to_clip(-100.0, 100.0), [-1.0, -1.0]);
        assert_eq!(p.world_to_clip(100.0, 100.0), [1.0, -1.0]);
        assert_eq!(p.world_to_clip(0.0, 0.0), [0.0, 0.0]);
    }

    #[test]
    fn ortho_projection_for_aspect_preserves_shape() {
        let p = OrthoProjection::CROSSING_VIEW.for_aspect(2.0);
        // Wider view: x range expands, z range unchanged.
        assert_eq!(p.world_to_clip(0.0, 0.0), [0.0, 0.0]);
        let corner = p.world_to_clip(100.0, 0.0);
        assert!(corner[0] < 1.0, "world x=100 is no longer the right edge");
        let wide = OrthoProjection::CROSSING_VIEW.for_aspect(0.5);
        let corner = wide.world_to_clip(0.0, 100.0);
        assert!(corner[1] > -1.0, "world z=100 is no longer the bottom edge");
    }

    #[test]
    fn shapes_produce_triangles_and_rectangles_in_world_space() {
        let tri = DebugShape::Triangle {
            center: [1.0, 2.0],
            radius: 10.0,
            color: [1.0, 0.0, 0.0, 1.0],
        };
        assert_eq!(tri.world_vertices().len(), 3);
        let rect = DebugShape::Rect {
            center: [0.0, 0.0],
            half_size: [5.0, 3.0],
            color: [0.0, 1.0, 0.0, 1.0],
        };
        assert_eq!(rect.world_vertices().len(), 6);
        // Demo scene: 3 shapes with 3 distinct colors.
        let demo = demo_shapes();
        assert_eq!(demo.len(), 3);
        let colors: Vec<[f32; 4]> = demo.iter().map(|s| s.color()).collect();
        assert_ne!(colors[0], colors[1]);
        assert_ne!(colors[1], colors[2]);
        assert_ne!(colors[0], colors[2]);
    }

    #[test]
    fn count_color_matches_within_unorm_tolerance() {
        let color = [0.95, 0.25, 0.25, 1.0];
        let expected = [
            (color[0] * 255.0_f32).round() as u8,
            (color[1] * 255.0_f32).round() as u8,
            (color[2] * 255.0_f32).round() as u8,
            255,
        ];
        let mut pixels = vec![0u8, 0, 0, 255]; // background black
        pixels.extend_from_slice(&expected);
        pixels.extend_from_slice(&[expected[0].wrapping_add(1), expected[1], expected[2], 255]); // within tolerance
        pixels.extend_from_slice(&[expected[0].wrapping_add(9), expected[1], expected[2], 255]); // outside
        assert_eq!(count_color(&pixels, color), 2);
    }

    #[test]
    fn offscreen_debug_render_verifies_shape_colors_when_adapter_exists() {
        match crate::init_headless() {
            Err(crate::BackendError::NoAdapter(_)) => {}
            Err(other) => panic!("unexpected backend error: {other}"),
            Ok((backend, report)) => {
                println!("{report}");
                let mut pipeline =
                    DebugPipeline::new(&backend.device, crate::OffscreenTarget::FORMAT);
                let target =
                    crate::OffscreenTarget::new(&backend.device, 128, 128).expect("target");
                let clear = crate::DEFAULT_CLEAR;
                let shapes = demo_shapes();
                let projection = OrthoProjection::CROSSING_VIEW.for_aspect(1.0);
                pipeline.render(
                    &backend.device,
                    &backend.queue,
                    &target.view,
                    clear.to_wgpu(),
                    &projection,
                    &shapes,
                );
                let pixels = target
                    .read_back(&backend.device, &backend.queue)
                    .expect("readback");
                // Background is the clear color everywhere except shapes.
                crate::verify_clear_pixels_background(
                    &pixels,
                    target.width,
                    target.height,
                    clear,
                    &shapes,
                )
                .expect("frame must be clear color + shape colors only");
                // Every demo shape is visibly present (GPU-generated pixels).
                for shape in &shapes {
                    let count = count_color(&pixels, shape.color());
                    assert!(count > 0, "shape color must appear in the readback");
                }
            }
        }
    }
}
