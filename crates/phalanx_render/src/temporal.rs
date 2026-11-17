//! Vendor-neutral temporal-rendering contract (issue #97, ADR-0010 §2).
//!
//! Modern temporal upscalers (AMD FSR 4, DLSS, XeSS, …) need more than a
//! final image: linear scene color at a *render* resolution distinct from the
//! output resolution, a depth buffer with a declared convention, motion
//! vectors with a documented space/sign/scale convention, the exact camera
//! jitter offset used for the frame, optional exposure, and a temporal history
//! with explicit reset/invalidation on discontinuities. This module defines
//! that contract as data + pure CPU state so it is unit-testable without a GPU
//! and usable by FSR and non-FSR fallbacks alike.
//!
//! Conventions fixed here (change only via ADR-0010 update):
//! - **Scene color:** linear-light values in the Rec.709/sRGB primaries
//!   working space, stored in [`SCENE_COLOR_FORMAT`] at render resolution. The
//!   M1 scene target is 8-bit unorm (production HDR scene color is #98); the
//!   present pass writes to the output view, whose format encoding (sRGB on
//!   windowed surfaces) is applied by the hardware on write.
//! - **Depth:** wgpu clip convention — NDC z in `[0, 1]`, 0 at the near plane,
//!   1 at the far plane, less-is-closer, NOT inverted (fixed by #104).
//! - **Motion vectors:** [`MOTION_FORMAT`], per-fragment UV-space delta
//!   `uv_current - uv_previous` in fractions of the full target extent per
//!   frame, +x right, +y down (texture v), including the camera-jitter delta
//!   (both frames use jittered projections, following the SDK jitter
//!   convention). Opaque scene and HUD geometry contribute; HUD writes zero
//!   motion in the scene pass and is composited at output resolution instead.
//! - **Jitter:** sub-pixel camera offset applied to the projection as an NDC
//!   translation, drawn from a Halton(2,3) sequence ([`JitterSequence`]); the
//!   exact offset applied to frame *n* is reported via
//!   [`FrameRenderer::last_jitter`](crate::FrameRenderer::last_jitter).
//! - **History:** [`TemporalHistory`] tracks validity; any discontinuity
//!   (camera cut, resolution change, map load, skipped frame) resets it —
//!   stale history must never produce ghosting.
//! - **Upscaler selection:** capability-based ([`UpscalerContract`]); the
//!   built-in fallback is a bilinear upscale (the present pass). FSR/native
//!   effects are queried only on machines with the SDK + supported hardware
//!   (#96) and are never a startup dependency.

use crate::math::Mat4;

/// The scene-color attachment format at render resolution: linear-light
/// Rec.709/sRGB-primaries working space, 8-bit unorm for the M1 slice
/// (production HDR scene color is issue #98).
pub const SCENE_COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// The motion-vector attachment format: 16-bit float RGBA (signed UV deltas).
pub const MOTION_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// The documented motion-vector convention (see module docs).
pub const MOTION_CONVENTION: &str = "motion = uv_current - uv_previous, in fractions of the full target extent per frame; +x right, +y down (texture v); current = current jittered VP * current position, previous = previous jittered VP * previous-frame position, so camera motion, object motion, and the camera-jitter delta are all included";

/// The documented depth convention (fixed by issue #104).
pub const DEPTH_CONVENTION: &str =
    "wgpu clip convention: NDC z in [0,1], 0 near / 1 far, less-is-closer, non-inverted";

/// The documented scene-color working space.
pub const SCENE_COLOR_SPACE: &str = "linear-light Rec.709/sRGB primaries, no transfer encoding in the scene target; output encoding applied by the output format on present";

/// Render/output resolution separation: `render_scale` is the ratio of render
/// resolution to output resolution (1.0 = render at output resolution).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TemporalRenderConfig {
    /// Render-scale factor, clamped to `[0.25, 1.0]`.
    pub render_scale: f32,
}

