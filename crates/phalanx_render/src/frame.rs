//! The combined per-frame renderer (issues #104/#97): owns the 2D debug
//! pipeline (issue #62 demo path), the temporal 3D pipelines
//! ([`crate::scene3d`]), and the render-resolution targets (scene color,
//! motion vectors, depth), so the windowed loop hands the frame callback one
//! object with both render paths:
//!
//! - [`FrameRenderer::render_debug`] — the original 2D debug-geometry frame
//!   (no depth attachment), used by the plain `--graphical` demo;
//! - [`FrameRenderer::render_battle_temporal`] — the depth-tested 3D battle
//!   frame with temporal outputs (issue #97): pass 1 renders the scene +
//!   world HUD into the scene-color and motion attachments at *render*
//!   resolution with the depth attachment; pass 2 presents to the output view
//!   at *output* resolution via the bilinear-fallback fullscreen upscale, then
//!   composites the screen-space HUD strip at output resolution. The applied
//!   jitter offset is recorded ([`FrameRenderer::last_jitter`]) and the
//!   temporal history advances per frame; a resize notes a
//!   [`Discontinuity::ResolutionChange`].
//!
//! The depth and motion textures carry `COPY_SRC` so offscreen verification
//! can read them back (issue #104/#97 acceptance: geometry/depth/motion
//! verified on GPU machines, structured skip on GPU-less machines).

use crate::geometry::{DebugPipeline, DebugShape, OrthoProjection};
use crate::math::Mat4;
use crate::present::PresentError;
use crate::scene3d::{DEPTH_FORMAT, HudVertex, ScenePipelines, SceneVertex};
use crate::temporal::{
    Discontinuity, MOTION_FORMAT, SCENE_COLOR_FORMAT, TemporalHistory, TemporalRenderConfig,
    half_to_f32, jitter_translation,
};

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

/// Creates a render-resolution target texture + view with the given format and
/// usages.
fn create_target(
    device: &wgpu::Device,
    label: &str,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
    extra_usage: wgpu::TextureUsages,
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | extra_usage,
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
    read_back_texels(device, queue, texture, width, height, 4, |bytes| {
        f32::from_le_bytes(bytes.try_into().expect("4 bytes"))
    })
}

/// Reads back the whole motion target as `width * height` `[f32; 2]` UV deltas
/// (row-major, no padding) from an `Rgba16Float` texture with `COPY_SRC`.
pub fn read_back_motion(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Result<Vec<[f32; 2]>, PresentError> {
    read_back_texels(device, queue, texture, width, height, 8, |bytes| {
        let x = half_to_f32(u16::from_le_bytes([bytes[0], bytes[1]]));
        let y = half_to_f32(u16::from_le_bytes([bytes[2], bytes[3]]));
        [x, y]
    })
}

/// Generic texel readback: copies `texture` (COPY_SRC) to a buffer and decodes
/// `texel_size`-byte texels row-major with `decode`.
fn read_back_texels<T>(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
    texel_size: u32,
    decode: impl Fn(&[u8]) -> T,
) -> Result<Vec<T>, PresentError> {
    let bytes_per_row = (width * texel_size).div_ceil(256) * 256;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("phalanx-texel-readback"),
        size: u64::from(bytes_per_row) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("phalanx-readback-encoder"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
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
        return Err(PresentError::Readback("texel buffer map failed".to_owned()));
    }
    let mapped = buffer
        .slice(..)
        .get_mapped_range()
        .map_err(|error| PresentError::Readback(error.to_string()))?;
    let mut out = Vec::with_capacity((width * height) as usize);
    for row in 0..height as usize {
        let start = row * bytes_per_row as usize;
        for col in 0..width as usize {
            let offset = start + col * texel_size as usize;
            out.push(decode(&mapped[offset..offset + texel_size as usize]));
        }
    }
    drop(mapped);
    buffer.unmap();
    Ok(out)
}

/// One temporal 3D battle frame's inputs (issue #97): the unjittered
/// view-projection matrix (jitter is applied by the renderer and reported),
/// the vertex batches, the jitter offset to apply this frame, and the linear
/// exposure multiplier.
pub struct BattleTemporalFrame<'a> {
    /// The view-projection matrix for this frame (before jitter).
    pub vp: &'a Mat4,
    /// Lit scene geometry (ground, obstacles, units).
    pub scene: &'a [SceneVertex],
    /// World-space HUD (health bars, rings, markers, drag box).
    pub world_hud: &'a [HudVertex],
    /// Screen-space HUD (perf strip), drawn at output resolution.
    pub screen_hud: &'a [HudVertex],
    /// The NDC jitter offset applied to `vp` and reported for this frame.
    pub jitter: [f32; 2],
    /// Linear exposure multiplier (1.0 = neutral).
    pub exposure: f32,
}

