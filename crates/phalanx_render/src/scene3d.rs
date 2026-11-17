//! Minimal depth-tested 3D presentation for the Crossing battle (issue #104).
//!
//! This is a deliberately small M1 proof of "small 3D battlefield and camera",
//! not a production renderer (that is issue #98): the ground plane, static
//! obstacles, and live units are drawn as colored boxes through a real wgpu
//! render pipeline with a depth buffer, under a perspective camera derived
//! from the existing pan/zoom state. All geometry is rebuilt per frame from
//! the immutable [`RenderSnapshot`] — the renderer never reads or mutates
//! authoritative simulation state (ADR-0001 §3.5).
//!
//! Contract notes for the later temporal-upscaling work (issue #97,
//! ADR-0010): the depth buffer uses wgpu's standard convention — NDC z in
//! `[0, 1]`, 0 at the near plane, 1 at the far plane, `CompareFunction::Less`
//! wins (NOT inverted/infinite depth). Motion vectors, camera jitter, and
//! temporal history are intentionally not produced yet (#97).
//!
//! Lighting is a minimal hemisphere term (`AMBIENT + DIFFUSE * max(normal.y,
//! 0)`), exact for the axis-aligned geometry here, so offscreen pixel
//! verification can compute the expected lit colors on the CPU bit-for-bit.

use crate::math::{Mat4, cross, normalize, sub};
use crate::perf::{
    GRAPHICS_CPU_COLOR, MS_TO_M, OVERLAY_BG_COLOR, OVERLAY_STRIP_M, PerfStats, SIMULATION_COLOR,
    SNAPSHOT_COLOR, TOTAL_COLOR,
};
use crate::snapshot::{OBSTACLE_COLOR, RenderSnapshot, team_color};

/// The depth buffer format: 32-bit float, wgpu clip convention (0 = near,
/// 1 = far, less-is-closer).
pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// The terrain/ground color (linear RGBA).
pub const GROUND_COLOR: [f32; 4] = [0.30, 0.40, 0.24, 1.0];
/// Height of obstacle boxes in meters.
pub const OBSTACLE_HEIGHT_M: f32 = 3.0;
/// Height of unit boxes in meters.
pub const UNIT_HEIGHT_M: f32 = 1.5;

/// Hemisphere ambient term of the minimal lighting model.
pub const AMBIENT: f32 = 0.55;
/// Zenith contribution for upward-facing normals.
pub const DIFFUSE: f32 = 0.45;

/// The fixed camera pitch in radians (55 degrees), steep enough to read the
/// battlefield as 3D while keeping most of the map visible.
pub const CAMERA_PITCH_RAD: f32 = 0.959_931_1;
/// The default vertical field of view in radians (50 degrees).
pub const CAMERA_FOV_Y_RAD: f32 = 0.872_664_6;

/// The lit color of a base color under the hemisphere lighting model — mirrors
/// the WGSL fragment shader exactly (`color.rgb * factor`).
#[must_use]
pub fn lit_color(color: [f32; 4], factor: f32) -> [f32; 4] {
    [
        color[0] * factor,
        color[1] * factor,
        color[2] * factor,
        color[3],
    ]
}

/// One lit scene vertex: world position + normal + linear RGBA color.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SceneVertex {
    /// World position (meters; x/z ground plane, y up).
    pub position: [f32; 3],
    /// Surface normal (unit axis for the box/ground geometry).
    pub normal: [f32; 3],
    /// Linear RGBA base color.
    pub color: [f32; 4],
}

/// One unlit HUD vertex: world position + linear RGBA color, drawn with the
/// same view-projection but depth-always and unlit so it overlays the 3D
/// scene. Screen-space strip vertices use z = 0 with x/y as NDC.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HudVertex {
    /// Position (world meters, or NDC for the screen-space strip).
    pub position: [f32; 3],
    /// Linear RGBA color.
    pub color: [f32; 4],
}

const SCENE_ATTRIBUTES: [wgpu::VertexAttribute; 3] =
    wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x4];
const HUD_ATTRIBUTES: [wgpu::VertexAttribute; 2] =
    wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x4];

impl SceneVertex {
    fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<SceneVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &SCENE_ATTRIBUTES,
        }
    }

    fn to_bytes(vertices: &[SceneVertex]) -> Vec<u8> {
        assert_eq!(std::mem::size_of::<SceneVertex>(), 40, "no padding allowed");
        let mut out = Vec::with_capacity(vertices.len() * 40);
        for v in vertices {
            for component in v
                .position
                .iter()
                .chain(v.normal.iter())
                .chain(v.color.iter())
            {
                out.extend_from_slice(&component.to_le_bytes());
            }
        }
        out
    }
}

impl HudVertex {
    fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<HudVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &HUD_ATTRIBUTES,
        }
    }

    fn to_bytes(vertices: &[HudVertex]) -> Vec<u8> {
        assert_eq!(std::mem::size_of::<HudVertex>(), 28, "no padding allowed");
        let mut out = Vec::with_capacity(vertices.len() * 28);
        for v in vertices {
            for component in v.position.iter().chain(v.color.iter()) {
                out.extend_from_slice(&component.to_le_bytes());
            }
        }
        out
    }
}

const SCENE_SHADER_WGSL: &str = r#"
struct Uniforms {
    vp: mat4x4<f32>,
    prev_vp: mat4x4<f32>,
    exposure: vec4<f32>,
};
@group(0) @binding(0) var<uniform> uniforms: Uniforms;

@group(1) @binding(0) var scene_tex: texture_2d<f32>;
@group(1) @binding(1) var scene_sampler: sampler;

struct SceneIn {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec4<f32>,
};
struct HudIn {
    @location(0) position: vec3<f32>,
    @location(1) color: vec4<f32>,
};
struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) clip_cur: vec4<f32>,
    @location(3) clip_prev: vec4<f32>,
};
struct FsOut {
    @location(0) color: vec4<f32>,
    @location(1) motion: vec4<f32>,
};

@vertex
fn vs_scene(in: SceneIn) -> VsOut {
    var out: VsOut;
    out.clip_cur = uniforms.vp * vec4<f32>(in.position, 1.0);
    out.position = out.clip_cur;
    out.clip_prev = uniforms.prev_vp * vec4<f32>(in.position, 1.0);
    out.color = in.color;
    out.normal = in.normal;
    return out;
}

@vertex
fn vs_hud(in: HudIn) -> VsOut {
    var out: VsOut;
    out.clip_cur = uniforms.vp * vec4<f32>(in.position, 1.0);
    out.position = out.clip_cur;
    out.clip_prev = uniforms.prev_vp * vec4<f32>(in.position, 1.0);
    out.color = in.color;
    out.normal = vec3<f32>(0.0, 1.0, 0.0);
    return out;
}

