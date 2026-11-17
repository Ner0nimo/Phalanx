//! Minimal 4x4 matrix math for the depth-tested 3D presentation path (issue
//! #104).
//!
//! Deliberately dependency-free (the workspace has no math crate yet and
//! AGENTS.md forbids adding one for trivial functionality): a column-major
//! [`Mat4`] with just the operations the presentation path needs — perspective
//! projection in wgpu clip space (NDC z in `[0, 1]`, right-handed view looking
//! down `-z`), look-at view matrices, multiplication, and point transforms.
//! Everything is pure `f32` CPU math, so it is unit-tested without a GPU.

/// A column-major 4x4 matrix (`columns[j]` is column `j`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mat4 {
    /// Columns of the matrix.
    pub columns: [[f32; 4]; 4],
}

impl Mat4 {
    /// The identity matrix.
    pub const IDENTITY: Self = Self {
        columns: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ],
    };

    /// A perspective projection for wgpu clip space: NDC x/y in `[-1, 1]`,
    /// NDC z in `[0, 1]` (0 at `near`, 1 at `far`), right-handed view looking
    /// down `-z`.
    #[must_use]
    pub fn perspective(fov_y_rad: f32, aspect: f32, near: f32, far: f32) -> Self {
        let f = 1.0 / (fov_y_rad / 2.0).tan();
        Self {
            columns: [
                [f / aspect, 0.0, 0.0, 0.0],
                [0.0, f, 0.0, 0.0],
                [0.0, 0.0, far / (near - far), -1.0],
                [0.0, 0.0, (near * far) / (near - far), 0.0],
            ],
        }
    }

    /// A right-handed look-at view matrix: the camera at `eye` looks at
    /// `target`, with `up` as the approximate up direction.
    #[must_use]
    pub fn look_at(eye: [f32; 3], target: [f32; 3], up: [f32; 3]) -> Self {
        let forward = normalize(sub(target, eye));
        let right = normalize(cross(forward, up));
        let true_up = cross(right, forward);
        Self {
            columns: [
                [right[0], true_up[0], -forward[0], 0.0],
                [right[1], true_up[1], -forward[1], 0.0],
                [right[2], true_up[2], -forward[2], 0.0],
                [-dot(right, eye), -dot(true_up, eye), dot(forward, eye), 1.0],
            ],
        }
    }

    /// Matrix product `self * rhs` (applies `rhs` first).
    #[must_use]
    pub fn mul(&self, rhs: &Self) -> Self {
        let mut columns = [[0.0; 4]; 4];
        for (j, column) in rhs.columns.iter().enumerate() {
            columns[j] = self.transform_vector(*column);
        }
        Self { columns }
    }

    /// Transforms a homogeneous vector (no perspective divide).
    #[must_use]
    pub fn transform_vector(&self, v: [f32; 4]) -> [f32; 4] {
        let mut out = [0.0; 4];
        for (j, column) in self.columns.iter().enumerate() {
            for row in 0..4 {
                out[row] += column[row] * v[j];
            }
        }
        out
    }

    /// Transforms a 3D point to clip space (w included, no divide).
    #[must_use]
    pub fn transform_point(&self, p: [f32; 3]) -> [f32; 4] {
        self.transform_vector([p[0], p[1], p[2], 1.0])
    }

    /// Encodes the matrix as 64 little-endian bytes in column-major order —
    /// the exact layout WGSL `mat4x4<f32>` expects in a uniform buffer.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 64] {
        let mut out = [0u8; 64];
        for (j, column) in self.columns.iter().enumerate() {
            for (row, value) in column.iter().enumerate() {
                out[j * 16 + row * 4..j * 16 + row * 4 + 4].copy_from_slice(&value.to_le_bytes());
            }
        }
        out
    }
}

/// `a - b`.
#[must_use]
pub fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

/// Dot product.
#[must_use]
pub fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Cross product.
#[must_use]
pub fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Normalizes a vector (returns the zero vector for a zero input).
#[must_use]
pub fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = dot(v, v).sqrt();
    if len > 0.0 {
        [v[0] / len, v[1] / len, v[2] / len]
    } else {
        [0.0, 0.0, 0.0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_leaves_points_unchanged() {
        let p = Mat4::IDENTITY.transform_point([1.0, 2.0, 3.0]);
        assert_eq!(p, [1.0, 2.0, 3.0, 1.0]);
    }

    #[test]
    fn perspective_maps_near_to_zero_and_far_to_one() {
        let proj = Mat4::perspective(60f32.to_radians(), 1.5, 1.0, 100.0);
        let near_clip = proj.transform_point([0.0, 0.0, -1.0]);
        assert!(
            (near_clip[2] / near_clip[3]).abs() < 1e-6,
            "near -> ndc z = 0"
        );
        let far_clip = proj.transform_point([0.0, 0.0, -100.0]);
        assert!(
            (far_clip[2] / far_clip[3] - 1.0).abs() < 1e-5,
            "far -> ndc z = 1"
        );
        // A point on the +x edge of the frustum at mid depth maps to x in (0, 1).
        let mid = proj.transform_point([1.0, 0.0, -10.0]);
        let ndc_x = mid[0] / mid[3];
        assert!(ndc_x > 0.0 && ndc_x < 1.0, "ndc_x={ndc_x}");
    }

    #[test]
    fn look_at_puts_eye_at_origin_and_target_down_negative_z() {
        let view = Mat4::look_at([0.0, 5.0, 10.0], [0.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        let eye_view = view.transform_point([0.0, 5.0, 10.0]);
        assert!((eye_view[0]).abs() < 1e-6 && (eye_view[1]).abs() < 1e-6);
        let target_view = view.transform_point([0.0, 0.0, 0.0]);
        assert!(target_view[2] < 0.0, "target is in front (-z)");
        assert!((target_view[0]).abs() < 1e-6 && (target_view[1]).abs() < 1e-6);
    }

    #[test]
    fn mul_applies_rhs_first() {
        let view = Mat4::look_at([0.0, 0.0, 5.0], [0.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        let proj = Mat4::perspective(90f32.to_radians(), 1.0, 0.1, 100.0);
        let vp = proj.mul(&view);
        let clip = vp.transform_point([0.0, 0.0, 0.0]);
        let ndc = [clip[0] / clip[3], clip[1] / clip[3], clip[2] / clip[3]];
        // The target sits at the view center: ndc x/y = 0, z in (0, 1).
        assert!(ndc[0].abs() < 1e-6 && ndc[1].abs() < 1e-6);
        assert!(ndc[2] > 0.0 && ndc[2] < 1.0);
    }

    #[test]
    fn to_bytes_is_column_major_little_endian() {
        let m = Mat4 {
            columns: [
                [1.0, 2.0, 3.0, 4.0],
                [5.0, 6.0, 7.0, 8.0],
                [9.0, 10.0, 11.0, 12.0],
                [13.0, 14.0, 15.0, 16.0],
            ],
        };
        let bytes = m.to_bytes();
        assert_eq!(&bytes[0..4], &1.0f32.to_le_bytes());
        assert_eq!(&bytes[4..8], &2.0f32.to_le_bytes());
        assert_eq!(
            &bytes[16..20],
            &5.0f32.to_le_bytes(),
            "column 1 starts at byte 16"
        );
        assert_eq!(&bytes[60..64], &16.0f32.to_le_bytes());
    }
}