/// The minimum supported render scale.
pub const MIN_RENDER_SCALE: f32 = 0.25;
/// The default (and maximum) render scale: render at output resolution.
pub const DEFAULT_RENDER_SCALE: f32 = 1.0;

impl TemporalRenderConfig {
    /// Creates a config, clamping `render_scale` to the supported range.
    #[must_use]
    pub fn new(render_scale: f32) -> Self {
        Self {
            render_scale: render_scale.clamp(MIN_RENDER_SCALE, DEFAULT_RENDER_SCALE),
        }
    }

    /// The render-target size for an output size `(width, height)`.
    #[must_use]
    pub fn render_size(&self, width: u32, height: u32) -> (u32, u32) {
        let w = ((width as f32 * self.render_scale).round() as u32).max(1);
        let h = ((height as f32 * self.render_scale).round() as u32).max(1);
        (w, h)
    }
}

/// The Halton low-discrepancy sequence value `index` (1-based) in base `base`,
/// mapped to `[-1, 1]`.
#[must_use]
pub fn halton(mut index: u64, base: u64) -> f32 {
    index = index.max(1);
    let mut value = 0.0_f32;
    let mut fraction = 1.0_f32 / base as f32;
    let mut i = index;
    while i > 0 {
        value += fraction * (i % base) as f32;
        i /= base;
        fraction /= base as f32;
    }
    // Map [0, 1) to [-1, 1).
    value * 2.0 - 1.0
}

/// A deterministic sub-pixel jitter sequence (Halton(2,3)) for camera
/// jittering, in NDC units. The exact offset returned for a frame is what the
/// renderer applies to the projection and reports to the temporal effect.
#[derive(Clone, Debug)]
pub struct JitterSequence {
    frame: u64,
}

impl JitterSequence {
    /// A new sequence (frame counter starts at 0).
    #[must_use]
    pub fn new() -> Self {
        Self { frame: 0 }
    }

    /// The number of jitter offsets produced so far.
    #[must_use]
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Returns the jitter offset `[x, y]` in NDC units for the current frame
    /// at render resolution `width` x `height`, and advances. The offset is at
    /// most half a render pixel per axis (sub-pixel by construction).
    #[must_use]
    pub fn next(&mut self, width: u32, height: u32) -> [f32; 2] {
        self.frame += 1;
        // Halton(2,3), 1-based; mapped to [-1,1] in each axis, then scaled so
        // one NDC unit = one render pixel is 2/width: half a pixel = 1/width.
        let x = halton(self.frame, 2);
        let y = halton(self.frame, 3);
        [x / width.max(1) as f32, y / height.max(1) as f32]
    }
}

impl Default for JitterSequence {
    fn default() -> Self {
        Self::new()
    }
}

/// A discontinuity that invalidates temporal history (ghosting guard).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Discontinuity {
    /// The camera jumped (cut, or moved/zoomed/rotated this frame).
    CameraCut,
    /// The render or output resolution changed.
    ResolutionChange,
    /// A new map/scenario was loaded.
    MapLoad,
    /// Frames were skipped (the effect must not interpolate across the gap).
    FrameSkip,
}

/// The temporal-history lifecycle state: valid until a discontinuity resets
/// it; a fresh history warms up (no accumulation) for the first frames.
#[derive(Clone, Debug, PartialEq)]
pub struct TemporalHistory {
    valid: bool,
    frames_since_reset: u32,
    reset_count: u64,
    last_reset: Option<Discontinuity>,
}

impl TemporalHistory {
    /// A fresh history starts invalid (no accumulated history yet).
    #[must_use]
    pub fn new() -> Self {
        Self {
            valid: false,
            frames_since_reset: 0,
            reset_count: 0,
            last_reset: None,
        }
    }

