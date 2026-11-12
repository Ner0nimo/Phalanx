# Phalanx — RTS Engine

We're building a production-grade, large-scale real-time strategy (RTS) engine in **Rust** for **Windows and Linux (native builds)**. Target scale: Supreme Commander / Planetary Annihilation / Ashes of the Singularity — **10,000+ units per match** in a fully 3D world. Professional-grade software: stable, fully tested, real-world deployable. No hacks, no shortcuts — solid as a rock.

## Decisions / status (maintained as we go)

- Engine name: **Phalanx** (Cargo crates `phalanx_*`).
- Repo: https://github.com/Ner0nimo/Phalanx (public).
- Language: **Rust** (stable toolchain, Cargo workspace).
- Genre/scale: large-scale 3D RTS, 10,000+ units; free-moving RTS camera, full 3D terrain.
- Platforms: **Windows & Linux, native builds** via Vulkan on all platforms. Proton is only a distribution fallback (Steam), never part of the dev loop. *(Proposal — confirm.)*
- Multiplayer: **singleplayer + skirmish AI first**; networking is a later phase. The sim is designed deterministically (fixed timestep) from day one so lockstep can be added later.
- MVP scope: **core gameplay** — selection, command queue, resources, building, tech tree, combat, fog of war. The 10k-unit scale is a standing performance target built in from the start (SoA + GPU compute), not bolted on later.
- Headless: **optional, but recommended once cheap** — a `--headless` mode running the sim without window/GPU enables CI tests of 10k-unit scenarios. *(Proposal.)*
- Editor: **fully controllable by AI agents** — JSON-lines command API (`--commands file.jsonl`) including the map editor; "build me a skirmish on map X" must work without manual intervention.
- Dev-workflow standards: proposed below (unit tests everywhere, CI, AGENTS.md + skills, branch + PR flow). *(Proposal — confirm.)*

## Dev Workflow (proposal)

- Work autonomously; ask questions only when absolutely necessary.
- IMPORTANT: do not look outside the working directory or use other code on my computer as reference.
- Testing is of the utmost importance — lots of unit tests (`cargo test`), run automated tests before every commit; `cargo clippy -- -D warnings` and `cargo fmt --check` must be clean.
- CI (GitHub Actions): build + `cargo test` on `ubuntu-latest` and `windows-latest`; security gates: `cargo audit` + secret scan (gitleaks). (Can be extended as needed.)
- Provide a test map/skirmish that exercises every engine feature — all systems, the full command/scripting API, pathfinding, combat, fog of war, save/load.
- Do things properly: production-grade, not a hack project.
- Create `AGENTS.md` and relevant skills (`.vibe/skills/phalanx-dev`) with concrete development guidelines.
- Git: work on `vibe/<slug>` branches, open PRs to `main`, do a code review before merging; never push directly to `main`.

## Tech stack (Rust)

- **Vulkan via ash** for rendering **and** GPU compute — raw Vulkan bindings on Windows & Linux; shaders in GLSL compiled to SPIR-V at build time; compute also usable without a window (headless instance, no swapchain).
- **winit** for windowing and input.
- ECS: **bevy_ecs** (or **hecs**) — decide at the first milestone; needs SoA-friendly archetypes for 10k+ entities.
- Math: **glam**; CPU parallelism: **rayon**; serialization: **serde** (+ RON/JSON for maps, scenes, saves); assets: glTF via the **gltf** crate.
- Collision/world queries: **rapier3d** for static geometry; mass-unit local avoidance runs in GPU compute instead.
- Audio: **rodio**.
- Scripting hooks: **rhai** or **mlua** (Lua) for data-driven behavior; core AI in Rust. *(Open — decide at the first scripting milestone.)*
- Networking (later phase): QUIC via **quinn** with a lockstep protocol.

## Basic Architecture

