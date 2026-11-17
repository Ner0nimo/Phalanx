# ADR-0010: High-Fidelity Rendering and Temporal Upscaling

- **Status:** Accepted direction and constraints; native vendor-effect integration and final renderer-backend commitment remain provisional.
- **Date:** 2026-11-17
- **Related:** [RtsEngineDoc.md](../../RtsEngineDoc.md) §§4, 5, 11, 14, 15; [ADR-0004](0004-rendering-backend.md); issues #96–#102.

## Context

Phalanx is intended to ship as a visually polished 3D RTS, not merely a functional simulation with debug geometry. The game needs to render thousands of units while preserving strategic readability, tactical detail, stable frame pacing, and high image quality. The long-term target includes modern temporal upscaling such as AMD FSR 4 where the player hardware and platform support it.

The current repository has a real but early presentation path:

- `phalanx_render` uses `wgpu = 30.0.1` and `winit = 0.30`.
- The current backend requests all compiled wgpu backends, enables validation, and requests no optional wgpu features beyond defaults.
- The visible scene is still primarily debug geometry: 2D colored shapes, unit/obstacle rectangles, and HUD/debug overlays. The offscreen target is currently `Rgba8Unorm`.
- The render snapshot exposes presentation-oriented positions, team/category/radius and map obstacles, not production mesh/material/animation data.
- The performance overlay measures CPU-side encoding/submission but does not yet measure GPU execution with timestamp queries.
- There is no production render graph, HDR scene-color path, depth/motion-vector pipeline, cooked material/texture pipeline, or temporal history management.

These are reasonable milestones for proving native presentation and a graphical battle, but they do not prove that the renderer is ready for high-fidelity production content or native vendor SDK effects.

## Current external constraints (verified 2026-11-17)

AMD's current **FSR SDK 2.4** (October 2026) includes FSR Upscaling 4.1.2, FSR Frame Generation 4.0.2, FSR Frame Generation (Swapchain) 4.0.0, and FSR 3.1.6 fallback technologies. The SDK provides DX12 and Vulkan 1.2 integrations for upscaling and frame generation. AMD requires applications to use the prebuilt, signed FSR API loader/effect DLLs.

FSR Upscaling 4.1.2 supports selected Radeon RX 6000/7000/9000-series GPUs and supported Ryzen APUs. The FSR API selects FSR 3.1.6 on hardware that does not support the ML upscaler. AMD's published supported-OS list for this SDK is Windows 10 and Windows 11. Vulkan support must therefore **not** be interpreted as proof of an officially supported Linux FSR 4 runtime; Linux support must be independently verified before it is promised.

FSR Frame Generation 4.0.2 is more restrictive: AMD documents Windows 11 and Radeon RX 9000-series GPUs or later, with DX12 or Vulkan 1.2. AMD's guidance calls for at least 30 FPS before interpolation for this generation path. Frame generation is an optional feature, not a substitute for a healthy base frame rate or responsive simulation.

FSR Upscaling is temporal, not a simple final-image resize. Its integration contract includes linear-color input, depth, motion vectors, and a correctly reported camera jitter. Motion data should cover opaque, alpha-tested, and alpha-blended objects. Exposure is optional when using the SDK's automatic-exposure path. FSR 4 no longer requires reactive and transparency/composition masks, although resource requirements must be queried for the actual effect/version selected.

The wgpu high-level API does not provide a generic stable safe handle API for arbitrary external SDKs. Native backend access is exposed through unsafe wgpu/wgpu-hal escape hatches; wgpu-hal explicitly places resource ownership and transition responsibilities on the caller. Integrating an external SDK therefore requires a deliberate native-resource, queue, lifetime, and synchronization design, not simply passing a wgpu texture view to the SDK.

## Decision

### 1. Keep wgpu as the working renderer baseline, not as a proven FSR integration

Continue the renderer implementation on wgpu while the native interop spike is performed. The existing wgpu choice remains justified for safe resource management, validation, and Windows/Linux portability. It is **not yet proven** that the current wgpu device, queue, textures, and presentation path can be shared with the AMD FSR SDK efficiently and safely.

The FSR integration spike must prove a same-device, no-expensive-copy path for the selected Windows backend(s), including all required barriers/transitions, resource lifetime, queue synchronization, and presentation. If that cannot be achieved within a small, auditable boundary, compare the cost of a dedicated native backend boundary or a renderer-backend change before the production renderer grows further. Do not switch to raw Vulkan/ash solely on the assumption that it will be faster.

The renderer should choose an explicit platform/backend capability profile. `Backends::all()` is appropriate for discovery/prototyping, but production effect selection must know which native API is active and whether the required device features, texture formats, and SDK runtime are available.

### 2. Make temporal reconstruction a first-class renderer contract

The renderer must represent render resolution separately from display/output resolution. Its render graph must be able to provide the upscaler's required inputs without ad-hoc copies or hidden conventions:

- Linear scene color in a documented HDR working space.
- Depth with an explicit convention (including inverted/infinite-depth flags where relevant).
- Motion vectors with documented coordinate space, scale, sign, and jitter convention. Opaque, alpha-tested, alpha-blended, instanced, and animated objects must contribute valid motion wherever they cover pixels.
- Camera jitter and the exact offset used for the current frame.
- Exposure or the selected effect's automatic-exposure mode.
- Explicit history lifecycle: initialization, reset on camera cut/map load/resolution change, and invalidation on discontinuities.

The render graph owns resource formats, lifetimes, usages, pass dependencies, and transitions. A generic upscaler interface must not erase the information needed for native interop or make synchronization implicit.

