# ADR-0004: Rendering Backend — wgpu for the Production Prototype

- **Status:** Accepted (production prototype direction; final commitment after headed GPU validation — see Revisit criteria)
- **Date:** 2026-11-12
- **Related:** [RtsEngineDoc.md](../../RtsEngineDoc.md) §4 (rendering candidate), §4.1 (spikes 1–2), §11 (rendering), §17.2 (ADR list); ADR-0001 (headless CPU-only simulation is independent of this decision); issue #10; spike source `benchmarks/graphics/`.

## Context

`RtsEngineDoc.md` §4 requires evaluating wgpu against ash before selecting, without building both production backends, and §4.1 requires spikes for rendering 10k instanced units and a compute workload on the candidate graphics API. Issue #10 requires the same narrow workload for both candidates, recorded native Windows/Linux behavior, a comparison of device/resource lifetime and synchronization complexity, shader workflow and validation, compute capability, and the recommendation in ADR-0004. The workspace lints forbid `unsafe_code` in workspace crates (`[workspace.lints.rust] unsafe_code = "forbid"`), which is a hard architectural constraint on any renderer we build in-tree.

## Problem

The renderer API choice becomes a large sunk cost once low-level graphics infrastructure exists. The decision must weigh safety (the workspace forbids `unsafe`), validation/debug tooling, shader workflow, resource lifetime and synchronization burden, compute support, dependency/build cost, and portability — not just frame time. The sandbox and CI runners have **no GPU and no Vulkan loader**, so raw frame time cannot be measured here; this spike measures everything that is measurable headless and records GPU-dependent items as explicit limitations.

## Spike setup (report)

- **Environment:** Intel Xeon Platinum 8581C, Linux 6.18.35 x86_64, rustc 1.99.1, no GPU (`/dev/dri` absent), no `libvulkan.so.1` — i.e., the same class of machine as GitHub-hosted runners. CI additionally runs both spike binaries headless on `ubuntu-latest` and `windows-latest` (new `graphics-spike` job), recording native init behavior on both OSes.
- **Candidates:** `wgpu 30.0.1` (safe, cross-platform; `benchmarks/graphics/src/wgpu_spike.rs`, 172 lines, **0 `unsafe` blocks**) and `ash 0.38.0+1.3.281` (raw Vulkan bindings; `benchmarks/graphics/src/ash_spike.rs`, 187 lines, **17 `unsafe` blocks**). Detached benchmark crate, excluded from the production workspace like `benchmarks/ecs`.
- **Shared narrow workload (planned identically in both binaries):** grid terrain-like mesh + 10,000 instanced unit markers (one instanced draw call, one `vec4<f32>` per instance = 160,000-byte instance buffer) + a 4 KiB storage-buffer compute update. **Execution asymmetry is deliberate and documented (review fix): the wgpu binary executes and result-verifies the compute dispatch when an adapter exists (no rendering — headless has no surface); the ash binary demonstrates initialization and resource/lifetime management only — no compute, no rendering, no SPIR-V. The comparison therefore covers initialization, API ergonomics, and lifetime/synchronization management — NOT performance or rendering capability.**

### Measured headless behavior (this machine; CI logs record the same on ubuntu/windows)

- wgpu on Linux (this sandbox): instance with `InstanceFlags::VALIDATION` created; adapter request failed gracefully: `No suitable graphics adapter found; … vulkan drivers/libraries could not be loaded …` → structured `ADAPTER found=false`, exit 0. Validation/debug layers are built into wgpu (no external layer install).
- wgpu on Windows CI (`windows-latest`, software adapter "Microsoft Basic Render Driver", CPU): device created, WGSL validated, and the compute dispatch **executed and result-verified** — CI log: `COMPUTE candidate=wgpu dispatched=true workgroups=64 buffer_bytes=4096 verified=true time_ms=18.831` (end-to-end CPU-adapter latency, not a discrete-GPU figure).
- ash: `ash::Entry::load()` failed gracefully on Linux: `libvulkan.so.1: cannot open shared object file` → structured `ADAPTER found=false`, exit 0; on CI runners the loader is present but `VK_LAYER_KHRONOS_validation` is absent (`available=false` on both OSes). Validation requires the external layer, which is absent on minimal systems.
- **Dependency/build cost:** wgpu tree ≈ 149 crates (first cold release build ≈ 43 s here; warm ≈ 1 s), ash tree = 3 crates (`ash`, `libloading`, `cfg-if`, ≈ 1 s). Note wgpu's Vulkan backend itself embeds an `ash`-based HAL internally, but behind a safe API.
- **Compute:** wgpu path validated a WGSL compute shader at `create_shader_module` and builds a compute pipeline + dispatch (executed when an adapter exists); ash path requires precompiled SPIR-V via an external toolchain (glslangValidator/shaderc), not exercised headless. wgpu reports adapter compute limits (`max_compute_workgroup_size_x`, etc.) from the device.

### Comparison against the same planned workload (execution scope as above)

