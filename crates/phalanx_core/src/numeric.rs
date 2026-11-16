//! Authoritative numeric and floating-point determinism rules (ADR-0002,
//! `docs/adr/0002-determinism-and-numeric-model.md`; issue #50).
//!
//! Binding policy summary:
//!
//! - **Determinism target:** same-build, same-platform bit-identical state
//!   hashes; the CI OS matrix (ubuntu + windows, same stable toolchain) proves
//!   the golden vectors below produce identical canonical digests on both.
//!   Cross-compiler/cross-CPU bit identity is NOT claimed.
//! - **Permitted float operations** in authoritative state: IEEE-754 basic
//!   arithmetic (`+ - * / %`), `sqrt`, `abs`, `signum`, `floor`, `ceil`,
//!   `round`, `trunc`, `min`, `max`, `clamp`, `powi`, comparisons, `to_bits`/`from_bits`
//!   — all exactly rounded or bit-exact under strict IEEE-754, which Rust
//!   guarantees on supported targets *without* fast-math. Transcendental/libm
//!   functions are forbidden in authoritative state for M1.
//! - **No fast-math:** `-C fast-math`, FTZ/DAZ, reassociation, and FMA
//!   contraction must never be enabled (CI greps the manifests). Rust does
//!   not contract `a * b + c` into an FMA by default.
//! - **NaN / infinities** are invalid in authoritative gameplay state:
//!   [`require_finite_f32`]/[`require_finite_f64`] reject them with a
//!   structured error; the canonical encoding still preserves their bit
//!   patterns exactly so hashing detects them instead of normalizing them.
//! - **Negative zero:** encoded bit-exactly (`+0.0` and `-0.0` hash
//!   differently; IEEE equality treats them as equal).
//!   [`normalize_negative_zero_f32`] maps `-0.0` to `+0.0` for
//!   sign-insensitive state. **Denormals** are preserved; FTZ is forbidden.
//! - **Integers** are exact with checked operations; division is explicit,
//!   and the gameplay rounding rule is round-half-away-from-zero via
//!   [`div_round_half_away_from_zero`]. Fixed-point requires a documented
//!   scale constant per type.
//! - **Canonical float encoding:** `f32`/`f64` as IEEE-754 LE bits
//!   (`crate::hash`), covered by `CANONICAL_ENCODING_VERSION`.

/// Structured error for a non-finite value in authoritative state (ADR-0002
/// §4). NaN and ±infinity are invalid in authoritative gameplay state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NonFiniteFloat {
    /// The value was NaN.
    NaN,
    /// The value was +infinity.
    PositiveInfinity,
    /// The value was -infinity.
    NegativeInfinity,
}

impl std::fmt::Display for NonFiniteFloat {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NaN => formatter.write_str("NaN is invalid in authoritative state"),
            Self::PositiveInfinity => {
                formatter.write_str("+infinity is invalid in authoritative state")
            }
            Self::NegativeInfinity => {
                formatter.write_str("-infinity is invalid in authoritative state")
            }
        }
    }
}

impl std::error::Error for NonFiniteFloat {}

/// Validates that an authoritative `f32` is finite (not NaN, not infinite).
/// Validation boundaries call this and reject with the structured error
/// instead of storing a non-finite value (ADR-0002 §4).
pub fn require_finite_f32(value: f32) -> Result<f32, NonFiniteFloat> {
    if value.is_nan() {
        Err(NonFiniteFloat::NaN)
    } else if value == f32::INFINITY {
        Err(NonFiniteFloat::PositiveInfinity)
    } else if value == f32::NEG_INFINITY {
        Err(NonFiniteFloat::NegativeInfinity)
    } else {
        Ok(value)
    }
}

/// Validates that an authoritative `f64` is finite (see
/// [`require_finite_f32`]).
pub fn require_finite_f64(value: f64) -> Result<f64, NonFiniteFloat> {
    if value.is_nan() {
        Err(NonFiniteFloat::NaN)
    } else if value == f64::INFINITY {
        Err(NonFiniteFloat::PositiveInfinity)
    } else if value == f64::NEG_INFINITY {
        Err(NonFiniteFloat::NegativeInfinity)
    } else {
        Ok(value)
    }
}

