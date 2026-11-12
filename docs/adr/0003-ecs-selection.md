# ADR-0003: ECS Selection — hecs for the Initial Implementation Spike

- **Status:** Accepted (initial selection for the M1 slice; production commitment deferred — see Revisit criteria)
- **Date:** 2026-11-12
- **Related:** [RtsEngineDoc.md](../../RtsEngineDoc.md) §4 (technology stack), §4.1 (spike 4), §17.2 (ADR list); ADR-0001 (authoritative simulation and determinism); issue #9; benchmark source `benchmarks/ecs/`.

## Context

`RtsEngineDoc.md` §4 names `bevy_ecs` as the preferred first-spike ECS candidate and `hecs` as the comparison, and forbids building a custom ECS before measurements justify it. Issue #9 requires a measured comparison on representative RTS workloads at 10k/50k/100k entities, with the recommendation recorded as an ADR. The authoritative simulation (ADR-0001) needs stable iteration order, low memory overhead, and fast batch iteration at 10k+ units with frequent spawn/despawn churn; renderer integration is a separate decision (ADR-0004). Two facts must be documented explicitly:

- ECS table storage is **per-component columns, not automatically separate x/y/z field arrays**: a `Position { x, y, z }` component is stored as one column of `Position` structs (array-of-structs per component), so iterating `(&Position, &Velocity)` walks two columns of whole structs. Field-level (x/y/z) splitting is *not* automatic in either candidate and would require separate components or custom storage if vectorization demands it.
- No custom ECS and no duplicate authoritative position storage was introduced for this benchmark; the benchmark crate is detached from the production workspace (`benchmarks/ecs/` has its own `[workspace]` and is excluded in the root `Cargo.toml`) and is covered by a dedicated `bench-checks` CI job instead.

## Problem

Choosing an ECS on reputation or on the word "SoA" leads to the wrong trade-offs. The decision needs measured throughput, memory, and allocation data on workloads shaped like Phalanx (dense entity iteration, multi-component combat queries, churn), plus a qualitative assessment of ergonomics, scheduling, determinism, and integration cost.

## Benchmark setup and methodology (spike report)

- **Hardware:** Intel Xeon Platinum 8581C @ 2.30 GHz, 5 cores (4 visible to the process), ~6 GB RAM, Linux 6.18.35 x86_64 (GitHub-hosted-class runner).
- **Toolchain:** rustc/cargo 1.99.1 (2026-10-13), stable channel; `cargo build --release` with `opt-level = 3`, `debug = false` — identical profile for both candidates (single crate, both libraries linked).
- **Candidates:** `bevy_ecs 0.20.1`, `hecs 0.11.2` (locked in `benchmarks/ecs/Cargo.lock`).
- **Workload** (`benchmarks/ecs/src/main.rs`, seed 42, 20 measured rounds after 2 warmup rounds per entity count): entities are 80% units `(Position, Velocity, Health, Attack)`, 10% armored units `(…, Armor)`, 10% buildings `(Position, Health, Building)`. Benches: `spawn` (batch insert), `iterate_update` (`pos += vel * dt` over `(&mut Position, &Velocity)`), `query_movement_read`, `query_combat_wounded` (`(&Position, &Health, &Attack)` with health filter), `query_movement_no_building` (filtered), `churn_spawn_despawn` (despawn+respawn ~1% per round).
- **Symmetric query lifecycle (review fix):** both implementations construct their query handle *inside every measured round*, so query-construction costs are paid identically. The churn id-scan is per-round in both because the world mutates between rounds.
- **Throughput is visit-based (review fix):** each bench counts the items actually iterated (`visits`) and items passing its filter (`matched`); reported rates are visits/sec and matched/sec, not total-entities/sec.
- **Statistics (review fix):** 5 independent processes per candidate, interleaved (bevy, hecs, bevy, hecs, …) to spread machine-state drift; tables report the **median**, with p95 in parentheses where shown. Single-run figures are not treated as definitive; a Criterion-based harness is a candidate for the M3 rerun.
- **Memory/allocation methodology:** a counting global allocator records allocator *calls* and *requested* bytes (live and peak). These are allocator-accounted metrics, **not process RSS** and not peak system memory; allocation counts are global-allocator calls, not internal ECS bookkeeping.
- Checksums of deterministic results matched exactly across candidates, confirming identical workloads.