| aspect | wgpu 30.0.1 | ash 0.38 |
|---|---|---|
| safety | 100% safe API; 0 unsafe blocks in spike | every call `unsafe`; 17 unsafe blocks in spike; cannot be used in-tree without an unsafe boundary crate (workspace forbids `unsafe_code`) |
| validation/debug | built-in `InstanceFlags::VALIDATION` / GPU-based validation | external `VK_LAYER_KHRONOS_validation` layer, often absent |
| shader workflow | WGSL text, validated at module creation (naga) | GLSL → SPIR-V via external toolchain; no built-in validation |
| device/resource lifetime | tracked by wgpu; Rust ownership | caller-managed (buffers, memory, fences, barriers, swapchain) |
| synchronization | wgpu tracks resource transitions and queue sync | fully manual: fences, events, pipeline barriers, memory barriers |
| compute | WGSL compute pipelines, limits reported; dispatch executed and XOR result verified via read-back when an adapter exists | SPIR-V compute pipelines; manual descriptor/dispatch management; not executed in this spike |
| portability | Vulkan/Metal/DX12/WebGPU/WebGL from one codebase (Windows + Linux + web) | Vulkan only (Windows/Linux native; no Metal/web) |
| dependency/build cost | ~149 crates, ~43 s cold build | 3 crates, ~1 s build |
| headless error reporting | structured `RequestAdapterError`, graceful | graceful via `Entry::load()` error; panics possible if unchecked |

## Alternatives considered

1. **wgpu** — safe API (compatible with the workspace `unsafe_code = "forbid"` lint without a boundary crate), built-in validation, WGSL workflow, wgpu-managed lifetimes/synchronization, one codebase for Windows/Linux (and future web/Metal), larger dependency tree and an abstraction layer over Vulkan.
2. **ash** — direct Vulkan control with minimal overhead and a tiny dependency tree, but 100% unsafe API, manual lifetime/synchronization burden (the classic source of graphics bugs), external validation layer and SPIR-V toolchain, Vulkan-only. Incompatible with the in-tree unsafe ban unless wrapped in a dedicated unsafe crate.
3. **Build both production backends** — explicitly rejected by `RtsEngineDoc.md` §4.

## Decision

**Select wgpu 30 as the renderer API for the production prototype** (`phalanx_render` when it exists). This is an **engineering-risk decision, not a performance decision**: this spike demonstrates that wgpu is easier to integrate, validate, and manage safely, and does not demonstrate that wgpu outperforms raw Vulkan. The deciding factors are safety (the workspace forbids `unsafe`, and ash cannot be used in-tree without a bespoke unsafe boundary), built-in validation and WGSL shader validation, wgpu-managed resource lifetimes and synchronization, and portability across Windows/Linux from a single codebase. ash's direct-control advantage is real but not needed at this stage; its synchronization and lifetime burden is exactly the risk we want the safe layer to own while the engine is young. Headless simulation (ADR-0001) is unaffected: `phalanx_core` and the runtime have no graphics dependency, and the spike crate is detached from the production workspace.

GPU-dependent measurements (frame time for 10k instanced markers, compute dispatch time on real hardware, Windows vs Linux headed behavior) are **not** claimed from this environment — they are recorded as required follow-ups (see Revisit criteria). No production renderer is built twice; only the detached spike binaries exist.

## Consequences

**Positive:** safe renderer code inside the workspace lint policy; validation layers and shader validation available from day one; less hand-rolled synchronization; portable backend selection (Vulkan on Windows/Linux now, Metal/web later); clean separation from the headless simulation.

**Negative / costs:** ~149-crate dependency tree (build time, `cargo audit` surface — mitigated by the dedicated audit job and committed lockfiles); an abstraction layer whose overhead we have not yet measured on real GPUs; wgpu API churn across major versions (pin and review upgrades deliberately per AGENTS.md).

## Methodology notes

- **Timing semantics:** the wgpu spike's `time_ms` measures CPU encode + submit + GPU wait (end-to-end submission latency), not pure GPU execution time. Future performance comparisons should use GPU timestamp queries alongside CPU end-to-end measurements.
- **CI gate distinction:** the `graphics-spike` CI job is a *headless initialization gate* required on every platform (both binaries must exit 0 with structured results). A *GPU integration test* that verifies an actual compute result on GPU-equipped hardware is a separate future gate (see Revisit criteria), not a shared-runner gate.
- **Dependency cost:** the ~149 vs 3 crate difference is a build-time/audit-surface observation for now, not a long-term architectural cost by itself; runtime overhead, upgrade frequency, and vulnerability surface are the meaningful long-term metrics (tracked via the audit job and M3 measurements).

## Revisit criteria

Revisit (replacement ADR with migration impact) when any of the following occurs:

- Headed runs on named reference GPUs (Windows + Linux) show wgpu missing frame-time or memory budgets for the 10k-unit workload where raw Vulkan would not — measured with the spike extended to real present/swapchain rendering and the instanced draw.
- A concrete need for Vulkan-specific extensions or zero-overhead control that wgpu does not expose (evaluate `wgpu-hal` escape hatch before switching to ash).
- wgpu maintenance/version churn becomes costly, or its dependency tree becomes a security/build burden.
- Web/console targets become in scope and the backend matrix needs re-evaluation.

## Open questions and validation experiments

- Run the spike headed on Windows and Linux reference GPUs (the `graphics-spike` CI job currently records headless init behavior on both OSes; add a GPU-equipped runner or manual validation environment) and record frame time for the 10k-instanced-marker draw and the compute dispatch, using GPU timestamp queries. The GPU gate must verify the compute result (read-back), not just initialization.
- Measure wgpu vs raw-Vulkan overhead on the same machine for the instanced draw to quantify the abstraction cost.
- Validate shader compilation diagnostics quality (WGSL errors) in CI via a shader-validation test once `phalanx_render` exists (§4: shader compilation and validation must be part of CI).
- Decide the cooked-shader pipeline: WGSL compiled at build time (e.g., via naga in a build script) vs runtime compilation, once real shaders exist.