- Cargo workspace: `phalanx_core` (ECS, fixed-timestep deterministic sim, orders, economy, combat), `phalanx_map` (terrain, navgrid/flow fields, map format), `phalanx_gpu` (Vulkan compute: unit steering/separation, flow-field pathfinding, fog-of-war visibility, projectile sim), `phalanx_render` (Vulkan renderer: instancing, LOD, terrain chunks), `phalanx_ai` (skirmish AI), `phalanx_audio`, `phalanx_editor` (window + UI + agent command API), `phalanx_runtime` (game executable), `tools/` (map editor, pak/exporter).
- **Fixed-timestep simulation** (e.g. 20 Hz) decoupled from rendering, with deterministic iteration order — prerequisite for future lockstep multiplayer.
- Data-oriented mass-unit handling: SoA storage, spatial-hash grid, rayon jobs on CPU; hot paths (steering, avoidance, flow fields, FoW) offloaded to GPU compute via Vulkan compute pipelines.
- Editor embedded in the runtime exe or standalone *(open)*; agent command API as JSON lines (`create_entity`, `spawn_unit`, `issue_order`, `save_map`, `run_commands`, ...).
- Export/packaging: pack map + assets + scripts into a distributable pak; the runtime runs the game without editor capabilities.

## Function design rules (the three rules)

Every function in Phalanx follows these three rules. When writing, reviewing, or
refactoring code, check each function against all three.

### 1. Fulfill your contract completely; never make callers compensate — and be defensive about what you consume

A function must do exactly what its name and contract promise: return a
complete, meaningful result, or explicitly report why it could not. Callers
must not have to compensate for incomplete operations, interpret undocumented
sentinel values, repeat validation, or repair inconsistent state.

At the same time, do not blindly trust inputs or dependencies. Validate
untrusted input at system boundaries, enforce invariants at mutation
boundaries, and trust established internal invariants — but still detect
violations instead of silently accommodating them. When something can fail,
report the failure explicitly; the recovery decision (propagate, retry,
degrade, substitute a documented fallback, abort) belongs to the highest layer
that has enough semantic context for it — not necessarily `main()`. An
explicit, documented fallback is fine; a concealed one is not. Never hide a
programming error behind a fallback.

- `World::set_parent` validates entities and rejects cycles itself, so callers
  don't duplicate those checks and `world_matrix` can trust the acyclic
  invariant (no magic `guard < 1024` loop). In Rust:
  `fn set_parent(&mut self, child: Entity, parent: Entity) -> Result<(), SetParentError>`.
- `Map::load` really loads INTO the map and preserves registered systems
  instead of silently replacing `self` with the deserialized value:
  `fn load(&mut self, path: &Path) -> Result<(), LoadError>`.
- Low-level functions return `Result<T, E>` and never swallow errors; the
  caller decides what to do with the `Err`. The GPU device probe lives in
  `main`/app setup because only the top level knows that a driver crash is
  unrecoverable there.
- Negative example: `fn load_texture(path: &Path) -> Texture { /* returns a
  default on missing file */ }` — the caller cannot distinguish the requested
  texture from the fallback. Return `Result<Texture, LoadError>` or
  `Option<Texture>` and let the asset-policy layer decide on placeholders.

### 2. Signatures should communicate the contract and minimize caller burden

The signature — name, parameter types, return type, plus docs and
preconditions — should communicate inputs, outputs, ownership, and failure
modes. Callers should not have to guess what special values mean, construct
unnecessary temporaries, or read the implementation to use a function
correctly. Pick the type that matches the semantics of the operation; do not
add wrapper types when a simpler type already expresses the contract:

- `Result<T, E>` when the caller needs to understand failure:
  `fn init(&mut self, headless: bool) -> Result<(), InitError>` — one value
  carries success and error: `if let Err(e) = audio.init(true) { log(e); }` —
  instead of `fn init(&mut self, headless: bool, err: &mut String) -> bool`.
- `Option<T>` when absence is an ordinary outcome: `fn ray_aabb(...) -> Option<f32>`
  instead of a `-1.0` sentinel.
- Return values instead of out-params: `fn read(path: &Path) -> Result<Vec<u8>, ReadError>`
  instead of `fn read(path: &Path, out: &mut Vec<u8>) -> bool`;
  `fn screen_ray(cam: &Camera, aspect: f32, x: f32, y: f32) -> Ray` instead of
  two out-params.
- `bool` when the outcome is genuinely yes/no; `()` when the operation cannot
  meaningfully fail. The goal is to make correct usage obvious and misuse
  difficult — not to maximize type complexity.

