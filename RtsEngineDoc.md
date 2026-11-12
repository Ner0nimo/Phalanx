# Phalanx — Large-Scale RTS Engine & Game Architecture

**Status:** Architecture baseline — living document  
**Repository:** https://github.com/Ner0nimo/Phalanx  
**Language:** Rust (stable)  
**Platforms:** Windows and Linux, native builds  
**Primary target:** A large-scale 3D real-time strategy game built around operational warfare, territorial control, logistics, production, and battles involving thousands of units.

This document is both a game-design direction and an engineering contract for human developers and coding agents. It distinguishes binding decisions from choices that require prototypes. Agents must not silently turn an open question into a permanent architectural commitment.

---

## 1. Product vision

Phalanx aims to become an original spiritual successor in the large-scale RTS space occupied by Ashes of the Singularity (2016). It should capture the appeal of commanding enormous battles across a broad strategic battlefield without reproducing Ashes' proprietary content, exact faction designs, unit roster, maps, UI, terminology, or balance. Phalanx must establish its own setting, factions, mechanics, visual identity, and economic model.

The target experience is not simply a conventional RTS with a high unit cap. The defining experience is commanding a war across a large battlefield:

- Multiple armies operate across a broad, readable strategic map.
- Players capture and hold strategically meaningful regions and infrastructure.
- Front lines emerge from territorial control, chokepoints, terrain, and force concentration.
- Supply, logistics, production capacity, and resource flow constrain what an army can sustain.
- Reinforcements travel from production areas to active fronts; losing routes and hubs has strategic consequences.
- Battles contain hundreds or thousands of participating units while remaining legible at strategic and tactical zoom levels.
- The player issues high-level orders and shapes the battle rather than micromanaging every individual unit.
- Scouting, intelligence, vision, and uncertainty matter.
- The game rewards operational planning: where to attack, what to defend, which routes to secure, when to reinforce, and where to create local superiority.

The setting is deliberately undecided. It may be science fiction, near-future, alternate history, or another setting that supports large mechanized formations and strategic warfare. The engine and data model must not depend on a specific lore or visual theme.

### 1.1 Design pillars

1. **Scale with legibility:** large battles must be understandable, not merely technically possible.
2. **Operational depth:** territory, supply, reinforcement, production, and positioning matter as much as direct combat.
3. **Meaningful geography:** terrain, routes, chokepoints, elevation, and infrastructure shape strategy.
4. **Command at multiple levels:** individual units, squads, formations, and armies have distinct use cases.
5. **Responsive control:** issuing a command should produce understandable feedback and predictable behavior.
6. **Emergent front lines:** the game should create recognizable areas of pressure, defense, breakthrough, and retreat.
7. **Reproducible simulation:** bugs and battles can be replayed exactly on supported deterministic builds.
8. **Agent-friendly development:** every subsystem has explicit contracts, tests, diagnostics, and machine-readable interfaces.

### 1.2 Non-goals for the first playable version

- No promise of 10,000 fully detailed, independently animated combatants on every minimum-spec machine.
- No seamless planetary-scale simulation until a vertical slice proves the need and feasibility.
- No mandatory online multiplayer in the first release milestone.
- No full rigid-body physics simulation for every unit.
- No user-facing scripting language before the data-driven gameplay model is stable.
- No procedural campaign, narrative system, or sophisticated diplomacy in the initial core.
- No requirement that every visual or simulation workload run on the GPU.

---

## 2. Product and gameplay model

### 2.1 Battlefield scale

The world is a large 3D battlefield represented at several levels of detail:

- **Strategic level:** regions, control points, resource sites, supply routes, bases, fronts, and army-level intent.
- **Operational level:** groups, formations, routes, staging areas, reinforcement flows, and local objectives.
- **Tactical level:** individual units, weapon ranges, line of sight, local movement, target selection, and immediate combat.

The same world state must support all three views. Strategic overlays are derived from authoritative world data and cached or aggregated where appropriate.

### 2.2 Territory and regions

The map should be partitionable into strategically meaningful regions. A region may contain terrain, resource sites, build sites, infrastructure, and one or more control objectives.

Region ownership/control is a gameplay concept, not merely a visual overlay. The architecture must support:

- Neutral, contested, friendly, and enemy control states.
- Capture or contest progress where required by game rules.
- Multiple capture or influence sources.
- Region adjacency and traversability.
- Strategic value and resource/infrastructure associations.
- Visibility and intelligence state per player.
- Region-level events for UI, AI, and strategic planning.

Do not assume every map is a simple rectangular grid of equally valuable capture points. The region graph is a logical strategic layer that can coexist with continuous world-space movement.

### 2.3 Logistics and supply

Logistics is a first-class gameplay system, not a late UI feature.

The architecture must be able to represent:

- Supply sources (bases, depots, factories, or setting-appropriate infrastructure).
- Supply consumers (units, formations, buildings, or regions).
- A transport/supply graph over roads, lanes, links, or traversable connections.
- Capacity, throughput, distance/cost, ownership, and disruption.
- Supply reachability and supply state.
- Local stockpiles or resource buffers if the design requires them.
- Reinforcement routes from production sites to armies/fronts.
- The effect of severed or contested routes.

The exact rules—binary, continuous, resource-specific, or abstracted supply—are open game-design decisions. The data model must not hard-code one interpretation prematurely.

