# ADR-0009: Phase Schedule Justification, System Cadence, and Access Contract

- **Status:** Accepted
- **Date:** 2026-11-15
- **Related:** [ADR-0001](0001-authoritative-simulation.md) Decision §3 (tick-boundary command application and system-phase ordering) and §6 (parallel jobs); [RtsEngineDoc.md](../../RtsEngineDoc.md) §6.1 (tick schedule), §17.2; issue #36.

## Context

The 15-phase schedule in `RtsEngineDoc.md` §6.1 was declared before gameplay systems existed. Now the first real systems exist (`ScenarioSystem`, `MovementSystem`, the battle systems `BattleCommandSystem`/`BattleMovementSystem`/`BattleSystem`), so the schedule can be justified by actual dependencies instead of a speculative subsystem list. The open risks from issue #36: every system is considered on every tick, and every system gets unrestricted `&mut Simulation` access, which will make update frequencies, access conflicts, and deterministic parallel execution harder to introduce later — but the goal is not to build a complex scheduler prematurely, only to avoid hardening an unnecessarily restrictive API.

## Problem

Define, minimally and extensibly:

- Which phase boundaries are true ordering invariants (justified by real system dependencies).
- How systems with a deterministic multi-tick cadence are scheduled.
- How a system's state access is declared, and how authoritative data vs derived caches are distinguished.
- What constraints apply to future parallel execution and reductions.
- The version/replay policy for changing the schedule or cadence rules.

## Decision

### 1. Phase ordering: true invariants, justified by real dependencies

The schedule order is execution order and is versioned (`Phase::SCHEDULE`, canonical-encoded, part of the system manifest). The boundaries below are **true ordering invariants** because real systems depend on them:

- **Ingest/Apply commands before any gameplay update** — `IngestCommands`/`ApplyCommands` run first so every gameplay phase of tick N sees the commands arbitrated for tick N (issues #42/#43). `BattleCommandSystem` applies commands in `ApplyCommands`.
- **Movement resolution before combat targeting** — `ResolveMovement` runs before `ResolveCombat` because `BattleSystem` acquires targets from the positions produced by movement (`BattleMovementSystem` in `ResolveMovement`).
- **Combat before projectiles/damage** — `ResolveCombat` before `AdvanceProjectiles` before `ApplyDamageAndDeath`, so damage application sees this tick's combat results.
- **Events after the systems that produce them, invariants after events, hash last** — `EmitEvents` → `ValidateInvariants` → `ProduceStateHash`, so the checkpoint digest at phase 15 covers everything the tick produced.

Phases between these invariants (economy, production, navigation requests, intent, territory, …) currently have no registered systems; they remain as versioned schedule slots so inserting real systems later does not reorder existing ones. Adding, removing, or reordering phases is a simulation-version change: it requires an ADR, a `SIMULATION_SCHEMA_VERSION` bump where canonical state or execution semantics change, and replay rejection of old recordings (manifest digest + schema version, §13.2).

### 2. Deterministic cadence (implemented, issue #36)

- `SimulationSystem::cadence() -> u64` (default 1): `run` executes only on ticks where `tick % cadence == 0`. Cadence 0 is rejected at registration (`SystemRegistrationError::InvalidCadence`).
- Cadence gates `run` only. `validate_command` is **not** cadence-gated: command validation is a per-command contract at every tick boundary, not a scheduled update.
- Cadence is part of the finalized `SystemManifestEntry` (canonical, digest-compared in replay), so changing a system's cadence is a versioned, replay-visible transition.
- Semantics are pure functions of the tick: no wall-clock, no "catch-up" runs; a cadence-3 system simply does not run on other ticks.

### 3. System access contract

- Systems receive `&TickContext` (issue #37) and `&mut Simulation<P, E>`: the simulation is the single owner of authoritative state (issue #34 state-ownership contract), and the runner is single-threaded, so unrestricted mutable access has **no access conflicts today**. The API is deliberately not hardened into per-system borrow scopes now.
- A system that keeps state in `self` must treat it as authoritative (`write_canonical_state`/`restore_canonical_state`); rebuildable caches are the documented exception (rebuilt from authoritative state, excluded from the canonical encoding, never consulted for an outcome first).
- **Credible path to access-conflict detection and parallel execution (deferred, not yet enforced):** when a parallel executor lands, each system will additionally declare the state it reads/writes (keyed by its `SystemId` in the manifest, like `phase`/`order`/`cadence`); jobs then require disjoint write sets, and every reduction must combine in a fixed documented order (ADR-0001 §6). The canonical checkpoint digest must be identical with and without parallelism, with determinism tests run both ways. Until then, the single-threaded runner is the contract, and no API shape precludes the declarations (the manifest is the natural home).

### 4. Version/replay policy

- Changing `Phase::SCHEDULE`, per-system `(phase, order)`, or `cadence` changes behavior: it requires an ADR and is replay compatibility metadata — the manifest digest is compared at replay and old recordings are rejected (`ReplayError::IncompatibleSystemManifest`), and canonical-state changes bump `SIMULATION_SCHEMA_VERSION` so old recordings/snapshots are rejected at the boundary.

### 5. Intentionally deferred (documented, not built)

- Per-system read/write access declarations and their enforcement.
- A parallel executor and automatic conflict detection.
- Dynamic phase insertion/removal at runtime (registration is frozen after the first tick, issue #35).
- Budgets/quotas per system per tick (navigation already has a self-imposed planner budget; a runner-level budget API is deferred until profiling justifies it).

## Consequences

- The schedule is now justified by the movement/combat dependencies rather than a speculative list, and cadence gives systems a deterministic multi-tick mode without a scheduler.
- The API stays non-restrictive (full `&mut Simulation`), so nothing blocks future access declarations or a parallel executor; the manifest already carries the per-system metadata such a design needs.
- Every scheduling change is versioned and replay-visible, so replays cannot silently change meaning.

## Revisit criteria

- Revisit when the first parallel executor or per-system access declarations are designed (the manifest extension point is ready), when profiling justifies per-system budgets, or when gameplay needs phases not in the current 15.