// Motion vectors per crate::temporal::MOTION_CONVENTION: UV-space delta
// (cur - prev) in fractions of the full target, +x right, +y down; both
// frames use jittered projections, so the jitter delta is included.
fn motion_of(in: VsOut) -> vec2<f32> {
    let cur = in.clip_cur.xy / in.clip_cur.w;
    let prev = in.clip_prev.xy / in.clip_prev.w;
    return (cur - prev) * vec2<f32>(0.5, -0.5);
}

@fragment
fn fs_lit(in: VsOut) -> FsOut {
    let factor = 0.55 + 0.45 * max(in.normal.y, 0.0);
    var out: FsOut;
    out.color = vec4<f32>(in.color.rgb * uniforms.exposure.x * factor, in.color.a);
    out.motion = vec4<f32>(motion_of(in), 0.0, 1.0);
    return out;
}

@fragment
fn fs_flat(in: VsOut) -> FsOut {
    var out: FsOut;
    out.color = in.color;
    // World HUD is composited at output resolution; it contributes no motion.
    out.motion = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    return out;
}

// Present pass (bilinear fallback upscaler): fullscreen triangle sampling the
// scene-color attachment at render resolution into the output view.
struct PresentOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_present(@builtin(vertex_index) index: u32) -> PresentOut {
    var out: PresentOut;
    let x = f32((index << 1u) & 2u);
    let y = f32(index & 2u);
    out.position = vec4<f32>(x * 2.0 - 1.0, y * 2.0 - 1.0, 0.0, 1.0);
    out.uv = vec2<f32>(x, 1.0 - y);
    return out;
}

@fragment
fn fs_present(in: PresentOut) -> @location(0) vec4<f32> {
    return textureSample(scene_tex, scene_sampler, in.uv);
}
"#;

/// The per-frame uniform block: current (jittered) view-projection, previous
/// frame's view-projection (for motion vectors), and linear exposure.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameUniforms {
    /// Current frame's view-projection (jitter already applied).
    pub vp: Mat4,
    /// Previous frame's view-projection (jittered), for motion vectors.
    pub prev_vp: Mat4,
    /// Linear exposure multiplier (1.0 = neutral).
    pub exposure: f32,
}

impl FrameUniforms {
    /// The identity/neutral uniforms (used for output-space HUD draws).
    #[must_use]
    pub fn identity() -> Self {
        Self {
            vp: Mat4::IDENTITY,
            prev_vp: Mat4::IDENTITY,
            exposure: 1.0,
        }
    }

    /// Encodes as 144 bytes (two mat4 + f32 + vec3 pad), column-major LE.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 144] {
        let mut out = [0u8; 144];
        out[0..64].copy_from_slice(&self.vp.to_bytes());
        out[64..128].copy_from_slice(&self.prev_vp.to_bytes());
        out[128..132].copy_from_slice(&self.exposure.to_le_bytes());
        out
    }
}

/// The perspective 3D camera (issue #104): derived from the existing pan/zoom
/// state (`center` in world x/z, `view_height_m` = visible ground height) plus
/// a yaw angle; the pitch is fixed. Pure CPU math, unit-testable without a GPU.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera3D {
    /// Camera eye position in world meters.
    pub eye: [f32; 3],
    /// Look-at target (the ground point at the view center).
    pub target: [f32; 3],
    /// Up vector.
    pub up: [f32; 3],
    /// Vertical field of view in radians.
    pub fov_y_rad: f32,
    /// Near plane distance in meters.
    pub near: f32,
    /// Far plane distance in meters.
    pub far: f32,
}

impl Camera3D {
    /// Builds the camera for a pan/zoom view: `center` (world x/z) and
    /// `view_height_m` (the visible ground-plane height at the target) keep the
    /// 2D camera's zoom semantics; `yaw_rad` rotates around the target
    /// (0 = looking toward -z, i.e. map north up on screen).
    #[must_use]
    pub fn from_view(center: [f32; 2], view_height_m: f32, yaw_rad: f32) -> Self {
        let fov_y = CAMERA_FOV_Y_RAD;
        // Eye distance so the ground-plane footprint at the target spans about
        // view_height_m vertically: d = (view_height / (2 tan(fov/2))) / sin(pitch).
        let distance =
            (view_height_m * CAMERA_PITCH_RAD.sin()) / (2.0 * (fov_y / 2.0).tan()).max(0.01);
        let distance = distance.max(2.0);
        let eye = [
            center[0] + distance * CAMERA_PITCH_RAD.cos() * yaw_rad.sin(),
            distance * CAMERA_PITCH_RAD.sin(),
            center[1] + distance * CAMERA_PITCH_RAD.cos() * yaw_rad.cos(),
        ];
        Self {
            eye,
            target: [center[0], 0.0, center[1]],
            up: [0.0, 1.0, 0.0],
            fov_y_rad: fov_y,
            near: 1.0,
            far: 4000.0,
        }
    }

    /// The camera basis: (forward, right, up) unit vectors.
    fn basis(&self) -> ([f32; 3], [f32; 3], [f32; 3]) {
        let forward = normalize(sub(self.target, self.eye));
        let right = normalize(cross(forward, self.up));
        let up = cross(right, forward);
        (forward, right, up)
    }

    /// The view-projection matrix for the given view aspect (width/height).
    #[must_use]
    pub fn view_projection(&self, aspect: f32) -> Mat4 {
        Mat4::perspective(self.fov_y_rad, aspect.max(0.01), self.near, self.far)
            .mul(&Mat4::look_at(self.eye, self.target, self.up))
    }

    /// Projects a world point to NDC (x/y in [-1,1], z in [0,1]); returns `None`
    /// when the point is behind the camera.
    #[must_use]
    pub fn project(&self, aspect: f32, world: [f32; 3]) -> Option<[f32; 3]> {
        let clip = self.view_projection(aspect).transform_point(world);
        if clip[3] <= 0.0 {
            return None;
        }
        Some([clip[0] / clip[3], clip[1] / clip[3], clip[2] / clip[3]])
    }