Supply calculations should be deterministic and budgeted. Expensive network recomputation should be incremental or scheduled, not performed from scratch for every unit every tick. The system must expose debug views for graph connectivity, capacity, bottlenecks, and consumer supply state.

### 2.4 Armies, formations, squads, and units

An **army** is a player-facing and AI-facing operational grouping. A **formation** describes a movement/positioning arrangement. A **squad** is a tactical grouping. A **unit entity** is an individual simulation actor.

These concepts must not be conflated:

- Units remain individually represented where their individual state matters.
- Squads and formations provide group orders, slot assignment, cohesion, and movement intent.
- Armies aggregate operational objectives, composition, supply status, and strategic location.
- Membership may change during play; destroyed, detached, or reassigned units must not leave stale references.
- A unit may be temporarily unassigned or attached to a different group.
- Group commands must define how exceptions are handled (immobile, blocked, out of supply, under attack, unable to reach destination).

The first implementation may use simple formation patterns (line, column, loose, defensive) and basic group cohesion. The architecture should allow later specialized formations without rewriting unit ownership or command history.

### 2.5 Command philosophy

The player should primarily issue intent-based commands:

- Move / attack-move.
- Attack target.
- Defend / hold position.
- Patrol.
- Guard or escort.
- Capture / secure region or objective.
- Establish staging point.
- Retreat / withdraw.
- Resupply / return to supply source.
- Build / repair / reclaim, where supported.
- Rally and production destination.

Commands need explicit lifecycle states: accepted, active, suspended, completed, failed, or cancelled. Failure must be observable and explainable (blocked route, invalid target, insufficient resources, no supply path, etc.). Commands should not silently disappear.

Command queues and command replacement behavior must be consistent across single units, squads, formations, and armies. The UI must communicate whether a command applies to selected units individually or to a group-level controller.

---

## 3. Architecture invariants (binding)

These invariants take precedence over convenience and local implementation preferences. Sections 3.1–3.5 are recorded as an accepted decision in [ADR-0001: Authoritative simulation and determinism contract](docs/adr/0001-authoritative-simulation.md).

### 3.1 Authoritative simulation is CPU-side

The CPU simulation owns all gameplay-authoritative state:

- Entity existence and identity.
- Unit and building positions/velocities used by gameplay.
- Health, damage, armor, status, and death.
- Orders, command queues, and group membership.
- Combat decisions and hit outcomes.
- Projectile gameplay state and collision/hit resolution.
- Economy, production, construction, and research.
- Territory/control state.
- Supply and logistics state.
- Player visibility/exploration state that affects gameplay decisions.
- AI decisions and deterministic random state.

GPU compute may accelerate derived or non-authoritative work, but it must not become the sole source of truth for gameplay outcomes in the initial architecture. GPU results that influence authoritative state must be validated or reconciled by the CPU, or remain disabled until a separate determinism design and cross-device validation prove them safe.

The initial implementation must remain fully playable in CPU-only headless mode.

### 3.2 Deterministic simulation contract

The simulation uses a fixed tick rate (initial proposal: 20 ticks/second; validate through gameplay and profiling). Rendering runs independently and interpolates between simulation snapshots.

Determinism requirements:

- No wall-clock reads inside authoritative systems.
- All gameplay randomness uses explicitly seeded deterministic RNG streams.
- System execution order is explicit and versioned.
- Entity and command iteration order must be stable wherever order can affect outcomes.
- Parallel jobs must not introduce nondeterministic reductions, race-dependent writes, or order-dependent side effects.
- Floating-point behavior must be documented. If cross-platform lockstep is required, use deterministic numeric strategies for authoritative calculations or prove chosen operations equivalent on supported targets. Fixed timestep alone is not determinism.
- Authoritative commands are applied at defined tick boundaries.
- All state-changing inputs are recorded in a command/event history.
- A canonical state hash can be produced for any tick.
- Replays identify simulation/build/schema versions and initial scenario data.

A same-build replay must reproduce identical canonical state hashes on the same supported platform/configuration. Cross-platform bit-identical lockstep is a separate acceptance target and must not be claimed until validated on Windows and Linux with the chosen math and compiler settings.

### 3.3 Headless means no graphics device required

The headless runtime must initialize and run the complete authoritative simulation without creating a window, initializing a graphics API, requiring a physical/software GPU, or loading renderer-only assets.

Headless mode is a first-class runtime configuration, not a test-only stub. It is used for CI, replays, AI simulation, scenario validation, fuzzing, and performance tests.

A separate optional headless-GPU mode may be introduced for GPU compute profiling, but it must not be confused with CPU-only headless simulation.

### 3.4 One authoritative representation per gameplay fact

Do not maintain multiple independently mutable copies of the same gameplay fact (for example, ECS position plus a separate authoritative SoA position array plus a GPU-owned position buffer).

Derived caches and upload buffers are allowed only when their source of truth is explicit, update/invalidation rules are documented, they can be rebuilt from authoritative state, and stale data cannot affect gameplay outcomes.

### 3.5 Rendering is a consumer of simulation snapshots

The renderer consumes immutable/interpolated snapshots and visual metadata. It must not mutate gameplay state. Render interpolation, animation, LOD, culling, particles, and visual prediction must never feed back into authoritative simulation.

### 3.6 No premature abstraction or backend lock-in

Choose the simplest architecture that preserves the invariants. Validate expensive or high-risk technology choices with small, measurable spikes before committing the entire engine to them.

---

## 4. Proposed Rust technology stack