    /// Whether accumulated history may be reused this frame.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.valid
    }

    /// Frames rendered since the last reset (0 = warm-up frame).
    #[must_use]
    pub fn frames_since_reset(&self) -> u32 {
        self.frames_since_reset
    }

    /// How many times history was reset.
    #[must_use]
    pub fn reset_count(&self) -> u64 {
        self.reset_count
    }

    /// The discontinuity that caused the last reset, if any.
    #[must_use]
    pub fn last_reset(&self) -> Option<Discontinuity> {
        self.last_reset
    }

    /// Records a discontinuity: history becomes invalid (the effect must
    /// reset its accumulation; stale history must not be used).
    pub fn note_discontinuity(&mut self, reason: Discontinuity) {
        self.valid = false;
        self.frames_since_reset = 0;
        self.reset_count += 1;
        self.last_reset = Some(reason);
    }

    /// Advances one rendered frame: the first frame after a reset warms up
    /// (history becomes valid but is marked as fresh), later frames reuse it.
    pub fn advance_frame(&mut self) {
        self.frames_since_reset = self.frames_since_reset.saturating_add(1);
        self.valid = true;
    }
}

impl Default for TemporalHistory {
    fn default() -> Self {
        Self::new()
    }
}

/// A capability-based, vendor-neutral description of what an upscaler/effect
/// requires — queried at runtime instead of assuming one fixed resource set
/// (FSR 4 dropped the reactive/transparency masks, but the actual effect
/// version's requirements must be queried, not assumed).
#[derive(Clone, Debug, PartialEq)]
pub struct UpscalerContract {
    /// Effect name (e.g. "fsr-upscaling-4.1.2", "bilinear-fallback").
    pub name: &'static str,
    /// Required inputs, in the contract's vocabulary.
    pub required_inputs: &'static [&'static str],
    /// Optional inputs.
    pub optional_inputs: &'static [&'static str],
    /// Whether the effect renders at a resolution distinct from output.
    pub separates_render_resolution: bool,
    /// Whether the effect needs the camera jitter offset per frame.
    pub needs_jitter: bool,
    /// Whether the effect consumes temporal history (and thus resets).
    pub uses_history: bool,
}

/// The required-input vocabulary shared by the contracts.
pub const INPUT_SCENE_COLOR: &str = "scene_color_linear@render_res";
/// Depth with the documented convention.
pub const INPUT_DEPTH: &str = "depth[0..1 non-inverted]";
/// Motion vectors with the documented convention.
pub const INPUT_MOTION: &str = "motion_vectors[uv delta, jitter-consistent]";
/// The exact per-frame jitter offset.
pub const INPUT_JITTER: &str = "jitter_offset[ndc]";
/// Optional exposure (or the effect's automatic-exposure path).
pub const INPUT_EXPOSURE: &str = "exposure";
/// Temporal history with reset/invalidation.
pub const INPUT_HISTORY: &str = "temporal_history[reset on discontinuity]";

/// The built-in non-FSR fallback: a bilinear upscale from render resolution to
/// output resolution (the present pass). Always available, no SDK, no GPU
/// features beyond texture sampling.
#[must_use]
pub fn fallback_contract() -> UpscalerContract {
    UpscalerContract {
        name: "bilinear-fallback",
        required_inputs: &[INPUT_SCENE_COLOR],
        optional_inputs: &[
            INPUT_DEPTH,
            INPUT_MOTION,
            INPUT_JITTER,
            INPUT_EXPOSURE,
            INPUT_HISTORY,
        ],
        separates_render_resolution: true,
        needs_jitter: false,
        uses_history: false,
    }
}

/// The temporal-upscaling contract an FSR 4-class effect must satisfy (used
/// by the #96 native-integration spike; not available headless).
#[must_use]
pub fn fsr4_contract() -> UpscalerContract {
    UpscalerContract {
        name: "fsr-upscaling-4.x",
        required_inputs: &[
            INPUT_SCENE_COLOR,
            INPUT_DEPTH,
            INPUT_MOTION,
            INPUT_JITTER,
            INPUT_HISTORY,
        ],
        optional_inputs: &[INPUT_EXPOSURE],
        separates_render_resolution: true,
        needs_jitter: true,
        uses_history: true,
    }
}