### Results (median of 5 interleaved processes; visits/sec = entity-visits per second actually iterated)

| entities | bench | bevy_ecs 0.20.1 | hecs 0.11.2 |
|---|---|---|---|
| 10k | spawn | 0.70 ms (14M v/s) | **0.43 ms (23M v/s)** |
| 10k | iterate_update | 0.018 ms (489M v/s) | 0.018 ms (501M v/s) |
| 10k | movement_read | 0.019 ms (470M v/s) | **0.005 ms (1.69B v/s)** |
| 10k | combat_wounded | **0.033 ms (270M v/s, 90k matched/round)** | 0.047 ms (193M v/s) |
| 10k | movement_no_building | 0.019 ms (468M v/s) | 0.018 ms (490M v/s) |
| 10k | churn | 0.040 ms | **0.031 ms** |
| 50k | spawn | 3.26 ms (15M v/s) | **2.25 ms (22M v/s)** |
| 50k | iterate_update | 0.095 ms (474M v/s) | 0.101 ms (445M v/s) |
| 50k | movement_read | 0.092 ms (490M v/s) | **0.030 ms (1.51B v/s)** |
| 50k | combat_wounded | **0.202 ms (223M v/s)** | 0.241 ms (187M v/s) |
| 50k | movement_no_building | 0.092 ms (489M v/s) | 0.093 ms (484M v/s) |
| 50k | churn | 0.236 ms | **0.179 ms** |
| 100k | spawn | 5.46 ms (18M v/s) | **3.83 ms (26M v/s)** |
| 100k | iterate_update | 0.206 ms (437M v/s) | 0.195 ms (462M v/s) |
| 100k | movement_read | 0.187 ms (482M v/s) | **0.069 ms (1.31B v/s)** |
| 100k | combat_wounded | **0.421 ms (214M v/s, 897k matched/round)** | 0.482 ms (187M v/s) |
| 100k | movement_no_building | 0.188 ms (480M v/s) | 0.187 ms (482M v/s) |
| 100k | churn | 0.532 ms | **0.408 ms** |

Memory (allocator-accounted, 100k entities, median of 5 runs):

| phase | bevy_ecs 0.20.1 | hecs 0.11.2 |
|---|---|---|
| after spawn | 12.6 MB live / 18.6 MB peak / 4.7k alloc calls | **6.1 MB live / 10.7 MB peak / 0.6k alloc calls** |
| after churn | 22.4 MB live / 22.7 MB peak / 6.7k alloc calls | **9.7 MB live / 12.9 MB peak / 0.9k alloc calls** |