The stack is a proposal, not a reason to force a library into a poor fit. Record decisions in ADRs.

- **Language/toolchain:** stable Rust; Cargo workspace.
- **Window/input:** winit.
- **Rendering/compute candidate:** wgpu selected over ash after a headless spike on both candidates — see [ADR-0004](docs/adr/0004-rendering-backend.md) and `benchmarks/graphics/`. wgpu reduces low-level Vulkan lifetime/synchronization work and offers multiple backends; ash offers direct Vulkan control at substantially higher engineering cost. Do not build both production backends.
- **Shaders:** WGSL if using wgpu; GLSL/SPIR-V if using ash. Shader compilation and validation must be part of CI.
- **ECS candidate:** bevy_ecs vs hecs benchmarked on representative workloads at 10k/50k/100k entities; hecs selected for the initial implementation spike — see [ADR-0003](docs/adr/0003-ecs-selection.md) for measurements and revisit criteria. Do not build a custom ECS before measurements justify it.
- **Math:** glam, subject to deterministic-simulation validation.
- **CPU parallelism:** rayon only for jobs whose writes and reductions preserve deterministic outcomes.
- **Serialization:** serde; versioned RON/JSON for editable definitions and maps, with a suitable binary format considered for runtime saves if profiling warrants it.
- **Assets:** gltf for model import; define an engine-owned cooked asset format and asset manifest.
- **Collision/query:** evaluate rapier3d for static geometry queries and selected dynamic interactions. Do not assume a general-purpose rigid-body solver is suitable for every mass unit.
- **Audio:** rodio is an initial candidate; validate spatial audio, streaming, and platform behavior before commitment.
- **Scripting:** defer language selection. Prefer data-driven definitions and Rust systems first. If scripts later affect authoritative simulation, deterministic execution, versioning, sandboxing, and resource budgets become mandatory.
- **Networking:** defer transport selection. quinn/QUIC is one possible transport, not a multiplayer architecture.

### 4.1 Required technology spikes

Before finalizing major choices, create isolated prototypes and record results:

1. Render and update 10,000 instanced units with camera movement and selection highlighting (API-level spike done headless in `benchmarks/graphics/` — see ADR-0004; headed GPU run is a recorded follow-up).
2. Run a representative compute workload with the candidate graphics API (WGSL compute validated in the wgpu spike; execution on real GPUs is a recorded follow-up — see ADR-0004).
3. Start CPU-only headless simulation on Windows and Linux.
4. Benchmark ECS iteration and representative systems at 10k, 50k, and 100k entities (done for the library comparison — see [ADR-0003](docs/adr/0003-ecs-selection.md) and `benchmarks/ecs/`; repeat with real gameplay systems at M3).
5. Run deterministic simulation tests with and without Rayon.
6. Benchmark navigation for mixed movement profiles and crowded chokepoints.
7. Validate asset import, animation, and hot-reload requirements.

Each spike must have a short report: hardware, versions, workload, measured results, limitations, and recommendation.

---

## 5. Workspace and subsystem boundaries

Suggested Cargo workspace (names may evolve; boundaries should remain clear):

- phalanx_core: simulation clock, world state, entity identity, commands, events, deterministic RNG, state hashing, simulation schedule.
- phalanx_game: game rules and authoritative systems (combat, economy, production, territory, supply, research, unit definitions).
- phalanx_navigation: nav representation, global pathfinding, flow fields, local movement constraints, formation movement.
- phalanx_map: map schema, terrain data, region graph, map validation, import/export.
- phalanx_ai: strategic, operational, and tactical AI. AI submits ordinary commands through the same validated command interface as players.
- phalanx_render: renderer, materials, terrain rendering, instancing, LOD, overlays, visual effects.
- phalanx_gpu: optional GPU compute and upload/readback utilities; never owns authoritative gameplay state.
- phalanx_assets: asset registry, import/cook pipeline, manifests, runtime loading.
- phalanx_audio: audio playback and spatial audio.
- phalanx_editor: map/scenario editor, inspection, visualization, agent API.
- phalanx_runtime: executable, application lifecycle, mode selection, packaging.
- phalanx_replay: replay recording, playback, comparison, diagnostics.
- tools/: offline asset/map tools and validation utilities.

Dependency direction should be one-way: core types and simulation contracts must not depend on renderer, editor, windowing, or GPU crates. Headless builds must not pull in platform graphics initialization through accidental dependencies.

---

## 6. Simulation model

### 6.1 Tick schedule

The authoritative tick schedule must be explicit, documented, and tested. Initial candidate order:

1. Ingest validated commands assigned to this tick.
2. Apply command changes and resolve command lifecycle transitions.
3. Update strategic/group intent and formation targets.
4. Update economy, resource extraction, and supply production.
5. Update construction, production, and research progress.
6. Update navigation requests and scheduled path/flow-field work.
7. Compute unit movement intent and local constraints.
8. Resolve authoritative movement and collision constraints.
9. Acquire/validate targets and resolve combat decisions.
10. Advance authoritative projectiles and resolve impacts.
11. Apply damage, deaths, capture progress, and destruction effects.
12. Update territory, supply connectivity, and visibility as scheduled.
13. Emit deterministic gameplay events and update derived caches.
14. Validate critical invariants in debug/test builds.
15. Produce canonical state hash and replay checkpoint as configured.