/// The upscalers available on this machine, capability-described. Headless or
/// without the native SDK, only the fallback is available; FSR/native effects
/// are listed only where the SDK + supported hardware exist (#96). Selection
/// is capability-based, never a hard startup dependency.
#[must_use]
pub fn available_upscalers() -> Vec<UpscalerContract> {
    // The native SDK path is hardware/driver/SDK-gated and not linked here
    // (ADR-0010 §6: native interop stays behind a small audited boundary);
    // headless and fallback machines get the bilinear path.
    vec![fallback_contract()]
}

/// The NDC translation matrix for a jitter offset (applied as
/// `T(jitter) * vp`, i.e. a clip-space offset that becomes an NDC offset
/// after the perspective divide).
#[must_use]
pub fn jitter_translation(jitter: [f32; 2]) -> Mat4 {
    Mat4 {
        columns: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [jitter[0], jitter[1], 0.0, 1.0],
        ],
    }
}

/// Converts an IEEE-754 half-float bit pattern to `f32` (for reading back the
/// `Rgba16Float` motion attachment). Pure, no libm.
#[must_use]
pub fn half_to_f32(bits: u16) -> f32 {
    let sign = (bits >> 15) & 1;
    let exponent = ((bits >> 10) & 0x1f) as i32;
    let mantissa = (bits & 0x3ff) as f32;
    let value = if exponent == 0 {
        // Subnormal: mantissa * 2^-24.
        mantissa * 2.0_f32.powi(-24)
    } else if exponent == 31 {
        if mantissa == 0.0 {
            f32::INFINITY
        } else {
            f32::NAN
        }
    } else {
        (1.0 + mantissa / 1024.0) * 2.0_f32.powi(exponent - 15)
    };
    if sign == 1 { -value } else { value }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_scale_clamps_and_computes_render_size() {
        let config = TemporalRenderConfig::new(0.5);
        assert_eq!(config.render_size(512, 256), (256, 128));
        assert_eq!(
            TemporalRenderConfig::new(1.0).render_size(512, 512),
            (512, 512)
        );
        assert_eq!(
            TemporalRenderConfig::new(0.0).render_scale,
            MIN_RENDER_SCALE
        );
        assert_eq!(
            TemporalRenderConfig::new(4.0).render_scale,
            DEFAULT_RENDER_SCALE
        );
        // Never zero, even for tiny outputs.
        assert_eq!(TemporalRenderConfig::new(0.25).render_size(1, 1), (1, 1));
    }

    #[test]
    fn halton_sequence_is_deterministic_and_bounded() {
        for base in [2_u64, 3, 5] {
            for index in 1..=16_u64 {
                let value = halton(index, base);
                assert!(
                    (-1.0..=1.0).contains(&value),
                    "base={base} i={index} v={value}"
                );
                assert_eq!(value, halton(index, base), "deterministic");
            }
        }
        // Known values: halton(1,2) = 0.5 -> mapped 0.0; halton(2,2) = 0.25 -> -0.5.
        assert!((halton(1, 2) - 0.0).abs() < 1e-6);
        assert!((halton(2, 2) - (-0.5)).abs() < 1e-6);
        assert!((halton(1, 3) - (-1.0 / 3.0)).abs() < 1e-6);
    }

    #[test]
    fn jitter_sequence_advances_and_stays_subpixel() {
        let mut seq = JitterSequence::new();
        assert_eq!(seq.frame(), 0);
        for expected_frame in 1..=8_u64 {
            let jitter = seq.next(512, 512);
            assert_eq!(seq.frame(), expected_frame);
            // At 512px, half a pixel in NDC is 1/512; the offset never exceeds it.
            assert!(
                jitter[0].abs() <= 1.0 / 512.0 + 1e-9 && jitter[1].abs() <= 1.0 / 512.0 + 1e-9,
                "sub-pixel NDC: {jitter:?}"
            );
        }
        // Deterministic: a fresh sequence reproduces the same offsets.
        let mut twin = JitterSequence::new();
        let mut seq2 = JitterSequence::new();
        for _ in 0..8 {
            assert_eq!(seq2.next(512, 512), twin.next(512, 512));
        }
    }

    #[test]
    fn jitter_translation_offsets_ndc_by_the_jitter_amount() {
        let vp = Mat4::perspective(1.0, 1.0, 0.1, 100.0);
        let jittered = jitter_translation([0.1, -0.2]).mul(&vp);
        let p = [0.0, 0.0, -10.0];
        let plain = vp.transform_point(p);
        let with_jitter = jittered.transform_point(p);
        // The offset is applied in clip space, so the NDC delta equals jitter.
        let plain_ndc = [plain[0] / plain[3], plain[1] / plain[3]];
        let jitter_ndc = [
            with_jitter[0] / with_jitter[3],
            with_jitter[1] / with_jitter[3],
        ];
        assert!((jitter_ndc[0] - plain_ndc[0] - 0.1).abs() < 1e-5);
        assert!((jitter_ndc[1] - plain_ndc[1] + 0.2).abs() < 1e-5);
        assert_eq!(with_jitter[3], plain[3]);
    }

    #[test]
    fn history_lifecycle_resets_on_discontinuity_and_warms_up() {
        let mut history = TemporalHistory::new();
        assert!(!history.is_valid(), "fresh history is invalid (warm-up)");
        history.advance_frame();
        assert!(history.is_valid());
        assert_eq!(history.frames_since_reset(), 1);
        history.advance_frame();
        assert_eq!(history.frames_since_reset(), 2);
        history.note_discontinuity(Discontinuity::CameraCut);
        assert!(!history.is_valid());
        assert_eq!(history.frames_since_reset(), 0);
        assert_eq!(history.reset_count(), 1);
        assert_eq!(history.last_reset(), Some(Discontinuity::CameraCut));
        history.advance_frame();
        assert!(history.is_valid(), "first frame after reset warms up");
        history.note_discontinuity(Discontinuity::ResolutionChange);
        assert_eq!(history.last_reset(), Some(Discontinuity::ResolutionChange));
        assert_eq!(history.reset_count(), 2);
    }

    #[test]
    fn contracts_describe_the_fallback_and_fsr_requirements() {
        let fallback = fallback_contract();
        assert!(fallback.required_inputs.contains(&INPUT_SCENE_COLOR));
        assert!(fallback.separates_render_resolution);
        assert!(!fallback.uses_history, "bilinear fallback is stateless");
        let fsr = fsr4_contract();
        for input in [
            INPUT_SCENE_COLOR,
            INPUT_DEPTH,
            INPUT_MOTION,
            INPUT_JITTER,
            INPUT_HISTORY,
        ] {
            assert!(fsr.required_inputs.contains(&input), "fsr requires {input}");
        }
        assert!(fsr.optional_inputs.contains(&INPUT_EXPOSURE));
        assert!(fsr.needs_jitter && fsr.uses_history);
        // Headless: only the fallback is available, never a hard dependency.
        let available = available_upscalers();
        assert_eq!(available, vec![fallback_contract()]);
    }

    #[test]
    fn half_to_f32_decodes_known_patterns() {
        assert_eq!(half_to_f32(0x0000), 0.0);
        assert_eq!(half_to_f32(0x8000), -0.0);
        assert_eq!(half_to_f32(0x3c00), 1.0);
        assert_eq!(half_to_f32(0xbc00), -1.0);
        assert_eq!(half_to_f32(0x4000), 2.0);
        assert!(
            (half_to_f32(0x3555) - 0.33325).abs() < 1e-4,
            "0.3333 in half"
        );
        assert_eq!(half_to_f32(0x7bff), 65504.0, "max half");
        assert!(half_to_f32(0x7c00).is_infinite());
        assert!(half_to_f32(0x7e00).is_nan());
        // Subnormal: 0x0001 = 2^-24.
        assert!((half_to_f32(0x0001) - 2.0_f32.powi(-24)).abs() < 1e-12);
    }
}
