# ADR-0002: Determinism and Numeric Model (floating-point policy)

- **Status:** Accepted
- **Date:** 2026-11-15
- **Related:** [ADR-0001](0001-authoritative-simulation.md) Decision §5 (floating-point/numeric policy) and §9 (milestone scoping); [RtsEngineDoc.md](../../RtsEngineDoc.md) §3.2 (deterministic simulation contract), §13.3 (state hashing), §17.2 (ADR governance); issue #50.

## Context

ADR-0001 Decision §5 requires the floating-point/numeric policy to be documented before authoritative world math (positions, velocities, movement, collision, combat) lands. A canonical hash of float bit patterns does not make the *calculations* that produced those values deterministic: math-library choices, compiler fast-math settings, NaN/signed-zero/denormal handling, rounding rules, and cross-platform guarantees all become expensive to change once gameplay systems depend on them.

`phalanx_core::hash` already canonical-encodes `f32` as IEEE-754 little-endian bits (and `f64` likewise, added with this ADR), covered by `CANONICAL_ENCODING_VERSION`. `phalanx_navigation` already carries `f32` positions in authoritative state. CI runs the whole workspace test suite on both `ubuntu-latest` and `windows-latest` with the same stable toolchain.

## Problem

Define, before gameplay float math is written:

- The initial determinism target (same build/platform vs cross-platform bit identity) and exactly what may be claimed.
- Permitted float operations and the compiler/fast-math constraints.
- Handling of NaN, infinities, negative zero, and denormals.
- Fixed-point/integer use cases and rounding rules.
- The canonical float encoding.
- The Windows/Linux determinism experiments and their acceptance criteria.

## Alternatives considered

1. **Claim full cross-platform bit-identical lockstep now.** Rejected: different compilers, libm versions, and CPU microarchitectures are not bit-identical for all operations; ADR-0001 §9 forbids claiming it before automated equivalence tests pass. It remains a future acceptance target.
2. **Use a fixed-point-only model everywhere.** Rejected for M1: positions/velocities/geometry are far more naturally expressed in floats, and fixed-point everywhere would complicate navigation/combat code without buying provable cross-platform identity (fixed-point is still exact, but the encoding alone was never the issue).
3. **Allow fast-math for speed.** Rejected for authoritative code: reassociation, reciprocal approximations, and FMA contraction change results per compiler/flags and would silently break replay identity. Fast-math may be revisited only for derived, non-authoritative presentation paths with a separate validation design.
4. **Leave floats unhandled (bit-pattern hashing only).** Rejected: that hashes values but does not make their computation deterministic, which is the actual risk called out in ADR-0001 §5.

## Decision

### 1. Determinism target and permitted claims

- **Guaranteed now:** same-build, same-platform bit-identical replay and state hashes (ADR-0001 §9). Additionally, the fixed numeric test vectors in `phalanx_core::numeric` must produce identical canonical digests on `ubuntu-latest` and `windows-latest` with the same stable rustc/LLVM toolchain configuration — this is enforced by the existing CI OS matrix running `cargo test --workspace`, which includes the golden-vector tests.
- **Not claimed:** bit identity across different compilers, compiler versions, CPU microarchitectures, or GPUs. Cross-platform bit-identical lockstep remains a future acceptance target (ADR-0001 §9) that must not be promised until automated equivalence tests prove it on both OSes with the chosen math and settings.

### 2. Permitted float operations in authoritative state

Authoritative math may use only operations with exactly defined IEEE-754 semantics on supported targets, all available in `core`/`std` without libm:

- Basic arithmetic `+ - * / %` (each exactly rounded; no reassociation), `sqrt` (correctly rounded), `abs`, `signum`, `floor`, `ceil`, `round`, `trunc`, `min`, `max`, `powi`, comparisons, and `to_bits`/`from_bits`.
- **Forbidden in authoritative state for M1:** transcendental and other libm functions (`sin`, `cos`, `tan`, `exp`, `ln`, `log`, `powf`, `atan2`, `hypot`, …) — libm results are not guaranteed portable across platforms/toolchains. If gameplay needs them later, they must go through a proven-equivalent or deterministic replacement (e.g. fixed-point polynomial) with cross-OS tests, via a new ADR.
- No FMA contraction: Rust/LLVM does not contract `a*b+c` into an FMA unless explicitly enabled, and it must stay that way (see §3).
- No reassociation-sensitive reductions; reductions combine in a fixed, documented order (ADR-0001 §6).

### 3. Compiler and fast-math constraints

- `-C fast-math` (and its sub-options: reassociation, reciprocal approximations, FTZ/DAZ, FMA contraction) must **never** be enabled for authoritative code — not in `Cargo.toml` profiles, not via `RUSTFLAGS`, not via `.cargo/config.toml`.
- CI guards the manifests: the `rust` job greps all `Cargo.toml` files for `fast-math|fp-contract|ftz|daz` and fails if found.
- Rust's default code generation on supported targets (x86-64, aarch64) implements strict IEEE-754 for the permitted operations above, with denormals preserved (no FTZ/DAZ by default).

