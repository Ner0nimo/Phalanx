//! The combined per-frame renderer (issue #104): owns the 2D debug pipeline
//! (issue #62 demo path), the depth-tested 3D scene/HUD pipelines
//! ([`crate::scene3d`]), and the depth target, so the windowed loop hands the
//! frame callback one object with both render paths:
//!
//! - [`FrameRenderer::render_debug`] — the original 2D debug-geometry frame
//!   (no depth attachment), used by the plain `--graphical` demo;
//! - [`FrameRenderer::render_battle`] — one render pass with a color + depth
//!   attachment: the 3D scene (depth-tested, writes depth), then the HUD
//!   (depth-always overlay), so selection rings, health bars, order markers,
//!   the drag box, and the perf strip stay correctly ordered on top of the 3D
//!   scene.
//!
//! The depth texture carries `COPY_SRC` so offscreen verification can read it
//! back (issue #104 acceptance: geometry/depth output verified on GPU
//! machines, structured skip on GPU-less machines).

use crate::geometry::{DebugPipeline, DebugShape, OrthoProjection};
use crate::math::Mat4;
use crate::present::PresentError;
use crate::scene3d::{DEPTH_FORMAT, HudVertex, ScenePipelines, SceneVertex};

/// Creates the depth target texture + view (recreated on resize).
#[must_use]
pub fn create_depth_target(
    device: &wgpu::Device,
    width: u32,
    height: u32,
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("phalanx-depth"),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

/// Reads back the whole depth target as `width * height` f32 values (row-major,
/// no padding). The texture must have been created with `COPY_SRC`.
pub fn read_back_depth(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Result<Vec<f32>, PresentError> {
    // COPY_BYTES_PER_ROW_ALIGNMENT is 256; Depth32Float is 4 bytes per texel.
    let bytes_per_row = (width * 4).div_ceil(256) * 256;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("phalanx-depth-readback"),
        size: u64::from(bytes_per_row) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("phalanx-depth-readback-encoder"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::DepthOnly,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);
    let _ = device.poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: None,
    });
    let (tx, rx) = std::sync::mpsc::channel();
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result.is_ok());
        });
    let _ = device.poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: None,
    });
    if !rx.recv().unwrap_or(false) {
        return Err(PresentError::Readback("depth buffer map failed".to_owned()));
    }
    let mapped = buffer
        .slice(..)
        .get_mapped_range()
        .map_err(|error| PresentError::Readback(error.to_string()))?;
    let mut depths = vec![1.0_f32; (width * height) as usize];
    for row in 0..height as usize {
        let start = row * bytes_per_row as usize;
        for col in 0..width as usize {
            let offset = start + col * 4;
            depths[row * width as usize + col] =
                f32::from_le_bytes(mapped[offset..offset + 4].try_into().expect("4 bytes"));
        }
    }
    drop(mapped);
    buffer.unmap();
    Ok(depths)
}

/// One depth-tested 3D battle frame's inputs (issue #104): the
/// view-projection matrix and the three vertex batches (scene, world-space
/// HUD, screen-space HUD strip).
pub struct BattleFrame<'a> {
    /// The view-projection matrix.
    pub vp: &'a Mat4,
    /// Lit scene geometry (ground, obstacles, units).
    pub scene: &'a [SceneVertex],
    /// World-space HUD (health bars, rings, markers, drag box).
    pub world_hud: &'a [HudVertex],
    /// Screen-space HUD (perf strip), drawn with the identity matrix.
    pub screen_hud: &'a [HudVertex],
}

/// The per-frame renderer handed to the windowed frame callback: the 2D debug
/// pipeline, the 3D scene/HUD pipelines, and the depth target.
pub struct FrameRenderer {
    /// The 2D debug pipeline (issue #62 demo path).
    pub debug: DebugPipeline,
    scene: ScenePipelines,
    depth: wgpu::Texture,
    depth_view: wgpu::TextureView,
    /// Current framebuffer width in pixels.
    pub width: u32,
    /// Current framebuffer height in pixels.
    pub height: u32,
}

impl FrameRenderer {
    /// Creates the renderer for a framebuffer of `width` x `height` targeting
    /// `color_format` (the surface format windowed, `OffscreenTarget::FORMAT`
    /// offscreen).
    #[must_use]
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        color_format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Self {
        let (depth, depth_view) = create_depth_target(device, width, height);
        Self {
            debug: DebugPipeline::new(device, color_format),
            scene: ScenePipelines::new(device, queue, color_format),
            depth,
            depth_view,
            width: width.max(1),
            height: height.max(1),
        }
    }

    /// Recreates the depth target after a resize.
    pub fn resize(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.width = width;
        self.height = height;
        let (depth, depth_view) = create_depth_target(device, width, height);
        self.depth = depth;
        self.depth_view = depth_view;
    }

    /// The 2D debug frame (issue #62): clear + shapes, no depth attachment.
    pub fn render_debug(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        clear: wgpu::Color,
        projection: &OrthoProjection,
        shapes: &[DebugShape],
    ) {
        self.debug
            .render(device, queue, view, clear, projection, shapes);
    }

    /// One depth-tested 3D battle frame (issue #104): clear color + depth in a
    /// single render pass, then the scene (depth-tested), the world-space HUD,
    /// and the screen-space HUD strip (both depth-always overlays).
    pub fn render_battle(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        clear: wgpu::Color,
        frame: &BattleFrame<'_>,
    ) {
        let counts = self.scene.upload(
            device,
            queue,
            frame.vp,
            frame.scene,
            frame.world_hud,
            frame.screen_hud,
        );
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("phalanx-battle-encoder"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("phalanx-battle-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                ..Default::default()
            });
            self.scene.draw(&mut pass, counts);
        }
        queue.submit([encoder.finish()]);
    }

    /// Reads back the depth buffer of the last rendered battle frame
    /// (`width * height` f32 values).
    pub fn read_back_depth(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<Vec<f32>, PresentError> {
        read_back_depth(device, queue, &self.depth, self.width, self.height)
    }
}