### 3. Keep each function at a consistent level of abstraction

A function should read like one coherent operation: its major steps expressed
at roughly the same level of conceptual detail, with lower-level details
delegated to meaningful, named functions. Do not mix high-level orchestration
with low-level implementation details in one body — but also do not extract
trivial statements into helpers just to make a function shorter.
Decomposition should improve understanding, not create indirection. The goal
is readability, not maximal decomposition.

- `Sim::update` iterates unit entities and delegates the per-unit work to
  `update_unit(entity, dt)`.
- `AudioSystem::update` = `stop_orphaned_sources(world)` +
  `update_source(entity, world)` per entity.
- `App::run` delegates to `init_*` helpers and `main_loop` instead of one
  400-line body mixing window, renderer, UI and event handling.

### Using the three rules together

The rules complement each other — do not satisfy one at the expense of
another: don't split functions into meaningless helpers for rule 3, don't
wrap every call in `Result` for rule 2, don't push recovery into an
inappropriate layer for rule 1. When in doubt, prefer explicit contracts,
meaningful abstractions, and the simplest design that preserves correctness.
Rule 1 protects correctness, rule 2 API quality, rule 3 maintainability.

## RTS gameplay systems

- Selection: box and circle select, multi-select, control groups (hotkeys), shift-queued commands.
- Orders: move / attack / patrol / guard / stop / build / train; right-click context orders.
- Pathfinding: A* on the navgrid for single units + flow fields for groups; local avoidance (RVO/ORCA-style) on GPU for 10k units.
- Fog of war: per-player visibility (unseen / explored / visible), GPU-computed; recon reveals.
- Economy: resource nodes, harvesters, income flow, build costs, unit caps.
- Building: footprint placement on a build grid, construction progress, rally points, production queues.
- Tech tree: research/upgrade unlocks gating units and buildings.
- Combat: weapon ranges, hitscan vs. projectiles, HP/armor/damage types, AoE splash, target acquisition, corpse cleanup; projectile sim on GPU at scale.
- Skirmish AI: build orders, state machine (economy → expand → attack), squad orders, difficulty levels; fully drivable via the agent command API.
- Maps: heightmap terrain (plateaus/water, PA-style), navgrid generation from terrain, resource nodes, spawn points; in-editor map editor (sculpt terrain, paint, place entities) controllable via commands.
- Camera: edge pan, WASD/arrows, zoom, rotate, hotkeys, jump-to-event.
- HUD/UI: minimap with FoW, selection panel, build menu, resource bar, alerts.

## 3D Renderer

- Vulkan (ash), PBR materials, glTF models; **GPU instancing + mesh/material batching** for 10k+ units; LODs + occlusion culling; terrain as chunked heightmap with splat blending.
- Selection rings, health bars, order markers as instanced quads; projectiles/explosions as billboards/particles.
- HDR + bloom optional — keep the frame budget for sim and GPU compute.

## Performance targets

- 10,000+ units simulated and rendered at 60 FPS on mid-range hardware.
- Sim-tick budget enforced by headless CI benchmarks (criterion): N units, M ticks, assert wall-clock time.
- Profile early and continuously; GPU compute passes benchmarked separately.

## Security trust model

Agent commands and scripts are treated as **untrusted input** even when supplied locally; the engine enforces capability boundaries rather than trusting the caller:

- Actor classes: local trusted editor owner (full access), AI-generated commands (validated, path-scoped, size/batch-limited), imported projects / external integrations (same validation; exported runtime builds deny host-filesystem paths entirely).
- Filesystem access modes: editor filesystem / project-root-scoped (symlink-safe) / package-only (a miss is an error, never a host-disk fallback).
- Resource budgets: command size/batch limits, map/save parse limits (depth/size/counts), script instruction and memory budgets.

## Open questions (revisit as we go)

- Native Linux as primary target vs. Proton-first distribution. *(Proposal: native.)*
- Headless mode: implement as soon as the sim/render split allows it cheaply.
- Scripting language: rhai vs. Lua (mlua).
- Editor embedded in runtime vs. standalone executable.
- Networking model (lockstep vs. server-authoritative) — post-MVP.