This order is a starting contract, not a claim that every subsystem must run every tick. Systems may have explicit update frequencies or budgets, but their schedule and phase must be deterministic. Changes to ordering require an ADR or explicit simulation-version change because they can alter replay results and game balance. The schedule is implemented as `Phase::SCHEDULE`, executed by `SimulationRunner` in `phalanx_core::simulation` (`crates/phalanx_core/src/simulation.rs`); the contract is recorded in [ADR-0001](docs/adr/0001-authoritative-simulation.md).

### 6.2 Entity identity and lifecycle

Use stable logical entity identifiers suitable for commands, replays, saves, and agent APIs. Do not expose transient ECS archetype locations or raw memory indices as persistent identifiers.

Entity lifecycle must define creation and destruction tick, generation/reuse behavior, ownership/team, group membership changes, reference invalidation, death/destruction events, and save/load identity restoration.

Persistent references must be validated; stale references must return explicit errors rather than accidentally resolving to a newly created entity. Stable IDs are implemented as `EntityId` (slot index + generation) with `EntityAllocator` in `phalanx_core::entity` (`crates/phalanx_core/src/entity.rs`), which bumps the generation on release so stale IDs never resolve to reused slots.

### 6.3 Data definitions vs runtime state

Static definitions and mutable instances are separate concepts.

UnitDefinition:
- Stable definition ID and schema version.
- Movement profile and physical footprint.
- Health/armor and combat capabilities.
- Vision/sensor properties.
- Weapon definitions.
- Resource/production costs.
- Supply demand or logistics tags.
- Allowed orders and role tags.
- Asset references.

UnitState:
- Stable entity ID and owner.
- Current transform and velocity.
- Health/status.
- Current order and queued orders.
- Target and cooldown state.
- Group/formation membership.
- Supply state where applicable.
- Runtime-specific counters.

Buildings, weapons, upgrades, technologies, and map objects follow the same separation. Balance data must not be scattered as magic constants across systems.

### 6.4 Events

Gameplay systems may emit typed events for UI, audio, AI observation, telemetry, and replay diagnostics. Events must have deterministic ordering and stable identifiers where they are part of replay semantics.

Do not use an untyped global event bus as a substitute for clear ownership or direct system contracts. Distinguish authoritative state transitions from presentation notifications.

---

## 7. Navigation, movement, and formations

Navigation is a core differentiator and must be designed as three cooperating layers.

### 7.1 Global navigation

Responsible for route selection across the map:

- A* or another suitable algorithm for individual/low-count requests.
- Shared flow fields for many units with compatible movement profiles and destinations.
- Hierarchical pathfinding or region/portal graphs for large maps.
- Movement-profile-specific traversability and costs.
- Dynamic cost modifiers for threat, terrain, ownership, supply, and tactical constraints.
- Path request prioritization, deduplication, cancellation, and bounded per-tick budgets.
- Explicit outcomes for unreachable, blocked, stale, or invalid destinations.

A single global nav grid is not sufficient for every unit type. Movement profiles define what terrain and obstacles a unit can traverse and what costs apply.

### 7.2 Local movement

Responsible for following a route and handling nearby units/obstacles:

- Separation and collision avoidance.
- Static obstacle handling.
- Dynamic obstacle handling.
- Narrow passage and chokepoint behavior.
- Deadlock detection and recovery.
- Stable tie-breaking when multiple units compete for space.
- Movement constraints for large footprints and different speeds.

RVO/ORCA-style algorithms are candidates, not a complete solution. Benchmark and validate them under dense formations and bottlenecks. GPU acceleration is optional and non-authoritative in the initial design.

### 7.3 Formation movement

Formation movement must not be reduced to sending every member to the same point.

Required concepts:

- Formation template and slot layout.
- Destination assignment and slot reassignment.
- Group heading and speed.
- Cohesion/separation constraints.
- Formation compression/expansion in narrow terrain.
- Arrival behavior and regrouping.
- Handling of slow, damaged, blocked, or detached members.
- Group-level route planning with member-level local movement.
- Explicit fallback when the requested formation cannot be maintained.

Initial templates: line, column, wedge, loose/dispersed, and defensive posture. Names and exact shapes are game-design choices.

### 7.4 Movement profiles

Use a data-driven profile model rather than hard-coded unit classes. Initial examples may include ground, hover, naval, and air, but only implement profiles required by the chosen game setting.

A movement profile describes traversability, footprint, speed constraints, terrain costs, obstacle interaction, altitude/depth rules if relevant, and navigation layer. Mixed-profile groups need a documented policy: split routes, use compatible subgroups, or reject unsupported mixed orders.

### 7.5 Navigation diagnostics

Provide editor overlays and machine-readable queries for nav cells/regions and movement costs, blocked/traversable areas, path and flow-field visualization, formation slots and group centroid, local avoidance vectors, stuck/deadlock state, path request queue/budget, and route failure reasons.

---

## 8. Territory, supply, and operational warfare

### 8.1 Region graph

Maps may define a region graph independently of the movement grid. Nodes represent strategic areas or objectives; edges represent strategic adjacency, routes, or connections. The graph supports AI planning, front-line analysis, control propagation, and logistics.

The region graph must not be treated as a substitute for actual pathfinding. Strategic adjacency does not guarantee a unit can physically traverse the terrain.

### 8.2 Control and front lines

A front line is derived from friendly/enemy influence, controlled regions, forces, and traversable connections. It may be approximated for UI and AI; authoritative facts remain region control, unit positions, ownership, and visibility.