### 3. Keep upscaler selection capability-based and vendor-neutral

FSR 4 is an optional, high-quality path when the platform, runtime, driver, and GPU support it. It must not be a hard startup dependency, and lack of FSR support must never prevent a player from launching the game.

On supported Windows systems, investigate FSR Upscaling 4.1.2 first, with the FSR API's supported fallback behavior. On unsupported hardware, other vendors, or Linux configurations where the SDK runtime is not verified, use a tested fallback (for example, an available temporal upscaler or native-resolution temporal anti-aliasing). Do not claim that FSR 4 is available on Linux solely because the SDK exposes a Vulkan API.

The runtime must query effect capabilities and the required input resources for the actual SDK/effect version. Runtime selection should be based on detected support, not only on GPU vendor strings.

### 4. Treat frame generation as a separate optional feature

Do not block the first production renderer or the first FSR upscaler integration on frame generation. Only evaluate FSR Frame Generation 4.0.2 after the base renderer produces stable frames at its target rate and the upscaler path is validated.

Any frame-generation integration must test frame pacing, input latency, VRR/VSync behavior, camera motion, small moving units, particles, and UI composition. The RTS HUD, selection rings, text, and order markers must be kept sharp; use a supported separate UI-composition path or render UI after interpolation. Frame generation must remain separately toggleable and capability-gated.

FSR Ray Regeneration and Radiance Caching are not part of the current rendering target. Their DX12-only constraints (and Radiance Caching's preview status) do not justify constraining the initial renderer architecture.

### 5. Establish a real visual-quality baseline

The renderer roadmap must progress from debug shapes to a 3D scene with a coherent material model, linear-HDR lighting, tone mapping, shadows, terrain/material blending, model and animation LOD, instancing/batching, and an imported/cooked asset pipeline. Visual quality must be assessed at strategic, medium, and close zoom, under camera motion and in dense battles.

Image-quality and performance acceptance must be demonstrated on named GPU hardware and drivers. CPU encoding/submission time is not GPU frame time; add GPU timestamp queries and per-pass profiling before making performance claims.

### 6. Keep native unsafe code isolated

Keep the current workspace's safe-Rust policy as the default. If FSR/native API integration requires unsafe calls, isolate them in a very small, separately reviewed backend-interop boundary with documented safety invariants and tests. Do not allow raw Vulkan/D3D12 handles, unsafe code, or native synchronization assumptions to leak through the public presentation API. Any exception to the workspace unsafe lint must be explicit and narrowly scoped; do not weaken the policy globally as a convenience.

## Consequences

**Positive:**

- The wgpu renderer can continue evolving without pretending native FSR support is already solved.
- FSR 4 can be integrated where supported without making AMD hardware or Windows a hard requirement for the game.
- Linux remains a first-class target with an explicit fallback rather than an unsupported promise.
- Temporal reconstruction requirements influence the render graph and scene outputs before the renderer is locked into an incompatible design.
- Visual-quality work has measurable reference scenes and GPU profiling rather than relying on debug screenshots or CPU-only timings.

**Costs and risks:**

- A real FSR integration spike requires an actual compatible Windows GPU, AMD driver, SDK binaries, and careful resource synchronization.
- wgpu native interop may be version-coupled and may require a small unsafe boundary.
- Supporting multiple vendor/platform upscalers increases QA, packaging, and compatibility work.
- Frame generation adds UI composition, pacing, and latency complexity and may not be worthwhile for every RTS workload.

## Required validation and revisit criteria

1. Run FSR SDK 2.4 / FSR Upscaling 4.1.2 on a supported Windows Radeon GPU using the actual `phalanx_render` device and render resources. Record backend/API, adapter, driver, SDK DLLs, build profile, synchronization and transition behavior, GPU memory, timing, and image output.
2. Prove that the wgpu-to-FSR integration does not require a full-resolution CPU readback or expensive GPU copy between devices.
3. Compare the upscaled output against native resolution and the fallback path in representative RTS scenes: static formations, fast camera pan, zoom, small units, alpha-tested foliage, transparent effects, particles, and UI.
4. Verify behavior on unsupported AMD hardware, NVIDIA/Intel hardware, Windows without the SDK runtime, and native Linux. Fallback behavior must be tested, not assumed.
5. Capture GPU timestamps for geometry, lighting, shadows, post-processing, upscaling, UI, and presentation.
6. Revisit the wgpu backend decision if the native SDK path requires pervasive unsafe code, copies that erase the performance benefit, unsupported resource access, or unmanageable version coupling.

## References

- [AMD FSR SDK (current release, SDK 2.4)](https://gpuopen.com/amd-fsr-sdk/)
- [AMD FSR SDK 2.4 release details and GPU/driver support](https://gpuopen.com/learn/amd-fsr-sdk-2-4-rx-6000-ryzen-apus/)
- [AMD FSR Upscaling 4.1.2 integration and input-resource requirements](https://gpuopen.com/manuals/fsr_sdk/techniques/super-resolution-ml/)
- [AMD FSR Frame Generation 4.0.2 requirements](https://gpuopen.com/manuals/fsr_sdk/techniques/frame-interpolation-ml/)
- [AMD FSR Frame Generation UI-composition guidance](https://gpuopen.com/manuals/fsr_sdk/techniques/frame-interpolation-swap-chain/)
- [wgpu native backends and public API](https://wgpu.rs/doc/wgpu/)
- [wgpu-hal safety, resource, and synchronization contracts](https://wgpu.rs/doc/wgpu_hal/)
