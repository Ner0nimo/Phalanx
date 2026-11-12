# ADR-0001: Authoritative Simulation and Determinism Contract

- **Status:** Accepted
- **Date:** 2026-11-12
- **Related:** [RtsEngineDoc.md](../../RtsEngineDoc.md) §3 (architecture invariants), §6 (simulation model), §13 (savegames, replays, hashing), §17.2 (ADR governance), §18 (milestones M0/M1), §19 (multiplayer); issue #3.

## Context

Phalanx is a large-scale RTS whose first product is singleplayer with skirmish AI on Windows and Linux. `RtsEngineDoc.md` §3 already declares the simulation invariants binding; this ADR turns them into an explicit, reviewable decision before implementation choices in ECS storage, GPU compute, rendering, replay, testing, headless mode, and future multiplayer spread across the codebase and acquire incompatible assumptions.

The engine targets 10,000+ units per match (§14), must support replay, debugging, CI, fuzzing, and AI simulation without a graphics device (§3.3), and must preserve a path toward multiplayer without promising lockstep before cross-platform determinism is proven (§19).

## Problem

Gameplay outcomes must be exactly reproducible from recorded inputs, and every piece of gameplay state must have exactly one authoritative owner. Without an explicit contract, subsystem teams will independently choose between GPU-authoritative compute, multi-copy state, ad-hoc command timing, unseeded randomness, nondeterministic iteration, and implicit numeric assumptions — any of which would make replay, headless testing, and future multiplayer impossible or order-dependent. A fixed timestep is often mistaken for determinism; it is only a stepping mechanism and is not sufficient on its own.

The decision must define:

- CPU ownership of all gameplay-authoritative state.
- What is authoritative versus derived or presentation-only.
- Tick-boundary command application and explicit system-phase ordering.
- Deterministic RNG ownership and stable iteration/tie-breaking rules.
- The floating-point/numeric policy for the initial single-platform replay target and the future cross-platform lockstep question.
- How parallel jobs may write and reduce data without order-dependent results.
- CPU-only headless execution as a mandatory capability.
- State-hash and replay expectations.

## Alternatives considered

1. **GPU-authoritative simulation.** Let GPU compute own or co-own gameplay state (positions, combat, projectiles). Rejected for the initial architecture: cross-device floating-point variance, readback latency, and validation cost make replays and lockstep unprovable; GPU results that influence authoritative state would still require CPU validation or reconciliation. May be revisited only with a separate determinism design and cross-device validation.
2. **Fixed-timestep-only determinism.** Rely on a fixed tick rate alone to guarantee reproducibility. Rejected as a sufficient contract: it says nothing about command timing, phase ordering, RNG, iteration order, floating-point behavior, or parallel reductions. Fixed timestep is necessary for stable stepping but is not determinism.
3. **Cross-platform bit-identical lockstep as the initial target.** Demand bit-identical results across Windows and Linux from day one. Rejected as an initial commitment: unproven and potentially costly; it is kept as a future acceptance target that must be demonstrated by automated tests before being claimed.
4. **Server-authoritative multiplayer from the start.** Rejected: the initial product is singleplayer/skirmish; networking is deferred (§19). The simulation contract preserves the option but does not commit to it. If strict determinism later proves too costly, server-authoritative multiplayer may become the better product decision.
5. **Multiple mutable copies of gameplay state** (ECS plus separate SoA arrays plus GPU buffers). Rejected: violates single ownership, invites divergence, and makes state hashing and replay ambiguous.

## Decision

The authoritative simulation is CPU-only and deterministic by contract. The binding rules below codify `RtsEngineDoc.md` §3.1–§3.5, §6.1, and §13; where this ADR and the document disagree, the conflict must be resolved by an ADR update, not silently overridden.

### 1. CPU ownership of all gameplay-authoritative state

The CPU simulation owns all gameplay-authoritative state:

- Entity existence and identity.
- Gameplay positions and velocities.
- Health, damage, armor, status, and death.
- Orders, command queues, and group membership.
- Combat decisions and hit outcomes.
- Projectile gameplay state and collision/hit resolution.
- Economy, production, construction, and research.
- Territory/control state.
- Supply and logistics state.
- Player visibility/exploration state that affects gameplay decisions.
- AI decisions and deterministic random state.
- The simulation tick and the command/event history.

There is exactly one authoritative representation per gameplay fact. Derived caches and upload buffers are allowed only when their source of truth is explicit, their update/invalidation rules are documented, they can be rebuilt from authoritative state, and stale data cannot affect gameplay outcomes (§3.4). GPU compute may accelerate derived or non-authoritative work, but it must never be the sole source of truth for gameplay outcomes; GPU results that influence authoritative state must be validated or reconciled by the CPU, or remain disabled until a separate determinism design and cross-device validation prove them safe.

