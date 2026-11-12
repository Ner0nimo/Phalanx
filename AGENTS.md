# AGENTS.md — Phalanx repository instructions

These instructions apply to all coding agents working in this repository. They govern how work is performed; they do not replace the product, gameplay, or architecture contracts.

## Required context

Before changing code or documentation:

1. Inspect the relevant source, tests, and current repository state.
2. Read the applicable sections of [RtsEngineDoc.md](RtsEngineDoc.md).
3. Read relevant Architecture Decision Records in `docs/adr/` and subsystem documentation.
4. Identify the authoritative owner of each piece of state and the public contracts affected by the change.

Treat `RtsEngineDoc.md` as the source of truth for product direction, architecture invariants, subsystem responsibilities, simulation semantics, data models, and open technical decisions. Do not silently convert an open proposal into a binding decision.

## Engineering principles

### Function and API design

1. **Fulfil contracts completely.** Validate untrusted input at system boundaries, enforce invariants at mutation boundaries, and report failures explicitly. Do not hide programming errors behind silent fallbacks.
2. **Make signatures communicate semantics.** Use `Result<T, E>` for meaningful failure and `Option<T>` for ordinary absence. Avoid sentinel values, ambiguous return values, and unnecessary out-parameters.
3. **Keep a consistent abstraction level.** Decompose for clarity, not to maximize helper count. Separate high-level orchestration from low-level implementation details.
4. **Preserve data-oriented performance.** Hot systems should process batches or columns where appropriate. Avoid needless per-entity calls, allocations, dynamic dispatch, or abstraction layers that inhibit vectorization, cache-friendly iteration, or deterministic parallel execution.
5. **Make state ownership explicit.** Do not create independently mutable duplicate representations of authoritative state. Derived caches must have documented ownership, synchronization direction, and invalidation/rebuild rules.
6. **Make side effects visible.** Keep mutation responsibilities clear. Avoid hidden global state, implicit I/O, and wall-clock dependencies in authoritative simulation code.
7. **Design for testability.** Keep core logic testable without a renderer, window, GPU, or external service wherever practical.

### Error handling and trust boundaries

- Treat agent requests, imported assets/projects, maps, savegames, and other external data as untrusted input.
- Validate values and references at boundaries; return structured, actionable errors.
- Do not silently ignore invalid commands, failed operations, or invariant violations.
- Do not use placeholder implementations that appear complete. Mark unsupported behavior explicitly.
- Never weaken, delete, skip, or suppress a test merely to make a change pass.

### Performance and determinism

- Profile representative workloads before introducing custom storage, caches, parallelism, or complex abstractions.
- Preserve the authoritative simulation's deterministic ordering and state ownership contracts in `RtsEngineDoc.md`.
- Do not introduce wall-clock reads, nondeterministic iteration, race-dependent writes, or untracked randomness into authoritative simulation.
- Measure performance changes where meaningful and report the workload and environment; do not claim improvements without evidence.
- Keep renderer, GPU, and presentation concerns out of CPU-only headless simulation paths.

## Dependencies

- Prefer maintained dependencies and consult official documentation.
- Justify new dependencies; do not add a crate for trivial functionality without a clear benefit.
- Review platform support, security posture, license, maintenance activity, compile-time/runtime cost, and compatibility with the architecture.
- Pin or constrain versions deliberately and review major upgrades.
- Do not introduce a production backend or replace a foundational technology based only on preference; use a focused prototype and record consequential choices in an ADR.
- Keep dependency changes scoped to the task.

## Tests and quality gates

Run the narrowest relevant tests while iterating, then the applicable workspace checks before requesting review.

Expected repository quality gates (as the workspace becomes available):

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- Native build checks for supported Windows and Linux targets.
- Headless smoke tests for simulation/runtime changes.

Add or update tests for changed behavior. Select test categories appropriate to the change: unit, integration, determinism/replay, save/load, property-based, fuzz, navigation, supply, combat, stress, or GPU validation. Do not report a check as passing unless it was actually run successfully. If a check cannot be run, state why.

## Architecture decisions and documentation

- Follow accepted ADRs. Do not silently override or work around an accepted decision.
- If new evidence challenges an ADR or a binding architecture contract, explain the conflict and propose an ADR update or replacement, including migration and compatibility impact.
- Record meaningful architectural choices in `docs/adr/` using context, problem, alternatives, decision, consequences, and revisit criteria.
- Update documentation when public APIs, data schemas, subsystem contracts, or observable behavior change.
- Keep architecture and gameplay specifications in `RtsEngineDoc.md` or the relevant subsystem document. Keep this file focused on agent/contributor behavior; link to specifications rather than copying them.

## Repository workflow

- Work on a task-specific branch (for example, `docs/<slug>`, `feature/<slug>`, or `fix/<slug>`).
- Open pull requests against `main`; do not push directly to `main`.
- Keep changes focused, reviewable, and independently testable.
- Inspect the diff before committing and avoid unrelated formatting or generated-file churn.
- Do not search unrelated personal files or other projects on the developer's machine. Access official documentation, toolchain documentation, and source/documentation for project dependencies when needed.

## Completion report

For every substantial task, report:

- Implemented behavior and important design choices.
- Changed files and public API/schema changes.
- Tests and checks actually executed, including failures or omissions.
- Performance impact or measurements, when relevant.
- ADRs added or changed.
- Known limitations and remaining work.

Do not claim completion for behavior that is only stubbed, untested, or dependent on an unresolved decision.