/// The per-frame renderer handed to the windowed frame callback: the 2D debug
/// pipeline, the temporal 3D pipelines, and the render-resolution targets.
pub struct FrameRenderer {
    /// The 2D debug pipeline (issue #62 demo path).
    pub debug: DebugPipeline,
    scene: ScenePipelines,
    scene_color: wgpu::Texture,
    scene_color_view: wgpu::TextureView,
    motion: wgpu::Texture,
    motion_view: wgpu::TextureView,
    depth: wgpu::Texture,
    depth_view: wgpu::TextureView,
    sampler: wgpu::Sampler,
    present_bind: wgpu::BindGroup,
    /// Output framebuffer width in pixels.
    pub width: u32,
    /// Output framebuffer height in pixels.
    pub height: u32,
    render_w: u32,
    render_h: u32,
    config: TemporalRenderConfig,
    prev_vp: Mat4,
    last_jitter: [f32; 2],
    history: TemporalHistory,
    prev_positions: std::collections::HashMap<u32, [f32; 2]>,
}

impl FrameRenderer {
    /// Creates the renderer for an output framebuffer of `width` x `height`
    /// targeting `output_format`, rendering the 3D scene at `config`'s render
    /// resolution.
    #[must_use]
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        output_format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        config: TemporalRenderConfig,
    ) -> Self {
        let (render_w, render_h) = config.render_size(width, height);
        let (scene_color, scene_color_view) = create_target(
            device,
            "phalanx-scene-color",
            SCENE_COLOR_FORMAT,
            render_w,
            render_h,
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
        );
        let (motion, motion_view) = create_target(
            device,
            "phalanx-motion",
            MOTION_FORMAT,
            render_w,
            render_h,
            wgpu::TextureUsages::COPY_SRC,
        );
        let (depth, depth_view) = create_depth_target(device, render_w, render_h);
        let scene = ScenePipelines::new(
            device,
            queue,
            SCENE_COLOR_FORMAT,
            MOTION_FORMAT,
            output_format,
        );
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("phalanx-present-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let present_bind = make_present_bind(
            device,
            scene.present_bind_group_layout(),
            &scene_color_view,
            &sampler,
        );
        Self {
            debug: DebugPipeline::new(device, output_format),
            scene,
            scene_color,
            scene_color_view,
            motion,
            motion_view,
            depth,
            depth_view,
            sampler,
            present_bind,
            width: width.max(1),
            height: height.max(1),
            render_w,
            render_h,
            config,
            prev_vp: Mat4::IDENTITY,
            last_jitter: [0.0, 0.0],
            history: TemporalHistory::new(),
            prev_positions: std::collections::HashMap::new(),
        }
    }

    /// The render (scene) resolution in pixels.
    #[must_use]
    pub fn scene_size(&self) -> (u32, u32) {
        (self.render_w, self.render_h)
    }

    /// The exact jitter offset applied to the last rendered frame.
    #[must_use]
    pub fn last_jitter(&self) -> [f32; 2] {
        self.last_jitter
    }

    /// The temporal history state (reset on discontinuities).
    #[must_use]
    pub fn history(&self) -> &TemporalHistory {
        &self.history
    }

    /// Records a discontinuity (camera cut, resolution change, map load, frame
    /// skip): temporal history is invalidated so stale history is never used.
    pub fn note_discontinuity(&mut self, reason: Discontinuity) {
        self.history.note_discontinuity(reason);
        // Previous positions are also invalidated so the next frame contributes
        // zero object motion instead of ghosting across the discontinuity.
        self.prev_positions.clear();
    }

    /// Per-entity previous-frame positions (keyed by entity index) for motion
    /// vectors; pass to [`crate::build_scene_geometry`].
    #[must_use]
    pub fn prev_positions(&self) -> &std::collections::HashMap<u32, [f32; 2]> {
        &self.prev_positions
    }

    /// Records the snapshot's entity positions as the previous-frame positions
    /// for the next frame's motion vectors. Call after rendering the frame.
    pub fn commit_prev_positions(&mut self, snapshot: &crate::RenderSnapshot) {
        self.prev_positions.clear();
        for entity in &snapshot.entities {
            self.prev_positions
                .insert(entity.entity_index, entity.position);
        }
    }

    /// Recreates the render-resolution targets after an output resize and
    /// notes the [`Discontinuity::ResolutionChange`].
    pub fn resize(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.width = width;
        self.height = height;
        let (render_w, render_h) = self.config.render_size(width, height);
        self.render_w = render_w;
        self.render_h = render_h;
        let (scene_color, scene_color_view) = create_target(
            device,
            "phalanx-scene-color",
            SCENE_COLOR_FORMAT,
            render_w,
            render_h,
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
        );
        let (motion, motion_view) = create_target(
            device,
            "phalanx-motion",
            MOTION_FORMAT,
            render_w,
            render_h,
            wgpu::TextureUsages::COPY_SRC,
        );
        let (depth, depth_view) = create_depth_target(device, render_w, render_h);
        self.scene_color = scene_color;
        self.scene_color_view = scene_color_view;
        self.motion = motion;
        self.motion_view = motion_view;
        self.depth = depth;
        self.depth_view = depth_view;
        self.present_bind = make_present_bind(
            device,
            self.scene.present_bind_group_layout(),
            &self.scene_color_view,
            &self.sampler,
        );
        self.history
            .note_discontinuity(Discontinuity::ResolutionChange);
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

    /// One temporal 3D battle frame (issues #104/#97): pass 1 renders the
    /// scene + world HUD at render resolution into scene-color + motion +
    /// depth (jitter applied to the VP and recorded); pass 2 presents to the
    /// output view at output resolution (bilinear fallback) and composites the
    /// screen-space HUD strip on top.
    pub fn render_battle_temporal(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        output_view: &wgpu::TextureView,
        clear: wgpu::Color,
        frame: &BattleTemporalFrame<'_>,
    ) {
        // Apply and record the exact jitter offset for this frame.
        let vp_jittered = jitter_translation(frame.jitter).mul(frame.vp);
        let uniforms = crate::scene3d::FrameUniforms {
            vp: vp_jittered,
            prev_vp: self.prev_vp,
            exposure: frame.exposure,
        };
        let counts = self
            .scene
            .upload(device, queue, &uniforms, frame.scene, frame.world_hud);
        let screen_hud_n = self.scene.upload_output(device, queue, frame.screen_hud);

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("phalanx-battle-encoder"),
        });
        // Pass 1: scene at render resolution (scene color + motion + depth).
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("phalanx-scene-pass"),
                color_attachments: &[
                    Some(wgpu::RenderPassColorAttachment {
                        view: &self.scene_color_view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(clear),
                            store: wgpu::StoreOp::Store,
                        },
                    }),
                    Some(wgpu::RenderPassColorAttachment {
                        view: &self.motion_view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                    }),
                ],
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
            self.scene.draw_scene(&mut pass, counts);
        }
        // Pass 2: present to the output view (fullscreen upscale + screen HUD).
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("phalanx-present-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: output_view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            self.scene
                .draw_output(&mut pass, &self.present_bind, screen_hud_n);
        }
        queue.submit([encoder.finish()]);
        self.prev_vp = vp_jittered;
        self.last_jitter = frame.jitter;
        self.history.advance_frame();
    }

    /// Reads back the depth buffer of the last rendered battle frame
    /// (`render_w * render_h` f32 values).
    pub fn read_back_depth(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<Vec<f32>, PresentError> {
        read_back_depth(device, queue, &self.depth, self.render_w, self.render_h)
    }

    /// Reads back the motion-vector buffer of the last rendered battle frame
    /// (`render_w * render_h` `[f32; 2]` UV deltas).
    pub fn read_back_motion(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<Vec<[f32; 2]>, PresentError> {
        read_back_motion(device, queue, &self.motion, self.render_w, self.render_h)
    }
}

fn make_present_bind(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    scene_color_view: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("phalanx-present-bind"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(scene_color_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}