### 2. Authoritative vs derived vs presentation-only

- **Authoritative:** everything in the list above, including gameplay-affecting computed data such as movement paths used by authoritative movement, combat outcomes, and supply connectivity. Authoritative data lives only in the CPU simulation and is serialized into saves, replays, and the canonical state hash.
- **Derived:** rebuildable caches computed from authoritative state — strategic overlays, front-line approximations (§8.2), navigation debug views, LOD/culling results, render upload buffers, and GPU visibility textures. Derived data must never feed back into authoritative state.
- **Presentation-only:** camera, UI layout, selection, render interpolation, animation, particles, and visual prediction. The renderer consumes immutable/interpolated snapshots and visual metadata and must not mutate gameplay state (§3.5).

### 3. Tick-boundary command application and system-phase ordering

- The simulation advances in a fixed tick (initial proposal: 20 ticks/second; the final tick rate is an open question validated through gameplay and profiling). Rendering runs independently and interpolates between snapshots.
- Authoritative commands carry an explicit tick assignment and are validated at the system boundary; they are ingested and applied only at defined tick boundaries, in phase 1–2 of the tick. Editor-only map mutations are never silently mixed with active match state (§12).
- The system-phase order is explicit, documented, tested, and versioned. The starting contract is the 15-phase schedule in §6.1 (command ingest → … → invariant validation → canonical state hash and replay checkpoint). Systems may have explicit update frequencies or budgets, but their schedule and phase must be deterministic.
- Changing phase ordering can alter replay results and game balance; any change requires an ADR or an explicit simulation-version change.

### 4. Deterministic RNG ownership and stable iteration/tie-breaking

- All gameplay randomness uses explicitly seeded deterministic RNG streams owned by the CPU simulation. RNG seeds and stream state are part of authoritative state and are serialized in saves and replays (§13.1–13.2). No unseeded or global RNG, and no wall-clock reads, inside authoritative systems.
- Entity and command iteration order must be stable wherever order can affect outcomes. Entities use stable logical identifiers suitable for commands, replays, saves, and agent APIs; transient ECS archetype locations or raw memory indices are never exposed as persistent identifiers (§6.2).
- Simultaneous effects and target selection use deterministic tie-breaking (for example, by stable entity ID) (§9.4).

### 5. Floating-point/numeric policy

- Floating-point behavior must be documented. For the initial single-platform replay target, a same-build replay must reproduce identical canonical state hashes on the same supported platform and configuration. Fixed timestep alone is not determinism and is not presented as sufficient.
- Where exact accounting matters, avoid floating-point currency; use integer or fixed-point resource units with documented deterministic rounding (§9.2). Authoritative math must avoid reassociation-sensitive and fast-math-dependent operations; the chosen math library and compiler settings are part of the determinism contract.
- Cross-platform bit-identical lockstep (Windows and Linux) is a separate, future acceptance target. It must not be claimed until automated tests prove equivalence on both platforms with the chosen math and compiler settings, using deterministic numeric strategies for authoritative calculations or proven-equivalent operations. Lockstep multiplayer must not be promised before that proof exists (§19).

### 6. Parallel jobs

Parallel CPU jobs (rayon is permitted only under these rules) may write and reduce data only when the outcome is order-independent:

- Writes are partitioned so that no two jobs write the same authoritative data; no race-dependent writes.
- Reductions are deterministic: fixed-order or tree reductions with a deterministic combine order. Floating-point atomic accumulation is not a deterministic reduction.
- No order-dependent side effects, control flow, or nondeterministic iteration inside authoritative systems.
- The canonical state hash must be identical with and without parallelism; determinism tests must run fixed seeds and command streams both with and without the parallel executor (technology spike 5, §4.1).

### 7. CPU-only headless execution (hard requirement)

The complete authoritative simulation must initialize and run without creating a window, initializing a graphics API, requiring a physical or software GPU, or loading renderer-only assets (§3.3). Headless mode is a first-class runtime configuration — not a test-only stub — used for CI, replays, AI simulation, scenario validation, fuzzing, and performance tests. A separate optional headless-GPU mode may exist for GPU compute profiling but must not be confused with CPU-only headless simulation. Dependency direction is one-way: core simulation types must not depend on renderer, editor, windowing, or GPU crates, so headless builds cannot accidentally pull in graphics initialization (§5).

### 8. State-hash and replay expectations

