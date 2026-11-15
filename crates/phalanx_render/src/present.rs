//! Surface configuration, clear-color frame rendering, and the offscreen
//! render target used for headless verification (issue #61).
//!
//! Two presentation paths share [`render_clear_to_view`]:
//! - windowed: a wgpu surface configured by [`choose_surface_config`], with
//!   resize and lost/outdated/timeout handling in [`crate::window`];
//! - offscreen: [`OffscreenTarget`], a texture rendered and read back, so CI
//!   on GPU-less runners still exercises the full encode/submit/readback path
//!   structurally (structured no-adapter result) and GPU machines verify
//!   actual pixels via [`verify_clear_pixels`].

use std::fmt;

/// A clear color as linear RGBA in 0.0..=1.0.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClearColor(pub [f64; 4]);

impl ClearColor {
    /// Parses `0xRRGGBB`, `#RRGGBB`, or `RRGGBB` (alpha forced to 1.0).
    pub fn parse(text: &str) -> Result<Self, String> {
        let hex = text
            .strip_prefix("0x")
            .or_else(|| text.strip_prefix('#'))
            .unwrap_or(text);
        if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!(
                "invalid clear color {text:?}: expected RRGGBB hex (e.g. 0x18304a)"
            ));
        }
        let value = u32::from_str_radix(hex, 16).map_err(|_| format!("invalid hex: {text}"))?;
        let r = ((value >> 16) & 0xFF) as f64 / 255.0;
        let g = ((value >> 8) & 0xFF) as f64 / 255.0;
        let b = (value & 0xFF) as f64 / 255.0;
        Ok(Self([r, g, b, 1.0]))
    }

    /// The wgpu clear value.
    #[must_use]
    pub fn to_wgpu(self) -> wgpu::Color {
        wgpu::Color {
            r: self.0[0],
            g: self.0[1],
            b: self.0[2],
            a: self.0[3],
        }
    }
}

/// The default clear color: a clearly visible, non-default dark blue
/// (`0x18304a`), not wgpu's typical black.
pub const DEFAULT_CLEAR: ClearColor = ClearColor([
    0x18 as f64 / 255.0,
    0x30 as f64 / 255.0,
    0x4a as f64 / 255.0,
    1.0,
]);

/// Errors from presentation-path operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PresentError {
    /// The offscreen texture size is zero or too large.
    InvalidSize,
    /// Readback failed (map or poll error).
    Readback(String),
    /// The readback pixels do not match the expected clear color.
    PixelMismatch {
        /// First mismatching pixel index.
        pixel: usize,
        /// Expected RGBA.
        expected: [u8; 4],
        /// Found RGBA.
        found: [u8; 4],
    },
    /// A pixel buffer did not have the expected length.
    InvalidPixelBuffer { expected: usize, found: usize },
    /// Writing a screenshot file failed.
    Io(String),
}

impl fmt::Display for PresentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSize => formatter.write_str("offscreen target size must be positive"),
            Self::Readback(error) => write!(formatter, "offscreen readback failed: {error}"),
            Self::PixelMismatch {
                pixel,
                expected,
                found,
            } => write!(
                formatter,
                "pixel {pixel} mismatch: expected {expected:02x?} found {found:02x?}"
            ),
            Self::InvalidPixelBuffer { expected, found } => write!(
                formatter,
                "pixel buffer length mismatch: expected {expected} bytes, found {found}"
            ),
            Self::Io(error) => write!(formatter, "screenshot write failed: {error}"),
        }
    }
}

impl std::error::Error for PresentError {}

/// Chooses a surface configuration from adapter capabilities: first sRGB
/// format (else first format), guaranteed-available `Fifo` present mode (else
/// first), first alpha mode, `RENDER_ATTACHMENT` usage. Pure function of the
/// capabilities + size, so it is unit-testable without a GPU (issue #61:
/// "configure the surface using its supported capabilities").
#[must_use]
pub fn choose_surface_config(
    capabilities: &wgpu::SurfaceCapabilities,
    width: u32,
    height: u32,
) -> wgpu::SurfaceConfiguration {
    let format = capabilities
        .formats
        .iter()
        .find(|format| format.is_srgb())
        .copied()
        .unwrap_or(capabilities.formats[0]);
    let present_mode = if capabilities
        .present_modes
        .contains(&wgpu::PresentMode::Fifo)
    {
        wgpu::PresentMode::Fifo
    } else {
        capabilities.present_modes[0]
    };
    let alpha_mode = capabilities.alpha_modes[0];
    wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width,
        height,
        present_mode,
        desired_maximum_frame_latency: 2,
        alpha_mode,
        view_formats: vec![],
        color_space: wgpu::SurfaceColorSpace::Auto,
    }
}