    /// Inverse of [`Camera3D::project`] for the ground plane (y = 0): maps a
    /// normalized screen coordinate (x right, y down, origin top-left) to a
    /// world (x, z) ground point. Returns `None` when the ray never hits the
    /// ground plane. Used for mouse picking, so picking stays correct under
    /// camera movement and resize.
    #[must_use]
    pub fn screen_to_ground(&self, aspect: f32, sx: f32, sy: f32) -> Option<[f32; 2]> {
        let (forward, right, up) = self.basis();
        let ndc_x = 2.0 * sx - 1.0;
        let ndc_y = 1.0 - 2.0 * sy;
        let tan_half = (self.fov_y_rad / 2.0).tan();
        let dir = normalize([
            forward[0] + right[0] * ndc_x * tan_half * aspect + up[0] * ndc_y * tan_half,
            forward[1] + right[1] * ndc_x * tan_half * aspect + up[1] * ndc_y * tan_half,
            forward[2] + right[2] * ndc_x * tan_half * aspect + up[2] * ndc_y * tan_half,
        ]);
        if dir[1] >= -1e-6 {
            return None;
        }
        let t = -self.eye[1] / dir[1];
        Some([self.eye[0] + dir[0] * t, self.eye[2] + dir[2] * t])
    }
}

/// Counts readback pixels matching a base color in either lighting variant
/// (top face = factor 1.0, vertical sides = factor [`AMBIENT`]). Robust to
/// which face of a small box dominates the rasterized pixels.
#[must_use]
pub fn count_color_family(pixels: &[u8], base: [f32; 4]) -> usize {
    crate::geometry::count_color(pixels, lit_color(base, AMBIENT + DIFFUSE))
        + crate::geometry::count_color(pixels, lit_color(base, AMBIENT))
}

/// Projects a world point to normalized screen coordinates (x right, y down)
/// plus its NDC z; `None` when the point is behind the camera.
#[must_use]
pub fn world_to_screen(camera: &Camera3D, aspect: f32, world: [f32; 3]) -> Option<(f32, f32, f32)> {
    camera
        .project(aspect, world)
        .map(|ndc| ((ndc[0] + 1.0) / 2.0, (1.0 - ndc[1]) / 2.0, ndc[2]))
}

/// Samples a depth readback in a `(2*radius+1)^2` window around the normalized
/// screen position `(sx, sy)`, returning the min (or max) of the window.
#[must_use]
pub fn sample_depth_window(
    depths: &[f32],
    width: u32,
    height: u32,
    sx: f32,
    sy: f32,
    radius: u32,
    take_min: bool,
) -> Option<f32> {
    let px = (sx * (width.saturating_sub(1)) as f32).round() as i64;
    let py = (sy * (height.saturating_sub(1)) as f32).round() as i64;
    let radius = i64::from(radius);
    let mut best: Option<f32> = None;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let x = px + dx;
            let y = py + dy;
            if x < 0 || y < 0 || x >= i64::from(width) || y >= i64::from(height) {
                continue;
            }
            let depth = depths[y as usize * width as usize + x as usize];
            best = Some(match best {
                None => depth,
                Some(b) => {
                    if take_min {
                        b.min(depth)
                    } else {
                        b.max(depth)
                    }
                }
            });
        }
    }
    best
}

/// Verifies that a depth readback orders near geometry before far geometry:
/// the sampled depth at `near`'s screen position must be strictly smaller
/// (closer) than at `far`'s, matching the CPU-projected NDC z ordering. This
/// is the scale-independent proof that the depth buffer discriminates 3D
/// geometry (absolute depth deltas shrink with camera distance, so a fixed
/// threshold would be flaky across GPUs).
pub fn verify_depth_ordering(
    depths: &[f32],
    width: u32,
    height: u32,
    camera: &Camera3D,
    aspect: f32,
    near: [f32; 3],
    far: [f32; 3],
) -> Result<(), String> {
    let (snx, sny, zn) = world_to_screen(camera, aspect, near).ok_or("near point not visible")?;
    let (sfx, sfy, zf) = world_to_screen(camera, aspect, far).ok_or("far point not visible")?;
    if zn >= zf {
        return Err(format!(
            "probe points are not depth-ordered on CPU: zn={zn} zf={zf}"
        ));
    }
    let d_near = sample_depth_window(depths, width, height, snx, sny, 2, true)
        .ok_or("no depth sample at the near point")?;
    let d_far = sample_depth_window(depths, width, height, sfx, sfy, 2, false)
        .ok_or("no depth sample at the far point")?;
    if d_near >= d_far {
        return Err(format!(
            "depth readback does not order near before far: near={d_near} far={d_far} (cpu zn={zn} < zf={zf})"
        ));
    }
    Ok(())
}

/// Pushes one quad (two triangles) into a scene vertex list.
fn push_quad(
    vertices: &mut Vec<SceneVertex>,
    a: [f32; 3],
    b: [f32; 3],
    c: [f32; 3],
    d: [f32; 3],
    normal: [f32; 3],
    color: [f32; 4],
) {
    for position in [a, b, c, a, c, d] {
        vertices.push(SceneVertex {
            position,
            normal,
            color,
        });
    }
}

/// Pushes an axis-aligned box (5 faces — the bottom rests on the ground and is
/// never visible) with per-face normals.
fn push_box(
    vertices: &mut Vec<SceneVertex>,
    center: [f32; 2],
    half_size: [f32; 2],
    height: f32,
    color: [f32; 4],
) {
    let (cx, cz) = (center[0], center[1]);
    let (hx, hz) = (half_size[0], half_size[1]);
    let (x0, x1, z0, z1) = (cx - hx, cx + hx, cz - hz, cz + hz);
    let (y0, y1) = (0.0, height);
    // Top face.
    push_quad(
        vertices,
        [x0, y1, z0],
        [x1, y1, z0],
        [x1, y1, z1],
        [x0, y1, z1],
        [0.0, 1.0, 0.0],
        color,
    );
    // +x side.
    push_quad(
        vertices,
        [x1, y0, z0],
        [x1, y0, z1],
        [x1, y1, z1],
        [x1, y1, z0],
        [1.0, 0.0, 0.0],
        color,
    );
    // -x side.
    push_quad(
        vertices,
        [x0, y0, z1],
        [x0, y0, z0],
        [x0, y1, z0],
        [x0, y1, z1],
        [-1.0, 0.0, 0.0],
        color,
    );
    // +z side.
    push_quad(
        vertices,
        [x1, y0, z1],
        [x0, y0, z1],
        [x0, y1, z1],
        [x1, y1, z1],
        [0.0, 0.0, 1.0],
        color,
    );
    // -z side.
    push_quad(
        vertices,
        [x0, y0, z0],
        [x1, y0, z0],
        [x1, y1, z0],
        [x0, y1, z0],
        [0.0, 0.0, -1.0],
        color,
    );
}

/// Pushes one unlit quad (two triangles) into a HUD vertex list.
fn push_hud_quad(
    vertices: &mut Vec<HudVertex>,
    a: [f32; 3],
    b: [f32; 3],
    c: [f32; 3],
    d: [f32; 3],
    color: [f32; 4],
) {
    for position in [a, b, c, a, c, d] {
        vertices.push(HudVertex { position, color });
    }
}