Expose front-line analysis as a derived system. Do not store a manually editable polyline as the source of truth.

### 8.3 Supply network

Represent supply infrastructure and routes as a graph or graph-like network with explicit capacity and cost. The initial gameplay model may simplify calculations, but must support source-to-consumer reachability, capacity bottlenecks, ownership and contest/disruption, route cost and distance, supply status and reason codes, scheduled recomputation and incremental updates, visualization and AI queries.

Supply must have a defined gameplay effect before being enabled: for example, reinforcement availability, production throughput, unit effectiveness, or resource delivery. Avoid adding a purely decorative supply overlay.

### 8.4 Reinforcement and production flow

Production queues should specify rally destinations, staging points, or army assignments. Newly produced units should have a clear path from production to the intended operational area.

The system should eventually support reinforcement priorities and transport capacity. Do not build a complex transport simulation before the core production-to-front loop is playable.

---

## 9. Core gameplay systems

Implement in phases; this list describes the intended system surface, not the first milestone.

### 9.1 Player and command systems

- Selection: click, box, multi-select, control groups, filters, and strategic group selection.
- Orders: move, attack-move, attack, defend, patrol, guard, capture, retreat, build, repair, resupply, and rally as supported.
- Command queue: replace, append, insert, cancel, and group-level order semantics.
- Context orders based on target type and modifiers.
- Feedback for accepted, active, completed, failed, and cancelled commands.
- Command history suitable for replay and debugging.

### 9.2 Economy and production

- Resource sources and extraction.
- Resource stockpiles and/or income flow.
- Construction costs and progress.
- Production queues and rally points.
- Unit/building caps where required.
- Region- or infrastructure-based resource bonuses where required.
- Explicit resource transaction rules and deterministic rounding.

Avoid floating-point currency if exact accounting matters; choose fixed-point or integer resource units and document rounding.

### 9.3 Building and infrastructure

- Placement validation, footprint, terrain constraints, and ownership rules.
- Construction sites and progress.
- Power/supply/logistics infrastructure only if required by the selected design.
- Production, repair, upgrade, and demolition.
- Placement preview and clear rejection reasons.

### 9.4 Combat

- Weapon definitions, range, cooldown, target filters, damage types, armor, and area effects.
- Target acquisition and target persistence rules.
- Line of sight and visibility interaction.
- Authoritative hit resolution.
- CPU-authoritative projectile state and impact resolution in the initial design.
- Damage application, destruction, wrecks/corpses, and cleanup.
- Combat events for UI/audio/replay.
- Deterministic tie-breaking for target selection and simultaneous effects.

### 9.5 Fog of war and intelligence

Per-player states: unseen, explored, and currently visible. Separate visual rendering from authoritative visibility state.

The model should eventually distinguish detection/sensor capabilities if the game design needs them (for example, visual, radar, sonar, stealth, or electronic detection). Do not assume one circular vision radius is sufficient for all units and terrain.

Visibility updates may be scheduled and spatially accelerated, but gameplay decisions must use authoritative CPU state. GPU rendering of visibility textures is permitted as a derived visualization.

### 9.6 AI

Use a hierarchy:

- Strategic AI: economy priorities, expansion, region value, production planning, and overall objectives.
- Operational AI: army assignment, fronts, reinforcement, supply routes, and attack/defense plans.
- Tactical AI: local target selection, positioning, retreat, and engagement behavior.

AI must issue the same validated commands as a player and observe the same permitted information. It must not read hidden enemy state. AI decisions use deterministic RNG and scheduled decision points. Start with a small number of understandable strategic states and measurable behavior, not a large opaque behavior framework.

---

## 10. World, maps, and terrain

### 10.1 Terrain representation

The initial terrain proposal is heightfield-based terrain with support for additional static geometry and explicit navigation layers. A heightmap alone cannot represent caves, overhangs, tunnels, or multiple walkable surfaces at the same horizontal coordinate.

If multi-level traversal is required, represent it explicitly through layers, portals, meshes, or another validated structure. Do not imply that a simple heightmap provides general 3D navigation.

### 10.2 Map contents

A map may contain terrain and material data, static geometry and collision data, region graph and strategic objectives, resource sites, build sites and infrastructure, spawn locations and player start data, navigation profile metadata, environmental and visual settings, and scenario rules/initial conditions.

Map loading is staged: parse, validate schema, validate references and constraints, build runtime data, then activate. A failed load must not leave a partially replaced active world.

### 10.3 Map versioning and validation

Map formats require schema versions and migration policy. Validation reports must identify path, field, and reason. The editor and command API must be able to run validation without starting a match.

---

## 11. Rendering and strategic readability

### 11.1 Rendering goals

- Large-scale instanced rendering and efficient batching.
- Chunked terrain rendering and material blending.
- LOD for meshes, animation, effects, and UI markers.
- Frustum and occlusion culling where beneficial.
- Selection rings, health/status indicators, order markers, and group/formation visualization.
- Strategic overlays for regions, ownership, supply, front lines, threat, and objectives.
- Fog-of-war visualization.
- Camera controls: pan, rotate, zoom, edge pan, keyboard movement, jump to event, and optional camera bookmarks.

### 11.2 Strategic zoom

At far zoom levels, do not attempt to render every unit as a full detailed model. Use aggregated representations: army/group markers and strength/composition summaries, region ownership and contest indicators, supply routes and bottlenecks, front-line and objective overlays, reinforcement flows, and alerts.