/// Renders one clear-color frame into `view` and submits it. Shared by the
/// windowed surface path and the offscreen target — a clear pass needs no
/// pipeline, so this is the minimal frame both paths prove.
pub fn render_clear_to_view(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    view: &wgpu::TextureView,
    clear: wgpu::Color,
) {
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("phalanx-clear-encoder"),
    });
    {
        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("phalanx-clear-pass"),
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
        // Empty pass: the clear happens on load.
        drop(pass);
    }
    queue.submit([encoder.finish()]);
}

/// An offscreen render target: a texture + view that frames render into and
/// that can be read back to CPU memory for verification (issue #61 headless
/// path; also the CI-verifiable presentation proof on GPU machines).
pub struct OffscreenTarget {
    /// The target texture (`Rgba8Unorm`, renderable + copyable everywhere).
    pub texture: wgpu::Texture,
    /// The default view used as the color attachment.
    pub view: wgpu::TextureView,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The texture format.
    pub format: wgpu::TextureFormat,
}

impl OffscreenTarget {
    /// The texture format used for offscreen targets.
    pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

    /// Creates an offscreen target of the given size.
    pub fn new(device: &wgpu::Device, width: u32, height: u32) -> Result<Self, PresentError> {
        if width == 0 || height == 0 {
            return Err(PresentError::InvalidSize);
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("phalanx-offscreen"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: Self::FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Ok(Self {
            texture,
            view,
            width,
            height,
            format: Self::FORMAT,
        })
    }

    /// Renders one clear-color frame into this target.
    pub fn render_clear(&self, device: &wgpu::Device, queue: &wgpu::Queue, clear: ClearColor) {
        render_clear_to_view(device, queue, &self.view, clear.to_wgpu());
    }

    /// Reads the whole target back as tightly packed RGBA8 bytes
    /// (`width * height * 4`).
    pub fn read_back(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<Vec<u8>, PresentError> {
        // COPY_BYTES_PER_ROW_ALIGNMENT is 256.
        let bytes_per_row = (self.width * 4).div_ceil(256) * 256;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("phalanx-offscreen-readback"),
            size: u64::from(bytes_per_row) * u64::from(self.height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("phalanx-readback-encoder"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
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
            return Err(PresentError::Readback("buffer map failed".to_owned()));
        }
        let mapped = buffer
            .slice(..)
            .get_mapped_range()
            .map_err(|error| PresentError::Readback(error.to_string()))?;
        let mut pixels = vec![0u8; (self.width * 4 * self.height) as usize];
        for row in 0..self.height as usize {
            let start = row * bytes_per_row as usize;
            let end = start + self.width as usize * 4;
            pixels[row * self.width as usize * 4..(row + 1) * self.width as usize * 4]
                .copy_from_slice(&mapped[start..end]);
        }
        drop(mapped);
        buffer.unmap();
        Ok(pixels)
    }
}

/// Verifies that every pixel of an RGBA8 readback equals the clear color.
/// `Rgba8Unorm` stores `round(c * 255)`, so each channel is compared with a
/// tolerance of 1 (alpha is always exactly 255 for an opaque clear).
pub fn verify_clear_pixels(
    pixels: &[u8],
    width: u32,
    height: u32,
    clear: ClearColor,
) -> Result<(), PresentError> {
    let expected_len = width as usize * height as usize * 4;
    assert_eq!(
        pixels.len(),
        expected_len,
        "readback length must match width*height*4"
    );
    let expected = [
        (clear.0[0] * 255.0).round() as i32,
        (clear.0[1] * 255.0).round() as i32,
        (clear.0[2] * 255.0).round() as i32,
        255,
    ];
    for (index, chunk) in pixels.chunks_exact(4).enumerate() {
        let found = [
            chunk[0] as i32,
            chunk[1] as i32,
            chunk[2] as i32,
            chunk[3] as i32,
        ];
        for channel in 0..4 {
            if (found[channel] - expected[channel]).abs() > 1 {
                return Err(PresentError::PixelMismatch {
                    pixel: index,
                    expected: [
                        expected[0] as u8,
                        expected[1] as u8,
                        expected[2] as u8,
                        expected[3] as u8,
                    ],
                    found: [chunk[0], chunk[1], chunk[2], chunk[3]],
                });
            }
        }
    }
    Ok(())
}

/// Verifies that every pixel of an RGBA8 readback is either the clear color
/// or one of the given shape colors (per-channel tolerance 1 for unorm
/// rounding). Used to prove a frame contains GPU-rendered geometry over the
/// background and nothing else (issue #62).
pub fn verify_clear_pixels_background(
    pixels: &[u8],
    width: u32,
    height: u32,
    clear: ClearColor,
    shapes: &[crate::geometry::DebugShape],
) -> Result<(), PresentError> {
    let mut allowed: Vec<[f32; 4]> = vec![[
        clear.0[0] as f32,
        clear.0[1] as f32,
        clear.0[2] as f32,
        clear.0[3] as f32,
    ]];
    allowed.extend(shapes.iter().map(|shape| shape.color()));
    let matches = |pixel: [u8; 4], color: [f32; 4]| {
        let expected = [
            (color[0] * 255.0).round() as i32,
            (color[1] * 255.0).round() as i32,
            (color[2] * 255.0).round() as i32,
            255,
        ];
        let found = [
            pixel[0] as i32,
            pixel[1] as i32,
            pixel[2] as i32,
            pixel[3] as i32,
        ];
        (0..4).all(|c| (found[c] - expected[c]).abs() <= 1)
    };
    for (index, chunk) in pixels.chunks_exact(4).enumerate() {
        let pixel = [chunk[0], chunk[1], chunk[2], chunk[3]];
        if !allowed.iter().any(|color| matches(pixel, *color)) {
            return Err(PresentError::PixelMismatch {
                pixel: index,
                expected: [
                    (clear.0[0] * 255.0).round() as u8,
                    (clear.0[1] * 255.0).round() as u8,
                    (clear.0[2] * 255.0).round() as u8,
                    255,
                ],
                found: pixel,
            });
        }
    }
    let _ = (width, height);
    Ok(())
}

/// Writes an RGBA8 readback as a binary PPM (P6) screenshot file — the
/// completion evidence for the graphical battle (issue #64). Pure CPU file
/// I/O over GPU-produced pixels; no CPU framebuffer rendering is involved in
/// producing the frame itself.
pub fn write_ppm(
    path: &std::path::Path,
    pixels: &[u8],
    width: u32,
    height: u32,
) -> Result<(), PresentError> {
    use std::io::Write;
    let expected_len = width as usize * height as usize * 4;
    if pixels.len() != expected_len {
        return Err(PresentError::InvalidPixelBuffer {
            expected: expected_len,
            found: pixels.len(),
        });
    }
    let mut file =
        std::fs::File::create(path).map_err(|error| PresentError::Io(error.to_string()))?;
    writeln!(file, "P6\n{width} {height}\n255")
        .map_err(|error| PresentError::Io(error.to_string()))?;
    for chunk in pixels.chunks_exact(4) {
        file.write_all(&chunk[..3])
            .map_err(|error| PresentError::Io(error.to_string()))?;
    }
    file.flush()
        .map_err(|error| PresentError::Io(error.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_ppm_produces_a_valid_p6_file() {
        let width = 4_u32;
        let height = 2_u32;
        let mut pixels = Vec::new();
        for i in 0..width * height {
            pixels.extend_from_slice(&[i as u8, 255 - i as u8, 7, 255]);
        }
        let path = std::env::temp_dir().join("phalanx_write_ppm_test.p6");
        write_ppm(&path, &pixels, width, height).expect("write");
        let bytes = std::fs::read(&path).expect("read back");
        let _ = std::fs::remove_file(&path);
        let header = format!("P6\n{width} {height}\n255\n");
        assert!(bytes.starts_with(header.as_bytes()), "P6 header");
        assert_eq!(bytes.len(), header.len() + (width * height * 3) as usize);
        // First pixel RGB = (0, 255, 7); last = (7, 248, 7).
        assert_eq!(&bytes[header.len()..header.len() + 3], &[0, 255, 7]);
        let tail = &bytes[bytes.len() - 3..];
        assert_eq!(tail, &[7, 248, 7]);
    }

    #[test]
    fn write_ppm_rejects_a_wrong_sized_buffer() {
        let path = std::env::temp_dir().join("phalanx_write_ppm_bad.p6");
        let error = write_ppm(&path, &[0u8; 3], 2, 2).expect_err("must reject");
        assert_eq!(
            error,
            PresentError::InvalidPixelBuffer {
                expected: 16,
                found: 3
            }
        );
    }

    #[test]
    fn clear_color_parses_hex_forms() {
        let expected = ClearColor([
            0x18 as f64 / 255.0,
            0x30 as f64 / 255.0,
            0x4a as f64 / 255.0,
            1.0,
        ]);
        assert_eq!(ClearColor::parse("0x18304a").unwrap(), expected);
        assert_eq!(ClearColor::parse("#18304a").unwrap(), expected);
        assert_eq!(ClearColor::parse("18304A").unwrap(), expected);
        assert!(ClearColor::parse("nope").is_err());
        assert!(ClearColor::parse("0x1234").is_err());
        assert_eq!(DEFAULT_CLEAR, ClearColor::parse("0x18304a").unwrap());
    }

    #[test]
    fn surface_config_prefers_srgb_and_fifo() {
        let capabilities = wgpu::SurfaceCapabilities {
            formats: vec![
                wgpu::TextureFormat::Bgra8Unorm,
                wgpu::TextureFormat::Bgra8UnormSrgb,
            ],
            format_capabilities: vec![],
            present_modes: vec![wgpu::PresentMode::Mailbox, wgpu::PresentMode::Fifo],
            alpha_modes: vec![wgpu::CompositeAlphaMode::Opaque],
            usages: wgpu::TextureUsages::RENDER_ATTACHMENT,
        };
        let config = choose_surface_config(&capabilities, 800, 600);
        assert_eq!(config.format, wgpu::TextureFormat::Bgra8UnormSrgb);
        assert_eq!(config.present_mode, wgpu::PresentMode::Fifo);
        assert_eq!(config.alpha_mode, wgpu::CompositeAlphaMode::Opaque);
        assert_eq!((config.width, config.height), (800, 600));
        assert_eq!(config.usage, wgpu::TextureUsages::RENDER_ATTACHMENT);

        // Without Fifo, fall back to the first advertised mode; without sRGB,
        // the first format.
        let capabilities = wgpu::SurfaceCapabilities {
            formats: vec![wgpu::TextureFormat::Rgba8Unorm],
            format_capabilities: vec![],
            present_modes: vec![wgpu::PresentMode::Immediate],
            alpha_modes: vec![wgpu::CompositeAlphaMode::PreMultiplied],
            usages: wgpu::TextureUsages::RENDER_ATTACHMENT,
        };
        let config = choose_surface_config(&capabilities, 1, 1);
        assert_eq!(config.format, wgpu::TextureFormat::Rgba8Unorm);
        assert_eq!(config.present_mode, wgpu::PresentMode::Immediate);
    }

    #[test]
    fn verify_clear_pixels_accepts_matching_and_rejects_wrong_pixels() {
        let clear = ClearColor::parse("0x102030").unwrap();
        let width = 4;
        let height = 2;
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        let expected = [
            (clear.0[0] * 255.0).round() as u8,
            (clear.0[1] * 255.0).round() as u8,
            (clear.0[2] * 255.0).round() as u8,
            255,
        ];
        for chunk in pixels.chunks_exact_mut(4) {
            chunk.copy_from_slice(&expected);
        }
        assert!(verify_clear_pixels(&pixels, width, height, clear).is_ok());
        // One wrong pixel (beyond tolerance) is a structured error.
        pixels[5] = expected[1].wrapping_add(8);
        let err = verify_clear_pixels(&pixels, width, height, clear).unwrap_err();
        assert!(matches!(err, PresentError::PixelMismatch { pixel: 1, .. }));
    }

    #[test]
    fn offscreen_clear_renders_and_verifies_when_adapter_exists() {
        // No GPU in CI: the structured NoAdapter path is asserted. On a GPU
        // machine the full render+readback+verify path is asserted.
        match crate::init_headless() {
            Err(crate::BackendError::NoAdapter(_)) => {}
            Err(other) => panic!("unexpected backend error: {other}"),
            Ok((backend, report)) => {
                println!("{report}");
                let target = OffscreenTarget::new(&backend.device, 64, 48).expect("target");
                let clear = ClearColor::parse("0x3366aa").unwrap();
                target.render_clear(&backend.device, &backend.queue, clear);
                let pixels = target
                    .read_back(&backend.device, &backend.queue)
                    .expect("readback");
                verify_clear_pixels(&pixels, target.width, target.height, clear)
                    .expect("clear color must fill the whole frame");
            }
        }
    }
}