/// Pushes a flat rect outline on the ground plane (y = `y`) as four thin quads.
fn push_ground_rect_outline(
    vertices: &mut Vec<HudVertex>,
    center: [f32; 2],
    half_size: [f32; 2],
    thickness: f32,
    y: f32,
    color: [f32; 4],
) {
    let (cx, cz) = (center[0], center[1]);
    let (hx, hz) = (half_size[0], half_size[1]);
    // Top and bottom edges (along x).
    for zc in [cz - hz, cz + hz - thickness] {
        push_hud_quad(
            vertices,
            [cx - hx, y, zc],
            [cx + hx, y, zc],
            [cx + hx, y, zc + thickness],
            [cx - hx, y, zc + thickness],
            color,
        );
    }
    // Left and right edges (along z).
    for xc in [cx - hx, cx + hx - thickness] {
        push_hud_quad(
            vertices,
            [xc, y, cz - hz],
            [xc + thickness, y, cz - hz],
            [xc + thickness, y, cz + hz],
            [xc, y, cz + hz],
            color,
        );
    }
}

/// Builds the 3D scene geometry from a render snapshot: the ground plane, one
/// box per static obstacle, and one team-colored box per live unit. Rebuilt
/// per frame from the immutable snapshot; destroyed entities are absent by
/// construction.
#[must_use]
pub fn build_scene_geometry(
    snapshot: &RenderSnapshot,
    map_width_m: f32,
    map_depth_m: f32,
) -> Vec<SceneVertex> {
    // Ground plane: one quad at y = 0 covering the whole map.
    let mut vertices = Vec::new();
    push_quad(
        &mut vertices,
        [0.0, 0.0, 0.0],
        [map_width_m, 0.0, 0.0],
        [map_width_m, 0.0, map_depth_m],
        [0.0, 0.0, map_depth_m],
        [0.0, 1.0, 0.0],
        GROUND_COLOR,
    );
    for obstacle in &snapshot.obstacles {
        push_box(
            &mut vertices,
            obstacle.center,
            obstacle.half_size,
            OBSTACLE_HEIGHT_M,
            OBSTACLE_COLOR,
        );
    }
    for entity in &snapshot.entities {
        let radius = entity.radius_m.max(0.5);
        push_box(
            &mut vertices,
            entity.position,
            [radius, radius],
            UNIT_HEIGHT_M,
            team_color(entity.team),
        );
    }
    vertices
}

/// Builds the world-space HUD geometry (issue #92 elements in 3D): health bars
/// floating above units, selection rings and order markers on the ground
/// plane, and the drag-box outline. `drag` is the drag box in world
/// coordinates (from [`Camera3D::screen_to_ground`]), if a drag is in progress.
/// Sizes scale with `view_height_m` so they stay readable at any zoom.
#[must_use]
pub fn build_hud_geometry(
    snapshot: &RenderSnapshot,
    view_height_m: f32,
    drag: Option<([f32; 2], [f32; 2])>,
) -> Vec<HudVertex> {
    let mut vertices = Vec::new();
    let bar_height = (view_height_m * 0.015).max(0.3);
    let bar_width = (view_height_m * 0.04).max(1.0);
    let ring_extra = (view_height_m * 0.008).max(0.2);
    let line_thickness = (view_height_m * 0.004).max(0.1);
    let bar_y = UNIT_HEIGHT_M + bar_height * 1.5;

    for entity in &snapshot.entities {
        let [x, z] = entity.position;
        let r = entity.radius_m.max(0.5);

        // Health bar: dark background + proportional foreground, floating
        // horizontally above the unit.
        let frac = if entity.max_health > 0 {
            entity.health as f32 / entity.max_health as f32
        } else {
            0.0
        };
        let health_color = if frac > 0.5 {
            [0.2, 0.8, 0.2, 1.0]
        } else if frac > 0.25 {
            [0.9, 0.7, 0.1, 1.0]
        } else {
            [0.9, 0.2, 0.2, 1.0]
        };
        let bar_h = bar_height / 2.0;
        let bar_w = bar_width / 2.0;
        push_hud_quad(
            &mut vertices,
            [x - bar_w, bar_y, z - bar_h],
            [x + bar_w, bar_y, z - bar_h],
            [x + bar_w, bar_y, z + bar_h],
            [x - bar_w, bar_y, z + bar_h],
            [0.1, 0.1, 0.1, 0.8],
        );
        let fg_width = bar_width * frac;
        if fg_width > 0.0 {
            let fg_w = fg_width / 2.0;
            let fg_cx = x - bar_w + fg_width / 2.0;
            push_hud_quad(
                &mut vertices,
                [fg_cx - fg_w, bar_y, z - bar_h],
                [fg_cx + fg_w, bar_y, z - bar_h],
                [fg_cx + fg_w, bar_y, z + bar_h],
                [fg_cx - fg_w, bar_y, z + bar_h],
                health_color,
            );
        }

        // Selection ring: a ground-plane outline around the unit.
        if entity.selected {
            push_ground_rect_outline(
                &mut vertices,
                [x, z],
                [r + ring_extra, r + ring_extra],
                line_thickness.max(0.15),
                0.05,
                [1.0, 1.0, 1.0, 0.9],
            );
        }

        // Order marker: a ground line from the unit toward its destination.
        if let Some(dest) = entity.move_destination {
            let dx = dest[0] - x;
            let dz = dest[1] - z;
            let len = (dx * dx + dz * dz).sqrt();
            let (nx, nz) = if len > 0.0 {
                (
                    -dz / len * line_thickness * 0.5,
                    dx / len * line_thickness * 0.5,
                )
            } else {
                (line_thickness * 0.5, 0.0)
            };
            push_hud_quad(
                &mut vertices,
                [x - nx, 0.06, z - nz],
                [x + nx, 0.06, z + nz],
                [dest[0] + nx, 0.06, dest[1] + nz],
                [dest[0] - nx, 0.06, dest[1] - nz],
                [1.0, 1.0, 0.4, 0.6],
            );
        }
    }

    // Drag-box outline (issue #90).
    if let Some((start, end)) = drag {
        let min_x = start[0].min(end[0]);
        let max_x = start[0].max(end[0]);
        let min_z = start[1].min(end[1]);
        let max_z = start[1].max(end[1]);
        if (max_x - min_x) > 0.1 && (max_z - min_z) > 0.1 {
            push_ground_rect_outline(
                &mut vertices,
                [(min_x + max_x) / 2.0, (min_z + max_z) / 2.0],
                [(max_x - min_x) / 2.0, (max_z - min_z) / 2.0],
                line_thickness.max(0.15),
                0.08,
                [1.0, 1.0, 1.0, 0.3],
            );
        }
    }
    vertices
}