### 4. NaN and infinities

- NaN and ±infinity are **invalid in authoritative gameplay state**.
- The canonical encoding preserves their bit patterns exactly (NaN payloads included), so hashing and snapshots detect and round-trip them without ambiguity — deterministic handling means detectable, not silently normalized.
- Validation boundaries must reject non-finite values with structured errors: `phalanx_core::numeric::require_finite_f32`/`require_finite_f64` return `NonFiniteFloat::{NaN, PositiveInfinity, NegativeInfinity}`. A system whose computation would produce a non-finite value reports a structured `SystemError` (issue #37) and the value is never stored.

### 5. Negative zero and denormals

- The canonical encoding is bit-exact: `+0.0` and `-0.0` hash differently, and IEEE-754 equality treats them as equal (`-0.0 == 0.0`). Both behaviors are intentional: equality for logic, distinct bits for digests.
- State whose meaning is sign-insensitive should normalize explicitly with `numeric::normalize_negative_zero_f32`/`_f64` (maps `-0.0` to `+0.0`, leaves everything else — including NaN — bit-untouched).
- Denormals/subnormals are preserved bit-exactly everywhere; flush-to-zero (FTZ/DAZ) is forbidden (§3).

### 6. Integers and fixed-point

- Integer arithmetic is exact; use checked operations and structured errors on overflow (never wrapping in authoritative state).
- Division must be explicit and intentional: `div_euclid`, `div_floor`, or remainder-based logic — never bare `/` where truncation semantics are an accident.
- The gameplay rounding rule for integer division is **round half away from zero**, implemented once as `numeric::div_round_half_away_from_zero(n, d) -> Option<i64>` (`None` on zero denominator or overflow). Resource/economy accounting uses integer units (ADR-0001 §5), not float currency.
- Fixed-point is allowed only with a documented `SCALE` constant on the type (e.g. `i64` millimeters, `SCALE = 1000`), encoded canonically as the raw integer.

### 7. Canonical float encoding

`f32`/`f64` canonical-encode as their IEEE-754 bit pattern in little-endian (`to_bits`/`from_bits`), inside the versioned canonical encoding (`CANONICAL_ENCODING_VERSION`, `RtsEngineDoc.md` §13.3). The encoding is bit-exact, so it preserves `-0.0`, denormals, and NaN payloads; gameplay-validity of non-finite values is enforced separately (§4), not by the encoding.

### 8. Windows/Linux determinism experiments and acceptance criteria

- The existing CI matrix runs `cargo test --workspace` on `ubuntu-latest` and `windows-latest` with the same stable toolchain; `phalanx_core::numeric` contains golden-vector tests whose expected canonical digests are hardcoded constants. Passing on both OSes is the automated same-toolchain cross-OS equivalence proof for the permitted operation set.
- Acceptance for the M1 claim "same stable toolchain, Windows and Linux, bit-identical for the permitted operations": the golden vectors (covering basic ops, sqrt, rounding ops, denormals, -0.0, NaN/inf round-trip, and a representative movement-step computation) must pass on both OSes.
- Anything beyond that (other compilers, other CPUs, GPUs, lockstep multiplayer) is out of scope and unclaimed (ADR-0001 §9).

## Consequences

- Gameplay code can use `f32` positions/velocities now (as `phalanx_navigation` already does) under strict-IEEE rules; no libm in authoritative state.
- Replays and hashes remain bit-exact for floats, including pathological bit patterns, so divergence detection never masks a NaN by normalization.
- Non-finite authoritative values become structured errors at validation boundaries instead of silent state corruption.
- The CI fast-math guard and OS matrix make the numeric contract continuously verified rather than documented-once.
- Cost: transcendental functions are unavailable to authoritative systems until a deterministic replacement is proven; this is accepted for M1 (movement/combat need only the permitted set).

## Revisit criteria

- Revisit when: (a) automated cross-compiler or cross-CPU equivalence tests exist and justify a stronger claim; (b) a gameplay system genuinely needs a transcendental function (then design a proven-equivalent replacement and extend the golden vectors); (c) a supported target proves not to implement strict IEEE-754 for a permitted operation (then restrict that operation or move it to fixed-point); (d) fast-math is proposed for a derived, non-authoritative path (then scope it explicitly outside authoritative state).

## Open questions and validation experiments

- Whether M2+ needs portable transcendentals (trig for ballistics/arc effects) — resolve with a fixed-point/trig-table design plus cross-OS golden tests if so.
- Whether fixed-point should replace floats for economy/resource values end-to-end — ADR-0001 §5 already prefers integer units; confirm with the economy design.
- Cross-compiler bit-identity experiments (e.g. stable vs nightly, or MSVC vs GNU toolchains) — future work, only after the same-toolchain cross-OS proof is green in CI.