At medium zoom, show formations and unit classes. At close zoom, show individual units and tactical detail. Thresholds and representations must be tuned for readability and tested with real battles.

The strategic representation is a view over actual simulation state, not a separate simplified game state.

### 11.3 Visual effects and animation

Particles, explosions, decals, animation interpolation, skeletal animation, and GPU skinning are presentation systems. They may be approximated or dropped under load without changing gameplay outcomes.

Animation LOD must be considered for mass battles. glTF import alone does not provide a complete animation runtime; define skeleton, clip, blending, retargeting (if needed), and asset validation requirements before building a large content pipeline.

### 11.4 Renderer fallback

Rendering must be optional for headless simulation. Renderer initialization failures must not be silently converted into a partially working graphical mode. The application layer reports the failure and selects a documented fallback only when requested.

---

## 12. Agent/editor command API

The editor and scenario tools must be controllable without manual interaction. The initial transport is JSON Lines (one JSON object per line), but the protocol must be versioned and documented.

### 12.1 Commands and queries are separate

**Commands** mutate state, for example: create_entity, spawn_unit, place_building, paint_terrain, set_region_control, issue_order, assign_to_formation, set_rally_point, save_map, load_map, run_scenario, undo, redo.

**Queries** inspect state, for example: get_project_info, get_map_info, validate_map, list_entities, get_entity, get_region_graph, get_supply_network, get_navigation_debug, get_simulation_status, get_state_hash, get_command_result, get_performance_counters.

### 12.2 Protocol requirements

Every request should support protocol/API version, request ID and command ID, explicit success/error result, stable entity and asset references, structured error code and human-readable explanation, validation-only/dry-run mode where meaningful, batch limits and transaction boundaries, idempotency semantics for retryable operations, explicit tick assignment for simulation-affecting commands, machine-readable output (diagnostics go to stderr or a separate channel), timeouts/cancellation for long-running operations, and capability checks with scoped filesystem access.

Commands that affect a running simulation are queued for a defined tick boundary. Editor-only map mutations are not silently mixed with active match state. The simulation-side command contract is the versioned `CommandEnvelope` (request ID, simulation-assigned command ID, target tick, actor, typed payload) in `phalanx_core::command` (`crates/phalanx_core/src/command.rs`), applied by `SimulationRunner` in ascending command-ID order at the target tick's boundary; late and duplicate commands are rejected with structured errors.

### 12.3 Agent integration

Coding agents must follow the repository-wide instructions in `AGENTS.md`. The command/query protocol, trust boundaries, and simulation tick-boundary requirements defined in this document remain the authoritative technical contract.

---

## 13. Savegames, replays, and compatibility

These are foundational systems, not polish.

### 13.1 Savegame model

Separate:

1. **Map/static data:** terrain, region graph, resource sites, static objects, and scenario setup.
2. **Authoritative match state:** entities, ownership, orders, queues, health, economy, supply, territory, AI state, projectiles, RNG states, and simulation tick.
3. **Presentation state:** camera, UI layout, selected entities, and other optional local preferences.

Do not serialize GPU handles, pointers, ECS archetype locations, or transient renderer resources. Rebuild derived caches after loading.

Save loading must validate versions, references, limits, and invariants before atomically replacing the active match state.

### 13.2 Replay model

A replay contains replay format version, simulation/game rules version, initial map/scenario and content identifiers/hashes, player identity/configuration, deterministic RNG seeds and stream state as required, tick-indexed commands and other authoritative external inputs, and optional periodic state hashes/checkpoints.

Replays must support seeking through checkpoints or deterministic resimulation. A replay mismatch report should identify the first divergent tick and provide subsystem-level diagnostics where feasible. The replay harness is implemented in `phalanx_core::replay` (`crates/phalanx_core/src/replay.rs`) — `ReplayRecording`/`ReplayRecorder` capture format and simulation schema versions, tick rate, RNG seed, the tick-indexed command stream, and per-tick hashes, and `replay_recording` reports the first divergent tick — with a small deterministic CI scenario in `phalanx_core::scenario`.

### 13.3 State hashing

Canonical state hashing must use stable ordering and explicit serialization of authoritative fields. Exclude pointers, padding bytes, GPU handles, renderer state, wall-clock timestamps, and other nondeterministic/transient data. Canonical hashing is implemented as `CanonicalHasher`/`CanonicalSerialize` in `phalanx_core::hash` and exposed as `Simulation::canonical_hash` (`crates/phalanx_core/src/simulation.rs`); the runner records per-tick hashes at the `ProduceStateHash` phase.

Hashing may be configurable in release builds, but must be available in tests and diagnostic builds.

---

## 14. Performance and scalability

### 14.1 Targets are workload-defined

The long-term target is 10,000+ units in a large match. “10,000 units at 60 FPS on mid-range hardware” is not a sufficient acceptance criterion without a workload and hardware profile.

Define and benchmark at least:

- **Mass movement:** 10k units moving, pathing, and avoiding congestion.
- **Mass combat:** 10k units in multiple engagements, with target acquisition and damage.
- **Full battlefield:** units, buildings, territory, supply, visibility, AI, and rendering.
- **Stress scale:** 50k/100k entities to identify scaling cliffs, even if not a shipping target.

Each benchmark specifies map size, unit composition, activity rate, number of groups, projectile count, visibility workload, hardware, OS, build mode, and duration.