(bevy_ecs's alloc-call count includes the per-round `QueryState` construction now paid symmetrically by both candidates.)

Reproduce: `cd benchmarks/ecs && cargo run --release -- --candidate bevy|hecs --rounds 20 --seed 42` (repeat for multiple processes; interleave candidates).

## Alternatives considered

1. **bevy_ecs 0.20.1** — rich scheduler/app ecosystem and a familiar API, but ~2× the live memory, ~7× the allocation calls, slower spawn and churn, and ~2.7–3.5× slower read-only multi-component queries at scale even with symmetric query construction (so the read advantage is *not* a measurement artifact); its multi-threaded executor also needs explicit ordering for determinism. Slightly faster on the write-heavy 3-component combat query (214M vs 187M v/s at 100k).
2. **hecs 0.11.2** — minimal, fast, low-allocation; wins spawn, churn, read queries, and memory at every scale; slightly slower on the write-heavy 3-component combat query. Iteration order is deterministic (archetype + entity order), which ADR-0001 requires us to enforce explicitly anyway via stable IDs and explicit system ordering.
3. **Custom ECS** — rejected per `RtsEngineDoc.md` §4 until measurements justify it; the benchmark shows both candidates are comfortably fast enough (hundreds of M visits/sec) that a custom ECS is not justified.

## Decision

**Select hecs 0.11 as the ECS for the initial implementation spike and the M1 vertical slice.** The RTS workload shape (dense iteration, frequent churn, low memory overhead at 10k–100k entities) is where hecs measured clearly ahead, while bevy_ecs's advantage is confined to one query shape and its ecosystem (app/scheduler/plugins) is not needed for the CPU-authoritative simulation, which owns its own fixed-tick schedule (ADR-0001, §6.1). Per the issue's acceptance criteria, this is a spike-driven *recommendation for the initial implementation*, not a production dependency lock-in: the production commitment is confirmed only after real gameplay workloads at M3.

Determinism rules for integration (binding with ADR-0001): hecs iteration order is deterministic but must still be made outcome-independent where order matters — the simulation sorts commands by command ID, uses stable logical `EntityId`s (not hecs `Entity` handles, which encode generation and are not exposed as persistent identifiers), and parallel jobs keep disjoint writes and deterministic reductions. hecs `Entity` handles are used only as transient storage keys inside the simulation, never in commands, replays, saves, or the agent API.

## Consequences

**Positive:** lower memory and allocation pressure at 10k+ units; faster spawn/churn (relevant for unit production/destruction); faster read queries (target acquisition, visibility); smaller dependency tree (matters for `cargo audit` surface and build times); deterministic iteration by default.

**Negative / costs:** no built-in scheduler — the fixed-tick phase schedule from ADR-0001/§6.1 is ours to own (already implemented in `phalanx_core::simulation`), which is arguably a better fit than bevy's generic scheduler for a deterministic lockstep-style sim. bevy_ecs's slightly faster 3-component combat query is accepted as a trade-off; if profiling at M3 shows combat queries dominating, revisit. hecs has a smaller ecosystem (no hierarchy/plugin crates needed now). Benchmark numbers come from a shared virtualized runner; absolute figures will differ on reference hardware.

## Revisit criteria

Revisit (replacement ADR with migration impact) when any of the following occurs:

- Real gameplay workloads at M3 (mass combat, mass movement, 10k-unit full battlefield) show hecs missing the performance budget where bevy_ecs (or another candidate) would not — measured with the same harness extended to real systems, ideally with Criterion.
- Cross-platform results (Windows + Linux reference hardware) contradict the Linux numbers above — rerun `benchmarks/ecs` on both OSes and on named reference hardware before M3, with multiple interleaved processes and dispersion reported.
- hecs maintenance/API stability becomes a problem, or bevy_ecs's ecosystem (e.g., required editor/render integration) becomes necessary.
- A demonstrated need for field-level SoA (separate x/y/z columns) for vectorization that neither candidate provides ergonomically.

## Open questions and validation experiments

- Rerun the benchmark on Windows and on named reference hardware with multiple interleaved processes; compare medians and dispersion against these Linux numbers.
- Extend the harness with real gameplay systems (movement resolution, combat at 10k units) once `phalanx_game` exists, and re-measure, preferably under Criterion.
- Measure hecs with parallel iteration only under ADR-0001's deterministic parallel-job rules, with and without parallelism producing identical state hashes.
- Confirm hecs `Entity` reuse/generation behavior aligns with our `EntityId` allocator semantics (stale-handle rejection) during M1 integration.
- The benchmark crate is kept compiling by the `bench-checks` CI job (fmt, clippy `-D warnings`, test, `build --release` in `benchmarks/ecs/`).