- A canonical state hash can be produced for any tick. Hashing uses stable ordering and explicit serialization of authoritative fields and excludes pointers, padding bytes, GPU handles, renderer state, wall-clock timestamps, and other nondeterministic or transient data (§13.3). Hashing may be configurable in release builds but must be available in tests and diagnostic builds.
- A replay contains replay format version, simulation/game-rules version, initial map/scenario and content identifiers/hashes, player identity/configuration, deterministic RNG seeds and stream state, tick-indexed commands and other authoritative external inputs, and optional periodic state hashes/checkpoints (§13.2). All state-changing inputs are recorded in a command/event history.
- Replays support seeking through checkpoints or deterministic resimulation. A replay mismatch report identifies the first divergent tick and provides subsystem-level diagnostics where feasible.

### 9. Milestone scoping: required guarantees vs future acceptance targets

**Required for the first milestone (M0, extended through M1):** CPU ownership of all authoritative state; one authoritative representation per gameplay fact; tick-boundary command application with the explicit phase schedule; seeded deterministic RNG in authoritative state; stable iteration and tie-breaking; deterministic parallel-job rules with tests run both with and without parallelism; CPU-only headless execution on Windows and Linux; canonical state hash available in test and diagnostic builds; and, at M1, command recording plus replay reproduction with matching state hashes on the same supported build and platform.

**Future acceptance targets (not required now, and must not be claimed):** cross-platform bit-identical lockstep proven by automated tests on Windows and Linux; lockstep multiplayer; GPU-accelerated authoritative computation; and the server-authoritative multiplayer evaluation if strict determinism proves too costly (§19).

## Consequences

**Positive:**

- Bugs, battles, and fuzz cases are exactly replayable from recorded inputs; CI, replays, AI simulation, and perf tests run headless without a GPU.
- State ownership is unambiguous: the renderer, GPU compute, and derived caches are consumers or rebuildable views, so renderer/backend choices (ADR-0004) and ECS storage choices (ADR-0003) cannot change gameplay outcomes.
- A path to multiplayer is preserved without committing to lockstep.

**Negative / costs:**

- The GPU cannot be the source of truth initially, so 10k-unit performance must come from data-oriented CPU design; GPU acceleration is limited to derived/non-authoritative work until separately validated.
- Parallelism is constrained: jobs need disjoint writes and deterministic reductions, and every parallel path needs determinism tests with and without the parallel executor.
- Numeric discipline is required: fixed-point/integer resources, documented rounding, no fast-math-dependent authoritative math, and a validated math library.
- Phase-order and tick-schedule changes become expensive (simulation-version changes) because they can alter replay results and balance.
- Headless mode must be maintained as a first-class configuration; accidental graphics dependencies in core crates are architectural regressions.

## Revisit criteria

Revisit this ADR (by replacement ADR with migration and compatibility impact, per §17.2) when any of the following occurs:

- An automated test proves or disproves cross-platform bit-identical determinism on Windows and Linux with the chosen math and compiler settings.
- A GPU determinism design passes cross-device validation and GPU compute is proposed for authoritative paths.
- Profiling shows the determinism constraints make the 10k-unit target unreachable on reference hardware, prompting evaluation of a server-authoritative model (§19).
- The tick rate, numeric representation, or RNG design changes.
- New platforms, build configurations, or scripting in authoritative simulation are introduced.

## Open questions and validation experiments

Open questions:

- Final tick rate (20 ticks/second is the initial proposal).
- Numeric representation for authoritative math: floating-point (which precision) vs fixed-point/integer; validation of the glam math library and compiler settings (for example, disallowing fast-math and reassociation).
- RNG algorithm choice and stream layout (single stream vs per-system or per-entity streams) and how RNG state is hashed.
- Hash algorithm, canonical serialization format, and checkpoint cadence; cost of hashing at 20 ticks/second at scale.
- Parallelization granularity and reduction strategy for movement, combat, and navigation at 10k+ entities.
- Whether and when gameplay scripting enters authoritative simulation (deterministic execution, versioning, sandboxing, and budgets would then become mandatory, §4).

Validation experiments:

1. Run the CPU-only headless scenario on Windows and Linux (M0 exit criterion, §4.1 spike 3).
2. Determinism tests with fixed seeds and command streams, run with and without the parallel executor (§4.1 spike 5).
3. Replay round-trip and reproduction tests: record a scenario, replay it headless on the same build/platform, and require identical canonical state hashes (M1 exit criterion).
4. Cross-platform hash comparison experiment on Windows and Linux: informational until it becomes an automated, passing acceptance test; until then, no cross-platform bit-identical claim is made.
5. Profiling of state-hash and checkpoint overhead against the per-tick budget (§14.2).