/// Maps one point of the 2D overlay strip (world x in `[0, map_width_m]`, world
/// z in `[-OVERLAY_STRIP_M, 0]`) to screen-space NDC in the top 20% band.
fn strip_to_ndc(map_width_m: f32, x: f32, z: f32) -> [f32; 3] {
    let ndc_x = 2.0 * x / map_width_m - 1.0;
    let ndc_y = 1.0 + (z / OVERLAY_STRIP_M) * 0.2;
    [ndc_x, ndc_y, 0.0]
}

/// Pushes a rect of the overlay strip (world coords) as an NDC HUD quad.
fn push_strip_rect(
    vertices: &mut Vec<HudVertex>,
    map_width_m: f32,
    center: [f32; 2],
    half_size: [f32; 2],
    color: [f32; 4],
) {
    let (cx, cz) = (center[0], center[1]);
    let (hx, hz) = (half_size[0], half_size[1]);
    let a = strip_to_ndc(map_width_m, cx - hx, cz - hz);
    let b = strip_to_ndc(map_width_m, cx + hx, cz - hz);
    let c = strip_to_ndc(map_width_m, cx + hx, cz + hz);
    let d = strip_to_ndc(map_width_m, cx - hx, cz + hz);
    push_hud_quad(vertices, a, b, c, d, color);
}

/// Builds the perf-overlay strip (issue #88) as screen-space HUD vertices in
/// NDC (x/y in [-1, 1], z = 0), drawn with the identity view-projection as a
/// strip across the top of the screen — so it stays readable in the 3D view.
#[must_use]
pub fn overlay_hud_vertices(stats: &PerfStats, map_width_m: f32) -> Vec<HudVertex> {
    let mut vertices = Vec::new();
    push_strip_rect(
        &mut vertices,
        map_width_m,
        [map_width_m / 2.0, -OVERLAY_STRIP_M / 2.0],
        [map_width_m / 2.0, OVERLAY_STRIP_M / 2.0],
        OVERLAY_BG_COLOR,
    );
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
        push_strip_rect(
            &mut vertices,
            map_width_m,
            [center_x, -height / 2.0],
            [half_w, height / 2.0],
            *color,
        );
    }
    vertices
}

/// The scene-pass + present-pass pipelines (issues #104/#97), sharing one WGSL
/// module:
/// - `scene_pipeline`: targets `[scene_color, motion]` + depth, writes depth
///   (`Less`) and motion vectors;
/// - `hud_pipeline`: targets `[scene_color, motion]` + depth-always, no depth
///   writes, zero motion — world HUD overlaid on the 3D scene;
/// - `present_pipeline`: targets `[output]`, no depth — the bilinear-fallback
///   fullscreen upscale from render to output resolution;
/// - `hud_output_pipeline`: targets `[output]`, no depth, identity VP — the
///   screen-space HUD strip composited at output resolution.
pub struct ScenePipelines {
    scene_pipeline: wgpu::RenderPipeline,
    hud_pipeline: wgpu::RenderPipeline,
    present_pipeline: wgpu::RenderPipeline,
    hud_output_pipeline: wgpu::RenderPipeline,
    present_layout: wgpu::PipelineLayout,
    present_bind_group_layout: wgpu::BindGroupLayout,
    vp_bind: wgpu::BindGroup,
    identity_bind: wgpu::BindGroup,
    vp_uniform: wgpu::Buffer,
    scene_buffer: wgpu::Buffer,
    scene_capacity: usize,
    hud_buffer: wgpu::Buffer,
    hud_capacity: usize,
    hud_output_buffer: wgpu::Buffer,
    hud_output_capacity: usize,
}

