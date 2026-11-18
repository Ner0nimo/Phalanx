//! Formation templates and deterministic slot assignment (issue #110;
//! `RtsEngineDoc.md` §2.4, §7.3).
//!
//! A formation template maps the members of a squad (in ascending
//! [`EntityId`] order — the canonical member ordering) to slot offsets in a
//! local frame whose +x axis is the group heading. The caller rotates the
//! offsets by the squad's heading vector and translates them to the anchor.
//! All math is plain `f32` multiply/add plus one IEEE-exact `sqrt`
//! (normalization), so slot assignment is deterministic across platforms per
//! ADR-0001/ADR-0002 (no `sin`/`cos`/`atan2`).

use phalanx_core::hash::{
    CanonicalDecode, CanonicalError, CanonicalReader, CanonicalSerialize, CanonicalSink,
};

/// Distance between adjacent formation slots, in meters.
pub const FORMATION_SPACING_M: f32 = 3.0;

/// The initial formation templates (issue #110; `RtsEngineDoc.md` §7.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormationTemplate {
    /// Single rank perpendicular to the heading.
    Line,
    /// Single file along the heading (column).
    Column,
    /// V/wedge: leader in front, pairs behind.
    Wedge,
    /// Dispersed: deterministic scatter with wide spacing.
    Loose,
    /// Defensive: a ring around the anchor (8-direction quantized circle, no
    /// trigonometry).
    Defensive,
}

impl FormationTemplate {
    /// Maps a canonical payload tag to a template; `None` for unknown tags.
    #[must_use]
    pub const fn from_tag(tag: u32) -> Option<Self> {
        match tag {
            0 => Some(Self::Line),
            1 => Some(Self::Column),
            2 => Some(Self::Wedge),
            3 => Some(Self::Loose),
            4 => Some(Self::Defensive),
            _ => None,
        }
    }

    /// The canonical payload tag of this template.
    #[must_use]
    pub const fn tag(self) -> u32 {
        match self {
            Self::Line => 0,
            Self::Column => 1,
            Self::Wedge => 2,
            Self::Loose => 3,
            Self::Defensive => 4,
        }
    }

    /// Deterministic slot offsets for `count` members, relative to the
    /// anchor, in the local frame (+x = heading, +y = left), spaced
    /// [`FORMATION_SPACING_M`] apart. Slot `i` belongs to the `i`-th member
    /// in ascending entity-ID order.
    #[must_use]
    pub fn slot_offsets(self, count: usize) -> Vec<(f32, f32)> {
        let s = FORMATION_SPACING_M;
        let n = count as f32;
        let mut out = Vec::with_capacity(count);
        match self {
            Self::Line => {
                for i in 0..count {
                    out.push((0.0, (i as f32 - (n - 1.0) / 2.0) * s));
                }
            }
            Self::Column => {
                for i in 0..count {
                    out.push((((n - 1.0) / 2.0 - i as f32) * s, 0.0));
                }
            }
            Self::Wedge => {
                for i in 0..count {
                    if i == 0 {
                        out.push((0.0, 0.0)); // leader on the anchor
                        continue;
                    }
                    let rank = (i + 1).div_ceil(2);
                    let side = if i % 2 == 0 { 1.0 } else { -1.0 };
                    out.push((-(rank as f32) * s, side * rank as f32 * s));
                }
            }
            Self::Loose => {
                // Deterministic low-discrepancy scatter (integer lattice
                // strides — no trigonometry, no RNG).
                for i in 0..count {
                    let x = ((i * 7 + 3) % 5) as f32 - 2.0;
                    let y = ((i * 3 + 1) % 5) as f32 - 2.0;
                    out.push((x * s * 0.8, y * s * 0.8));
                }
            }
            Self::Defensive => {
                // 8-direction quantized ring(s): direction index cycles
                // through the 8 compass offsets, radius grows every 8 slots.
                const DIRS: [(f32, f32); 8] = [
                    (1.0, 0.0),
                    (1.0, 1.0),
                    (0.0, 1.0),
                    (-1.0, 1.0),
                    (-1.0, 0.0),
                    (-1.0, -1.0),
                    (0.0, -1.0),
                    (1.0, -1.0),
                ];
                for i in 0..count {
                    let dir = DIRS[i % 8];
                    let ring = 1.0 + (i / 8) as f32;
                    let r = s * 0.7 * ring;
                    out.push((dir.0 * r, dir.1 * r));
                }
            }
        }
        out
    }
}