### 14.2 Budgets and metrics

Track simulation tick time (average, p95, p99, worst), per-system time and entity counts, navigation request queue and processing time, supply/region recomputation time, command latency from submission to application, render CPU time and GPU frame time, draw calls/instances/visible entities/LOD distribution, memory use and allocation rates, and replay/hash overhead.

Do not enforce fragile wall-clock thresholds on shared GitHub-hosted CI runners. Use functional CI everywhere and stable dedicated hardware for performance regression thresholds. Store benchmark trends and workload metadata.

### 14.3 Data-oriented optimization

Start with a conventional, well-understood ECS. Profile real workloads before introducing custom SoA stores or GPU mirrors.

Hot systems should process batches and contiguous data where practical. Function decomposition must preserve opportunities for vectorization, cache-friendly iteration, and deterministic parallel execution. Avoid per-entity abstraction layers that prevent efficient batch processing.

Any secondary data layout must have one clear owner and a documented synchronization direction. Derived buffers must be rebuildable.

---

## 15. Testing, CI, and quality gates

Testing is a primary engineering requirement.

### 15.1 Required automated checks

- cargo fmt --check
- cargo clippy --workspace --all-targets -- -D warnings
- cargo test --workspace
- Build on ubuntu-latest and windows-latest.
- Dependency/security audit (for example cargo audit) and secret scanning.
- Map/schema validation tests.
- Headless smoke tests.

The M0 subset of these gates is implemented in `.github/workflows/ci.yml`: formatting, Clippy with `-D warnings`, tests, a workspace build, and the CPU-only headless smoke scenario run on `ubuntu-latest` and `windows-latest` for pushes to `main` and pull requests targeting `main`, plus a `cargo audit` dependency job against the committed `Cargo.lock` and a gitleaks secret-scanning job. The workflow uses least-privilege permissions (`contents: read`) and cargo artifact caching; performance timing thresholds are intentionally excluded from shared hosted runners.

### 15.2 Required test categories

- Unit tests for local rules and data structures.
- Integration tests for subsystem interactions.
- Determinism tests with fixed seeds and command streams.
- Replay round-trip and replay reproduction tests.
- Save/load round-trip tests.
- Property-based tests for invariants and randomized scenarios.
- Fuzz tests for untrusted maps, saves, and agent commands.
- Navigation tests: reachability, blocked routes, chokepoints, mixed movement profiles, and deadlock recovery.
- Supply graph tests: reachability, capacity, disruption, and incremental updates.
- Combat tests: range, cooldown, simultaneous damage, target ordering, and projectile impacts.
- Stress tests at 10k+ and exploratory 50k/100k scale.
- GPU validation tests for shaders, resource lifetimes, and synchronization when GPU features exist.

### 15.3 Test scenario

Maintain a deterministic test scenario that exercises multiple players/teams, several unit and building definitions, multiple regions and contested objectives, resource extraction and production, supply sources/routes/bottlenecks/disruption, several armies/formations, different movement profiles where implemented, pathfinding and chokepoints, combat/projectiles/destruction, fog of war and scouting, save/load and replay, and the agent command/query API. The concrete M1 slice scenario and its acceptance test are specified in [docs/vertical-slice-scenario.md](docs/vertical-slice-scenario.md).

The scenario must be small enough for fast CI tests and have separate scaled variants for stress/performance testing.

---

## 16. Security and trust boundaries

Agent commands, imported projects, maps, saves, and scripts are untrusted input—even when supplied locally.

- Define actor capabilities: trusted local editor owner, agent automation, imported project, packaged runtime.
- Scope filesystem access to the project root or package content; handle symlinks safely.
- Packaged runtime must not fall back to arbitrary host filesystem paths.
- Apply request size, batch, nesting, entity-count, map-size, and parse-depth limits.
- Apply time and memory budgets to scripts if scripting is introduced.
- Validate references, coordinates, numeric ranges, and enum values at boundaries.
- Never execute arbitrary shell commands from map content or agent input.
- Keep secrets and credentials out of logs, replays, and project assets.
- Report denied capabilities with structured errors.

---

## 17. Development workflow and architecture governance

### 17.1 Repository workflow and agent instructions

Repository workflow, implementation discipline, testing expectations, dependency policy, and coding-agent completion reports are defined in `AGENTS.md`.

All contributors must review architecture-sensitive changes against this document and applicable ADRs. Keep architecture decisions and subsystem contracts here; do not duplicate their full definitions in agent instructions.

### 17.2 Architecture Decision Records

Store ADRs in docs/adr/. Each ADR includes context, problem, alternatives, decision, consequences, and revisit criteria.

ADR index (remaining entries are created as their decisions land):

- [0001-authoritative-simulation.md](docs/adr/0001-authoritative-simulation.md) (accepted; codifies the §3 simulation invariants and determinism contract)
- 0002-determinism-and-numeric-model.md
- [0003-ecs-selection.md](docs/adr/0003-ecs-selection.md) (accepted as an initial spike selection; production commitment deferred to M3 validation)
- [0004-rendering-backend.md](docs/adr/0004-rendering-backend.md) (accepted for the production prototype; headed GPU validation is a recorded follow-up)
- 0005-navigation-and-formation-model.md
- 0006-territory-and-supply-model.md
- 0007-map-and-save-format.md
- 0008-agent-command-protocol.md