/// Normalizes `-0.0` to `+0.0`, leaving every other bit pattern — including
/// NaN — untouched (ADR-0002 §5). Use for state whose meaning is
/// sign-insensitive, so digests do not depend on the sign of zero.
#[must_use]
pub fn normalize_negative_zero_f32(value: f32) -> f32 {
    if value == 0.0 { 0.0 } else { value }
}

/// `f64` variant of [`normalize_negative_zero_f32`].
#[must_use]
pub fn normalize_negative_zero_f64(value: f64) -> f64 {
    if value == 0.0 { 0.0 } else { value }
}

/// Integer division with the gameplay rounding rule: **round half away from
/// zero** (ADR-0002 §6). Returns `None` on a zero denominator or overflow
/// (`i64::MIN / -1`), never wrapping.
///
/// ```
/// use phalanx_core::numeric::div_round_half_away_from_zero;
/// assert_eq!(div_round_half_away_from_zero(7, 2), Some(4));
/// assert_eq!(div_round_half_away_from_zero(-7, 2), Some(-4));
/// assert_eq!(div_round_half_away_from_zero(1, 2), Some(1));
/// assert_eq!(div_round_half_away_from_zero(0, 5), Some(0));
/// assert_eq!(div_round_half_away_from_zero(1, 0), None);
/// ```
#[must_use]
pub fn div_round_half_away_from_zero(numerator: i64, denominator: i64) -> Option<i64> {
    if denominator == 0 {
        return None;
    }
    // i128 math avoids the i64::MIN / -1 overflow entirely.
    let n = i128::from(numerator);
    let d = i128::from(denominator);
    let quotient = n / d;
    let remainder = n % d;
    let rounded = if 2 * remainder.abs() >= d.abs() {
        // Round the magnitude up: away from zero.
        if (numerator < 0) == (denominator < 0) {
            quotient + 1
        } else {
            quotient - 1
        }
    } else {
        quotient
    };
    i64::try_from(rounded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::{CanonicalSerialize, canonical_decode, canonical_digest, canonical_encode};

    #[test]
    fn non_finite_values_are_structured_errors() {
        assert_eq!(require_finite_f32(f32::NAN), Err(NonFiniteFloat::NaN));
        assert_eq!(
            require_finite_f32(f32::INFINITY),
            Err(NonFiniteFloat::PositiveInfinity)
        );
        assert_eq!(
            require_finite_f32(f32::NEG_INFINITY),
            Err(NonFiniteFloat::NegativeInfinity)
        );
        assert_eq!(require_finite_f64(f64::NAN), Err(NonFiniteFloat::NaN));
        // Finite values — including zero, negative zero, denormals, and MAX —
        // pass.
        for value in [
            0.0_f32,
            -0.0,
            1e-40,
            f32::MIN_POSITIVE,
            1.5,
            -2.75,
            f32::MAX,
        ] {
            assert_eq!(require_finite_f32(value), Ok(value));
        }
        assert_eq!(require_finite_f64(-0.0), Ok(-0.0));
    }

    #[test]
    fn negative_zero_normalization_touches_only_zero() {
        assert_eq!(
            normalize_negative_zero_f32(-0.0).to_bits(),
            0.0_f32.to_bits()
        );
        assert_eq!(
            normalize_negative_zero_f32(0.0).to_bits(),
            0.0_f32.to_bits()
        );
        assert_eq!(
            normalize_negative_zero_f32(1.5).to_bits(),
            1.5_f32.to_bits()
        );
        assert_eq!(
            normalize_negative_zero_f32(-1.5).to_bits(),
            (-1.5_f32).to_bits()
        );
        // NaN is left bit-untouched (still detectable, still invalid).
        let nan = f32::NAN;
        assert_eq!(normalize_negative_zero_f32(nan).to_bits(), nan.to_bits());
        assert_eq!(
            normalize_negative_zero_f64(-0.0).to_bits(),
            0.0_f64.to_bits()
        );
    }

    #[test]
    fn integer_rounding_is_half_away_from_zero() {
        assert_eq!(div_round_half_away_from_zero(7, 2), Some(4));
        assert_eq!(div_round_half_away_from_zero(-7, 2), Some(-4));
        assert_eq!(div_round_half_away_from_zero(7, -2), Some(-4));
        assert_eq!(div_round_half_away_from_zero(-7, -2), Some(4));
        assert_eq!(div_round_half_away_from_zero(5, 2), Some(3));
        assert_eq!(div_round_half_away_from_zero(1, 2), Some(1));
        assert_eq!(div_round_half_away_from_zero(6, 3), Some(2));
        assert_eq!(div_round_half_away_from_zero(0, 5), Some(0));
        assert_eq!(div_round_half_away_from_zero(1, 0), None);
        assert_eq!(
            div_round_half_away_from_zero(i64::MIN, -1),
            None,
            "overflow is an error, never a wrap"
        );
        assert_eq!(div_round_half_away_from_zero(i64::MIN, 1), Some(i64::MIN));
    }

    #[test]
    fn canonical_float_encoding_is_bit_exact_for_all_patterns() {
        let values: Vec<f32> = vec![
            0.0,
            -0.0,
            1e-40, // denormal
            f32::MIN_POSITIVE,
            1.5,
            -2.75,
            f32::MAX,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ];
        let bytes = canonical_encode(&values);
        let decoded: Vec<f32> = canonical_decode(&bytes).expect("round-trip");
        let bits: Vec<u32> = decoded.iter().map(|value| value.to_bits()).collect();
        let expected: Vec<u32> = values.iter().map(|value| value.to_bits()).collect();
        assert_eq!(bits, expected, "bit-exact, including NaN payload and -0.0");
        // Same for f64.
        let wide: Vec<f64> = vec![0.0, -0.0, 1e-300, 1.5, f64::NAN, f64::INFINITY];
        let decoded_wide: Vec<f64> = canonical_decode(&canonical_encode(&wide)).expect("f64");
        assert_eq!(
            decoded_wide.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            wide.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
        );
    }

    /// A representative authoritative movement-step computation over fixed
    /// inputs, using only the permitted operation set (ADR-0002 §2): basic
    /// arithmetic, sqrt, floor/ceil/round/min/max.
    fn movement_step(position: [f32; 2], velocity: [f32; 2], dt: f32, ticks: u32) -> Vec<f32> {
        let mut out = Vec::new();
        let mut pos = position;
        for _ in 0..ticks {
            // Permitted: exactly-rounded +, *, and sqrt (correctly rounded).
            pos = [pos[0] + velocity[0] * dt, pos[1] + velocity[1] * dt];
            let distance = (pos[0] * pos[0] + pos[1] * pos[1]).sqrt();
            out.push(pos[0]);
            out.push(pos[1]);
            out.push(distance);
            out.push(distance.floor());
            out.push(distance.ceil());
            out.push(distance.round());
            out.push(distance.clamp(0.0, 100.0));
        }
        out
    }

    /// Golden digest of the representative computation + the pathological
    /// vector. The constant is the cross-OS equivalence proof: CI runs this
    /// test on ubuntu-latest and windows-latest with the same stable toolchain
    /// and both must produce exactly this digest (ADR-0002 §8). It was
    /// generated on x86-64 Linux with the stable toolchain; any divergence on
    /// a supported OS/CPU fails CI instead of silently weakening the claim.
    const GOLDEN_DIGEST: u64 = 0x04a7_91d0_3ff6_50b7;

    #[test]
    fn golden_numeric_vectors_have_a_stable_cross_os_digest() {
        let computed = movement_step([3.0, -4.0], [1.5, 0.25], 0.05, 64);
        let pathological: Vec<f32> =
            vec![0.0, -0.0, 1e-40, f32::MIN_POSITIVE, f32::NAN, f32::INFINITY];
        #[derive(Clone)]
        struct Vectors {
            computed: Vec<f32>,
            pathological: Vec<f32>,
        }
        impl CanonicalSerialize for Vectors {
            fn write_canonical(&self, sink: &mut dyn crate::hash::CanonicalSink) {
                self.computed.write_canonical(sink);
                self.pathological.write_canonical(sink);
            }
        }
        let vectors = Vectors {
            computed,
            pathological,
        };
        // Same computation twice: identical digests within a run.
        assert_eq!(
            canonical_digest(&vectors),
            canonical_digest(&vectors),
            "identical inputs must hash identically"
        );
        let digest = canonical_digest(&vectors);
        let value = u64::from_le_bytes(
            digest
                .value()
                .try_into()
                .expect("FNV-1a 64 digest is 8 bytes"),
        );
        assert_eq!(
            value, GOLDEN_DIGEST,
            "golden numeric vector digest changed — this must be a deliberate, reviewed ADR-0002 change with a regenerated constant"
        );
    }
}