impl ScenePipelines {
    /// Creates the pipelines: scene/HUD target `[scene_format, motion_format]`
    /// with [`DEPTH_FORMAT`]; present/HUD-output target `output_format`.
    #[must_use]
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        scene_format: wgpu::TextureFormat,
        motion_format: wgpu::TextureFormat,
        output_format: wgpu::TextureFormat,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("phalanx-scene3d-shader"),
            source: wgpu::ShaderSource::Wgsl(SCENE_SHADER_WGSL.into()),
        });
        let uniform_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("phalanx-scene3d-uniforms"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: std::num::NonZeroU64::new(144),
                    },
                    count: None,
                }],
            });
        let present_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("phalanx-present-texture"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
        let uniform_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("phalanx-scene3d-layout"),
            bind_group_layouts: &[Some(&uniform_bind_group_layout)],
            immediate_size: 0,
        });
        let present_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("phalanx-present-layout"),
            bind_group_layouts: &[Some(&present_bind_group_layout)],
            immediate_size: 0,
        });
        let color_target = |format: wgpu::TextureFormat| {
            Some(wgpu::ColorTargetState {
                format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })
        };
        let make_pipeline = |vertex_entry: &'static str,
                             fragment_entry: &'static str,
                             vertex_buffer: Option<wgpu::VertexBufferLayout<'static>>,
                             targets: Vec<Option<wgpu::ColorTargetState>>,
                             depth: Option<(bool, wgpu::CompareFunction)>,
                             layout: &wgpu::PipelineLayout| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("phalanx-scene3d-pipeline"),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(vertex_entry),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[vertex_buffer],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fragment_entry),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &targets,
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: depth.map(|(write, compare)| wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(write),
                    depth_compare: Some(compare),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let scene_pipeline = make_pipeline(
            "vs_scene",
            "fs_lit",
            Some(SceneVertex::layout()),
            vec![color_target(scene_format), color_target(motion_format)],
            Some((true, wgpu::CompareFunction::Less)),
            &uniform_layout,
        );
        let hud_pipeline = make_pipeline(
            "vs_hud",
            "fs_flat",
            Some(HudVertex::layout()),
            vec![color_target(scene_format), color_target(motion_format)],
            Some((false, wgpu::CompareFunction::Always)),
            &uniform_layout,
        );
        let present_pipeline = make_pipeline(
            "vs_present",
            "fs_present",
            None,
            vec![color_target(output_format)],
            None,
            &present_layout,
        );
        let hud_output_pipeline = make_pipeline(
            "vs_hud",
            "fs_flat",
            Some(HudVertex::layout()),
            vec![color_target(output_format)],
            None,
            &uniform_layout,
        );
        let vp_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("phalanx-scene3d-uniforms"),
            size: 144,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let identity_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("phalanx-scene3d-identity"),
            size: 144,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&identity_uniform, 0, &FrameUniforms::identity().to_bytes());
        let vp_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("phalanx-scene3d-vp-bind"),
            layout: &uniform_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: vp_uniform.as_entire_binding(),
            }],
        });
        let identity_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("phalanx-scene3d-identity-bind"),
            layout: &uniform_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: identity_uniform.as_entire_binding(),
            }],
        });
        let make_vertex_buffer = |label: &str| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: 4096,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        Self {
            scene_pipeline,
            hud_pipeline,
            present_pipeline,
            hud_output_pipeline,
            present_layout,
            present_bind_group_layout,
            vp_bind,
            identity_bind,
            vp_uniform,
            scene_buffer: make_vertex_buffer("phalanx-scene3d-scene-vertices"),
            scene_capacity: 4096 / std::mem::size_of::<SceneVertex>(),
            hud_buffer: make_vertex_buffer("phalanx-scene3d-hud-vertices"),
            hud_capacity: 4096 / std::mem::size_of::<HudVertex>(),
            hud_output_buffer: make_vertex_buffer("phalanx-scene3d-hud-output-vertices"),
            hud_output_capacity: 4096 / std::mem::size_of::<HudVertex>(),
        }
    }

    /// The present-pass pipeline layout (scene texture + bilinear sampler in
    /// group 1), for building the per-resize present bind group.
    #[must_use]
    pub fn present_layout(&self) -> &wgpu::PipelineLayout {
        &self.present_layout
    }

    /// The present-pass bind group layout (scene texture + bilinear sampler in
    /// group 1), for building the per-resize present bind group.
    #[must_use]
    pub fn present_bind_group_layout(&self) -> &wgpu::BindGroupLayout {
        &self.present_bind_group_layout
    }

    fn grow(
        device: &wgpu::Device,
        buffer: &mut wgpu::Buffer,
        capacity: &mut usize,
        needed: usize,
        vertex_size: usize,
        label: &str,
    ) {
        if needed > *capacity {
            *capacity = needed.next_power_of_two();
            *buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: (*capacity * vertex_size) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
    }

    /// Uploads the frame uniforms and the scene-pass vertex batches; returns
    /// the vertex counts `(scene, world_hud)`.
    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        uniforms: &FrameUniforms,
        scene: &[SceneVertex],
        world_hud: &[HudVertex],
    ) -> (u32, u32) {
        queue.write_buffer(&self.vp_uniform, 0, &uniforms.to_bytes());
        Self::grow(
            device,
            &mut self.scene_buffer,
            &mut self.scene_capacity,
            scene.len(),
            std::mem::size_of::<SceneVertex>(),
            "phalanx-scene3d-scene-vertices",
        );
        queue.write_buffer(&self.scene_buffer, 0, &SceneVertex::to_bytes(scene));
        Self::grow(
            device,
            &mut self.hud_buffer,
            &mut self.hud_capacity,
            world_hud.len(),
            std::mem::size_of::<HudVertex>(),
            "phalanx-scene3d-hud-vertices",
        );
        queue.write_buffer(&self.hud_buffer, 0, &HudVertex::to_bytes(world_hud));
        (scene.len() as u32, world_hud.len() as u32)
    }

    /// Uploads the present-pass screen-space HUD strip; returns its vertex count.
    pub fn upload_output(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen_hud: &[HudVertex],
    ) -> u32 {
        Self::grow(
            device,
            &mut self.hud_output_buffer,
            &mut self.hud_output_capacity,
            screen_hud.len(),
            std::mem::size_of::<HudVertex>(),
            "phalanx-scene3d-hud-output-vertices",
        );
        queue.write_buffer(&self.hud_output_buffer, 0, &HudVertex::to_bytes(screen_hud));
        screen_hud.len() as u32
    }

    /// Draws the scene pass: scene (depth-tested, motion) then world HUD
    /// (depth-always, zero motion) into an open pass with scene-color, motion,
    /// and depth attachments.
    pub fn draw_scene<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>, counts: (u32, u32)) {
        let (scene_n, world_hud_n) = counts;
        if scene_n > 0 {
            pass.set_pipeline(&self.scene_pipeline);
            pass.set_bind_group(0, &self.vp_bind, &[]);
            pass.set_vertex_buffer(0, self.scene_buffer.slice(..));
            pass.draw(0..scene_n, 0..1);
        }
        if world_hud_n > 0 {
            pass.set_pipeline(&self.hud_pipeline);
            pass.set_bind_group(0, &self.vp_bind, &[]);
            pass.set_vertex_buffer(0, self.hud_buffer.slice(..));
            pass.draw(0..world_hud_n, 0..1);
        }
    }

    /// Draws the present pass into an open pass targeting the output view: the
    /// fullscreen upscale of the scene color, then the screen-space HUD strip
    /// (identity VP) on top at output resolution.
    pub fn draw_output<'a>(
        &'a self,
        pass: &mut wgpu::RenderPass<'a>,
        present_bind: &wgpu::BindGroup,
        screen_hud_n: u32,
    ) {
        pass.set_pipeline(&self.present_pipeline);
        pass.set_bind_group(1, present_bind, &[]);
        pass.draw(0..3, 0..1);
        if screen_hud_n > 0 {
            pass.set_pipeline(&self.hud_output_pipeline);
            pass.set_bind_group(0, &self.identity_bind, &[]);
            pass.set_vertex_buffer(0, self.hud_output_buffer.slice(..));
            pass.draw(0..screen_hud_n, 0..1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{EntityCategory, ObstacleShape, RenderEntity};

    fn test_snapshot() -> RenderSnapshot {
        RenderSnapshot {
            tick: 3,
            entities: vec![
                RenderEntity {
                    entity_index: 0,
                    position: [20.0, 100.0],
                    team: 1,
                    category: EntityCategory::Unit,
                    radius_m: 0.5,
                    health: 100,
                    max_health: 100,
                    selected: true,
                    move_destination: Some([100.0, 100.0]),
                },
                RenderEntity {
                    entity_index: 6,
                    position: [180.0, 100.0],
                    team: 2,
                    category: EntityCategory::Unit,
                    radius_m: 0.5,
                    health: 15,
                    max_health: 60,
                    selected: false,
                    move_destination: None,
                },
            ],
            obstacles: vec![ObstacleShape {
                center: [100.0, 60.0],
                half_size: [45.0, 2.0],
            }],
        }
    }

    #[test]
    fn camera_projects_target_to_screen_center() {
        let camera = Camera3D::from_view([100.0, 100.0], 200.0, 0.0);
        let ndc = camera.project(1.5, camera.target).expect("in front");
        assert!(ndc[0].abs() < 1e-5 && ndc[1].abs() < 1e-5, "ndc={ndc:?}");
        assert!(ndc[2] > 0.0 && ndc[2] < 1.0);
    }

    #[test]
    fn screen_to_ground_is_the_inverse_of_project_on_the_ground_plane() {
        for (aspect, yaw) in [(1.0, 0.0), (1.7, 0.0), (0.8, 0.7), (1.3, -1.1)] {
            let camera = Camera3D::from_view([100.0, 100.0], 120.0, yaw);
            for world in [
                [100.0, 0.0, 100.0],
                [40.0, 0.0, 150.0],
                [170.0, 0.0, 30.0],
                [100.0, 0.0, 199.0],
            ] {
                let ndc = camera.project(aspect, world).expect("in front");
                let sx = (ndc[0] + 1.0) / 2.0;
                let sy = (1.0 - ndc[1]) / 2.0;
                let ground = camera
                    .screen_to_ground(aspect, sx, sy)
                    .expect("ray hits ground");
                assert!(
                    (ground[0] - world[0]).abs() < 1e-2 && (ground[1] - world[2]).abs() < 1e-2,
                    "aspect={aspect} yaw={yaw} world={world:?} ground={ground:?}"
                );
            }
        }
    }

    #[test]
    fn screen_center_maps_to_view_center() {
        let camera = Camera3D::from_view([80.0, 60.0], 200.0, 0.4);
        let ground = camera.screen_to_ground(1.0, 0.5, 0.5).expect("hits");
        assert!((ground[0] - 80.0).abs() < 1e-3 && (ground[1] - 60.0).abs() < 1e-3);
    }

    #[test]
    fn scene_geometry_has_ground_obstacle_and_unit_boxes() {
        let snapshot = test_snapshot();
        let vertices = build_scene_geometry(&snapshot, 200.0, 200.0);
        // Ground: 6 verts. Obstacle box: 5 faces * 6 = 30. Two unit boxes: 60.
        assert_eq!(vertices.len(), 6 + 30 + 60);
        let ground = &vertices[..6];
        assert!(ground.iter().all(|v| v.position[1] == 0.0
            && v.normal == [0.0, 1.0, 0.0]
            && v.color == GROUND_COLOR));
        let obstacle = &vertices[6..36];
        assert!(
            obstacle
                .iter()
                .any(|v| v.normal == [0.0, 1.0, 0.0] && v.color == OBSTACLE_COLOR)
        );
        assert!(
            obstacle
                .iter()
                .any(|v| v.normal == [1.0, 0.0, 0.0] && v.position[1] == 0.0)
        );
        let units = &vertices[36..];
        assert!(units.iter().any(|v| v.color == team_color(1)));
        assert!(units.iter().any(|v| v.color == team_color(2)));
        assert!(
            units
                .iter()
                .any(|v| v.normal == [0.0, 1.0, 0.0] && v.position[1] == UNIT_HEIGHT_M)
        );
    }

    #[test]
    fn hud_geometry_has_bars_ring_marker_and_drag_box() {
        let snapshot = test_snapshot();
        let hud = build_hud_geometry(&snapshot, 200.0, Some(([10.0, 10.0], [50.0, 40.0])));
        // Per unit: health bg + fg = 2 quads = 12 verts (x2 units = 24).
        // Selected unit: ring (4 quads = 24) + order marker (6). Drag box: 24.
        let expected = 24 + 24 + 6 + 24;
        assert_eq!(hud.len(), expected, "hud vertex count");
        assert!(
            hud.iter().any(|v| v.color == [0.9, 0.2, 0.2, 1.0]),
            "red bar"
        );
        assert!(
            hud.iter().any(|v| v.color == [0.2, 0.8, 0.2, 1.0]),
            "green bar"
        );
        assert!(hud.iter().any(|v| v.color == [1.0, 1.0, 1.0, 0.9]), "ring");
        assert!(
            hud.iter().any(|v| v.color == [1.0, 1.0, 0.4, 0.6]),
            "order marker"
        );
    }

    #[test]
    fn overlay_strip_is_screen_space_ndc_at_the_top() {
        use crate::perf::{DEFAULT_PERF_WINDOW, FrameProfiler, FrameTimings};
        use std::time::Duration;
        let mut profiler = FrameProfiler::new(DEFAULT_PERF_WINDOW);
        profiler.record(FrameTimings {
            total: Duration::from_millis(16),
            simulation: Duration::from_millis(2),
            snapshot: Duration::from_millis(1),
            graphics_cpu: Duration::from_millis(10),
            simulation_steps: 1,
        });
        let hud = overlay_hud_vertices(&profiler.stats(), 200.0);
        // Background strip + 4 bars = 5 quads = 30 vertices.
        assert_eq!(hud.len(), 30);
        assert!(
            hud.iter().all(|v| {
                (-1.0 - 1e-6..=1.0 + 1e-6).contains(&v.position[0])
                    && (0.8 - 1e-6..=1.0 + 1e-6).contains(&v.position[1])
                    && v.position[2] == 0.0
            }),
            "strip must stay in the top NDC band"
        );
        assert!(hud.iter().any(|v| v.color == OVERLAY_BG_COLOR));
        assert!(hud.iter().any(|v| v.color == TOTAL_COLOR));
    }

    #[test]
    fn lit_color_matches_the_shader_formula() {
        // Top face: factor AMBIENT + DIFFUSE = 1.0 -> base color unchanged.
        assert_eq!(
            lit_color([0.3, 0.45, 0.95, 1.0], AMBIENT + DIFFUSE),
            [0.3, 0.45, 0.95, 1.0]
        );
        let side = lit_color([1.0, 1.0, 1.0, 1.0], AMBIENT);
        assert!((side[0] - 0.55).abs() < 1e-7);
    }

    #[test]
    fn uniform_struct_wgsl_layout_matches_rust_encoding() {
        // The WGSL Uniforms struct must be exactly the size of
        // FrameUniforms::to_bytes (144 bytes) and of the bind group layout's
        // min_binding_size. WGSL aligns vec3<f32> to 16 bytes, so a trailing
        // `f32 + vec3<f32>` pad would inflate the struct to 160 bytes and fail
        // pipeline validation on real adapters (observed on the Windows DX12
        // CI adapter). Compute the layout with naga so this check also runs on
        // GPU-less machines.
        let module = wgpu::naga::front::wgsl::parse_str(SCENE_SHADER_WGSL).expect("wgsl parses");
        let gctx = wgpu::naga::proc::GlobalCtx {
            types: &module.types,
            constants: &module.constants,
            overrides: &module.overrides,
            global_expressions: &module.global_expressions,
        };
        let mut layouter = wgpu::naga::proc::Layouter::default();
        layouter.update(gctx).expect("layout computes");
        let uniforms = module
            .types
            .iter()
            .find(|(_, ty)| ty.name.as_deref() == Some("Uniforms"))
            .expect("Uniforms struct present");
        let expected = FrameUniforms::identity().to_bytes().len() as u32;
        assert_eq!(
            layouter[uniforms.0].size, expected,
            "WGSL Uniforms size must match FrameUniforms::to_bytes and min_binding_size"
        );
    }

    #[test]
    fn verify_depth_ordering_detects_unordered_and_ordered_readbacks() {
        let camera = Camera3D::from_view([100.0, 100.0], 200.0, 0.0);
        let near = [20.0, UNIT_HEIGHT_M, 100.0];
        let far = [20.0, 0.0, 90.0];
        let (snx, sny, zn) = world_to_screen(&camera, 1.0, near).expect("near visible");
        let (sfx, sfy, zf) = world_to_screen(&camera, 1.0, far).expect("far visible");
        assert!(zn < zf, "probe points must be depth-ordered on CPU");
        let (w, h) = (64_u32, 64_u32);
        // A flat buffer (nothing rendered closer) fails the ordering check.
        let flat = vec![1.0_f32; (w * h) as usize];
        assert!(verify_depth_ordering(&flat, w, h, &camera, 1.0, near, far).is_err());
        // A buffer with a nearer surface at the near point passes.
        let mut depths = vec![zf; (w * h) as usize];
        let px = (snx * (w - 1) as f32).round() as usize;
        let py = (sny * (h - 1) as f32).round() as usize;
        depths[py * w as usize + px] = zn - 0.001;
        verify_depth_ordering(&depths, w, h, &camera, 1.0, near, far)
            .expect("ordered readback passes");
        // Swapped probe points are rejected by the CPU ordering pre-check.
        assert!(verify_depth_ordering(&depths, w, h, &camera, 1.0, far, near).is_err());
        let _ = (sfx, sfy);
    }

    #[test]
    fn offscreen_temporal_frame_renders_scene_depth_and_motion_when_adapter_exists() {
        match crate::init_headless() {
            Err(crate::BackendError::NoAdapter(_)) => {}
            Err(other) => panic!("unexpected backend error: {other}"),
            Ok((backend, report)) => {
                println!("{report}");
                use crate::temporal::TemporalRenderConfig;
                // Render at half resolution into a 512x512 output (render/output
                // separation, issue #97).
                let config = TemporalRenderConfig::new(0.5);
                let mut renderer = crate::FrameRenderer::new(
                    &backend.device,
                    &backend.queue,
                    crate::OffscreenTarget::FORMAT,
                    512,
                    512,
                    config,
                );
                assert_eq!(renderer.scene_size(), (256, 256), "render resolution");
                let output =
                    crate::OffscreenTarget::new(&backend.device, 512, 512).expect("output");
                let camera = Camera3D::from_view([100.0, 100.0], 200.0, 0.0);
                let vp = camera.view_projection(1.0);
                let jitter = [0.05, -0.03];

                // Frame 1: unit at (20, 100).
                let snapshot_a = test_snapshot();
                let scene_a = build_scene_geometry(&snapshot_a, 200.0, 200.0);
                let hud_a = build_hud_geometry(&snapshot_a, 200.0, None);
                renderer.render_battle_temporal(
                    &backend.device,
                    &backend.queue,
                    &output.view,
                    crate::DEFAULT_CLEAR.to_wgpu(),
                    &crate::BattleTemporalFrame {
                        vp: &vp,
                        scene: &scene_a,
                        world_hud: &hud_a,
                        screen_hud: &[],
                        jitter,
                        exposure: 1.0,
                    },
                );
                assert_eq!(renderer.last_jitter(), jitter, "exact offset reported");
                // Frame 2: the team-1 unit moved 5 m — motion vectors must be
                // non-zero at the unit, ~zero for static ground (same camera).
                let mut snapshot_b = snapshot_a.clone();
                snapshot_b.entities[0].position = [25.0, 100.0];
                let scene_b = build_scene_geometry(&snapshot_b, 200.0, 200.0);
                renderer.render_battle_temporal(
                    &backend.device,
                    &backend.queue,
                    &output.view,
                    crate::DEFAULT_CLEAR.to_wgpu(),
                    &crate::BattleTemporalFrame {
                        vp: &vp,
                        scene: &scene_b,
                        world_hud: &[],
                        screen_hud: &[],
                        jitter,
                        exposure: 1.0,
                    },
                );
                let pixels = output
                    .read_back(&backend.device, &backend.queue)
                    .expect("output readback");
                let motion = renderer
                    .read_back_motion(&backend.device, &backend.queue)
                    .expect("motion readback");
                assert_eq!(motion.len(), 256 * 256);
                // The moved unit's screen area has non-zero motion.
                let (sx, sy, _) =
                    world_to_screen(&camera, 1.0, [25.0, UNIT_HEIGHT_M, 100.0]).expect("visible");
                let px = (sx * 255.0).round() as usize;
                let py = (sy * 255.0).round() as usize;
                let unit_motion = motion[py * 256 + px];
                assert!(
                    unit_motion[0].abs() > 1e-4 || unit_motion[1].abs() > 1e-4,
                    "moved unit must have non-zero motion: {unit_motion:?}"
                );
                // A static far corner of the map has ~zero motion (same camera).
                let corner_motion = motion[5 * 256 + 5];
                assert!(
                    corner_motion[0].abs() < 1e-3 && corner_motion[1].abs() < 1e-3,
                    "static ground with static camera has ~zero motion: {corner_motion:?}"
                );
                let moving = motion
                    .iter()
                    .filter(|m| m[0].abs() > 1e-4 || m[1].abs() > 1e-4)
                    .count();
                assert!(moving > 0, "some pixels carry motion");
                // Output frame (bilinear upscale of the 256px scene) still shows
                // the team colors.
                assert!(count_color_family(&pixels, team_color(1)) > 0);
                assert!(count_color_family(&pixels, team_color(2)) > 0);
                // Depth readback at render resolution orders near before far.
                let depths = renderer
                    .read_back_depth(&backend.device, &backend.queue)
                    .expect("depth readback");
                verify_depth_ordering(
                    &depths,
                    256,
                    256,
                    &camera,
                    1.0,
                    [25.0, UNIT_HEIGHT_M, 100.0],
                    [25.0, 0.0, 90.0],
                )
                .expect("depth ordering");
                assert!(renderer.history().is_valid());
                assert_eq!(renderer.history().frames_since_reset(), 2);
            }
        }
    }
}