An agent must not silently override an accepted ADR. If new evidence invalidates a decision, propose a replacement ADR and explain migration impact.

### 17.3 Implementation guidance

Coding agents must follow the function-design and implementation rules in `AGENTS.md`. Architecture-specific invariants—including single ownership of authoritative state, deterministic simulation, CPU-only headless operation, and atomic map activation—are defined in their respective sections of this document and remain authoritative.

### 17.4 Documentation and dependency governance

Use `AGENTS.md` for contributor-facing documentation and dependency-handling procedures. This document remains the source of truth for engine architecture, subsystem boundaries, technical constraints, and unresolved design decisions.

---

## 18. Milestones and acceptance criteria

Milestones are capability gates, not calendar promises. Do not start broad content production before technical risks are proven.

### M0 — Repository and architecture foundation

- Workspace builds on Windows and Linux.
- Headless executable starts without graphics initialization.
- Simulation clock and explicit schedule exist.
- Stable entity IDs and command envelope exist.
- CI runs formatting, clippy, tests, and headless smoke test.
- Initial ADRs are written for simulation authority, determinism, ECS, and renderer candidate.

**Exit:** a small deterministic headless scenario runs on both OS targets.

### M1 — First playable vertical slice

- Small 3D battlefield and camera.
- Two teams and a small set of unit definitions.
- Selection and basic command feedback.
- Move and attack-move orders.
- Global pathfinding and basic local avoidance.
- Basic combat and destruction.
- Headless execution of the same scenario.
- Command recording, replay, and state hashing.

The concrete scenario ("Crossing assault") and its headless acceptance test are specified in [docs/vertical-slice-scenario.md](docs/vertical-slice-scenario.md).

**Exit:** a complete small battle can be played, recorded, and reproduced in headless mode with matching state hashes on the same supported build/platform.

### M2 — Operational RTS foundation

- Squads/formations and group orders.
- Region graph and capture/control loop.
- Resource sites and production.
- Basic supply sources and routes with visible consequences.
- Strategic overlays for regions, groups, and supply.
- Basic skirmish AI using the public command interface.

**Exit:** a match has meaningful expansion, fronts, reinforcement, and at least one supply-driven strategic decision.

### M3 — Scale and performance validation

- Mass-movement benchmark.
- Mass-combat benchmark.
- 10k-unit full-battlefield scenario.
- Navigation and simulation profiling.
- LOD and strategic aggregation.
- Memory and frame-time budgets measured on named reference hardware.

**Exit:** documented performance results identify bottlenecks and demonstrate the agreed target workload. Do not claim a 10k/60 FPS target until this gate passes.

### M4 — Content and editor workflow

- Agent command/query API.
- Map editor operations and validation.
- Data-driven units, buildings, weapons, and upgrades.
- Asset import/cook pipeline.
- Save/load with schema versioning.
- Improved AI, HUD, audio, effects, and animation.

**Exit:** an agent can construct, validate, save, and launch a deterministic skirmish scenario without manual editor interaction.

### M5 — Product readiness

- Multiple maps and factions/rosters with distinct gameplay.
- Robust save/load and replay compatibility policy.
- Settings, onboarding, accessibility, and UX polish.
- Packaging and native Windows/Linux release pipeline.
- Profiling and crash diagnostics.
- Balance and large-scale gameplay iteration.

Multiplayer is a separate product milestone and must not be treated as a small transport add-on.

---

## 19. Multiplayer: future, but architecture-aware

The initial product is singleplayer and skirmish AI. Networking is deferred, but simulation contracts preserve a path toward it.

Before choosing lockstep or server-authoritative networking, evaluate cross-platform deterministic numeric behavior, command delay and input scheduling, late command handling and rollback requirements, state checksums and desync diagnosis, snapshot/resynchronization, host migration and disconnect behavior if required, anti-cheat and authority model, bandwidth for commands/snapshots/visibility, and AI determinism and simulation version compatibility.

QUIC (quinn) is only a possible transport. It does not define command synchronization, authority, rollback, or resync.

Do not promise lockstep multiplayer until cross-platform determinism has been demonstrated by automated tests. If strict determinism proves too costly, a server-authoritative model may be the better product decision.

---

## 20. Open decisions

These are intentionally open and must be resolved with prototypes, design tests, or ADRs:

- Final setting, factions, unit roster, and visual identity.
- Exact resource model and strategic value of regions.
- Exact supply rules and degree of logistics simulation.
- Capture/control rules and front-line presentation.
- Final tick rate and numeric representation.
- Cross-platform determinism target.
- wgpu vs ash.
- bevy_ecs vs hecs.
- Terrain representation beyond initial heightfield plus static geometry.
- Animation runtime and asset cooking format.
- Audio library.
- Whether and when gameplay scripting is needed.
- Embedded vs standalone editor.
- Multiplayer model and transport.
- Minimum/recommended hardware profiles.

Every resolved decision with meaningful architectural impact must be recorded as an ADR and reflected here.

---

## 21. Final principle

Build the smallest complete version of the intended large-scale warfare experience first.

The first proof is not a beautiful renderer, a sophisticated editor, or a 100,000-entity benchmark in isolation. It is a reproducible battle in which units can move, fight, capture meaningful territory, receive reinforcements, and create a readable front line—both in the graphical runtime and in headless simulation.

Scale, logistics, command clarity, and determinism must reinforce one another. No subsystem should be optimized in isolation at the expense of the actual game.