impl CanonicalSerialize for FormationTemplate {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u32(self.tag());
    }
}

impl CanonicalDecode for FormationTemplate {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        let tag = reader.read_u32()?;
        Self::from_tag(tag).ok_or(CanonicalError::UnknownTag(tag))
    }
}

/// Rotates a local-frame offset by the unit heading vector `(dx, dz)` and
/// translates it to `anchor`, returning the world-space slot position. The
/// heading must be normalized (callers use [`normalize_heading`]).
#[must_use]
pub fn rotate_slot(anchor: (f32, f32), heading: (f32, f32), offset: (f32, f32)) -> (f32, f32) {
    // Local frame: +x = heading (forward), +y = left = (-dz, dx).
    let (dx, dz) = heading;
    let forward = (dx, dz);
    let left = (-dz, dx);
    (
        anchor.0 + offset.0 * forward.0 + offset.1 * left.0,
        anchor.1 + offset.0 * forward.1 + offset.1 * left.1,
    )
}

/// Normalizes a direction to a unit heading; returns the default +x heading
/// for a zero-length direction. Uses only IEEE-exact operations.
#[must_use]
pub fn normalize_heading(dx: f32, dz: f32) -> (f32, f32) {
    let len = (dx * dx + dz * dz).sqrt();
    if len > 0.0 {
        (dx / len, dz / len)
    } else {
        (1.0, 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_assignment_is_deterministic_and_centered() {
        for template in [
            FormationTemplate::Line,
            FormationTemplate::Column,
            FormationTemplate::Wedge,
            FormationTemplate::Loose,
            FormationTemplate::Defensive,
        ] {
            let a = template.slot_offsets(5);
            let b = template.slot_offsets(5);
            assert_eq!(a, b, "slot offsets must be deterministic");
            assert_eq!(a.len(), 5);
            if template == FormationTemplate::Wedge {
                // A wedge is front-anchored: the leader sits on the anchor.
                assert_eq!(a[0], (0.0, 0.0), "wedge leader on the anchor");
            } else if template == FormationTemplate::Defensive {
                // A defensive ring is not necessarily symmetric for counts
                // that are not multiples of 8; distinctness is checked below.
            } else {
                // The other templates are centered on the anchor.
                let cx: f32 = a.iter().map(|o| o.0).sum::<f32>() / a.len() as f32;
                let cy: f32 = a.iter().map(|o| o.1).sum::<f32>() / a.len() as f32;
                assert!(cx.abs() < 1e-6 && cy.abs() < 1e-6, "{template:?} centered");
            }
            // No duplicate slots.
            let mut sorted = a.clone();
            sorted.sort_by(|p, q| p.0.total_cmp(&q.0).then(p.1.total_cmp(&q.1)));
            for w in sorted.windows(2) {
                assert!(w[0] != w[1], "{template:?} slots must be distinct");
            }
        }
    }

    #[test]
    fn templates_are_distinct() {
        let a = FormationTemplate::Line.slot_offsets(4);
        let b = FormationTemplate::Column.slot_offsets(4);
        let c = FormationTemplate::Wedge.slot_offsets(4);
        let d = FormationTemplate::Loose.slot_offsets(4);
        let e = FormationTemplate::Defensive.slot_offsets(4);
        for other in [&b, &c, &d, &e] {
            assert_ne!(&a, other);
        }
    }

    #[test]
    fn template_tags_round_trip() {
        for template in [
            FormationTemplate::Line,
            FormationTemplate::Column,
            FormationTemplate::Wedge,
            FormationTemplate::Loose,
            FormationTemplate::Defensive,
        ] {
            assert_eq!(FormationTemplate::from_tag(template.tag()), Some(template));
        }
        assert_eq!(FormationTemplate::from_tag(5), None);
    }

    #[test]
    fn rotation_by_unit_heading_preserves_lengths() {
        let heading = normalize_heading(3.0, 4.0);
        let slot = rotate_slot((10.0, 20.0), heading, (3.0, -2.0));
        let local_len = (3.0_f32 * 3.0 + 2.0 * 2.0).sqrt();
        let world_len = ((slot.0 - 10.0).powi(2) + (slot.1 - 20.0).powi(2)).sqrt();
        assert!(
            (world_len - local_len).abs() < 1e-5,
            "rotation preserves length"
        );
        // Zero direction falls back to +x.
        assert_eq!(normalize_heading(0.0, 0.0), (1.0, 0.0));
    }
}
