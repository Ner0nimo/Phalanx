# M1 Vertical Slice — Scenario and Acceptance Test Specification

- **Status:** Accepted specification for issue #11 (M1 first playable vertical slice); revised for issue #13 (see the corrected §8 worked example and §9 item 3)
- **Date:** 2026-11-12
- **Related:** [RtsEngineDoc.md](../RtsEngineDoc.md) §2 (gameplay model), §6 (simulation model), §9.4 (combat), §13 (replays/hashing), §15 (testing), §18 (M1); [ADR-0001](adr/0001-authoritative-simulation.md) (authoritative simulation and determinism contract); `phalanx_core` contracts from issues #5–#7 (`SimulationRunner`/`Phase`, `CommandEnvelope`, `EntityId`, `RngStreams`, `canonical_hash`, `replay_recording`).

## 1. Purpose and scope

One concrete, small, end-to-end battle that can be implemented and tested without inventing missing game rules. The slice uses the existing simulation contracts: fixed 20 Hz ticks (`TickRate::DEFAULT`), the 15-phase schedule (`Phase::SCHEDULE`), tick-indexed versioned commands (`CommandEnvelope`), stable entity IDs (`EntityId`), seeded RNG streams (`RngStreams`), canonical per-tick state hashes (`Simulation::canonical_hash`), and the replay harness (`ReplayRecorder`/`replay_recording`).

**Out of scope for this slice** (keep out unless essential to coherence): regions/territory, supply networks, technology trees, large-scale economy, production/building, fog of war (full visibility in the slice), projectiles (weapons are hitscan; the combat contract supports projectile weapons later), audio, and strategic zoom. Selection is a presentation concern only and is not a simulation command.

## 2. Map: "Crossing"

A small 3D battlefield, 200 m × 200 m, flat heightfield (height 0) with static blocking obstacles. Coordinates are meters, `(x, z)` on the ground plane, `y` is height.

| Object | Kind | Position (x, z) | Size (w × d) | Blocks movement |
|---|---|---|---|---|
| West spawn pad | spawn point, Team A | (20, 100) | 10 × 10 | no |
| East spawn pad | spawn point, Team B | (180, 100) | 10 × 10 | no |
| Wall N | obstacle | (100, 60) | 90 × 4 | yes |
| Wall S | obstacle | (100, 140) | 90 × 4 | yes |
| Rock W | obstacle | (60, 100) | 8 × 8 | yes |
| Rock E | obstacle | (140, 100) | 8 × 8 | yes |

The two walls leave a single **chokepoint lane**: Wall N occupies x∈[55,145], z∈[58,62] and Wall S occupies x∈[55,145], z∈[138,142], so the only passage between them is the lane z∈(62,138) (76 m wide) at x∈[55,145], with the two rocks (x∈[56,64] and x∈[136,144], z∈[96,104]) narrowing the lane entrances (the lane midpoint between the rocks is 72 m wide). Pathfinding runs on a 1 m grid over the 200×200 map with obstacles inflated by the unit radius (0.5 m).

Map data is static scenario data (part of the replay's initial scenario hash), not runtime state.

## 3. Teams and roster

Two teams, setting-neutral names: **Team A** (player side, `ActorId(1)`) and **Team B** (scripted side, `ActorId(2)`). Each side spawns 6 units at tick 0 (scenario setup, not commands): 3 `striker` + 3 `skirmisher`, placed on the team's spawn pad in a fixed layout (deterministic order by spawn index).

Unit definitions (placeholder balance values; definitions are data, not code constants):

| Field | striker | skirmisher |
|---|---|---|
| definition id | `unit.striker` | `unit.skirmisher` |
| health | 100 | 60 |
| armor | 0 | 0 |
| speed | 4 m/s (0.2 m/tick at 20 Hz) | 3 m/s (0.15 m/tick) |
| sight range | 40 m | 40 m |
| weapon range | 10 m | 30 m |
| weapon damage | 10 | 6 |
| weapon cooldown | 20 ticks (1 s) | 20 ticks (1 s) |
| hit resolution | hitscan, always hits | hitscan, always hits |
| radius (collision) | 0.5 m | 0.5 m |

## 4. Commands

Simulation-affecting commands use the existing envelope contract (`CommandRequest` → `CommandEnvelope` with request ID, command ID, target tick, actor, payload), applied at the target tick's boundary in ascending command-ID order. When the new payload variants land, their fields (including the `f32` coordinates) must implement `CanonicalSerialize` (deterministic `to_le_bytes` for `f32`), keeping the §13.3 hashing rules and ADR-0001 numeric policy applicable. The slice adds these payload variants to `CommandPayload`:

- `Move { destination: (f32, f32) }` — path to destination and stop.
- `AttackMove { destination: (f32, f32) }` — path to destination; acquire and engage any enemy that enters weapon range along the way; stop at destination.
- `Attack { target: EntityId }` — engage the target entity until it dies or a new command replaces this one.

Command lifecycle states per §2.5 (accepted/active/suspended/completed/failed/cancelled) with observable failure reasons (invalid target, blocked route). Command replacement: a new command for a unit cancels its current active command. Commands referencing stale/dead `EntityId`s fail with a structured error (§6.2 stale-reference rule) and never resolve to a reused entity.

## 5. Movement

- Global path: A* on the 1 m grid, recomputed when a Move/AttackMove command is issued or when the unit has been blocked for more than 10 ticks.
- Per tick (`UpdateMovementIntent`/`ResolveMovement` phases): advance along the path at the unit's speed; stop within 0.5 m of the destination.
- Local avoidance: units keep a 1.0 m separation radius; overlapping pairs resolve deterministically in ascending `EntityId` order (lower ID keeps its position, higher ID is pushed out along the separation vector). No unit crosses an obstacle or another unit.
- All movement state is authoritative CPU state (ADR-0001); no wall-clock reads.

## 6. Combat

Resolved in the `ResolveCombat` phase each tick:

1. **Target acquisition:** for each unit with an active Attack/AttackMove command, consider living enemies within sight range (40 m); pick the nearest; tie-break by lowest `EntityId` (§9.4 deterministic tie-breaking).
2. **Movement vs firing:** if the target is outside weapon range, continue moving toward it (AttackMove also continues toward its destination). If within range, hold and fire.
3. **Firing:** when the weapon cooldown has elapsed and the target is within range, apply damage immediately (hitscan): `damage = max(1, weapon.damage - armor)`. Cooldown restarts at 20 ticks. A `DamageDealt` event is emitted.
4. **Simultaneous fire:** if both sides can fire on the same tick, apply damage in ascending `EntityId` order.
5. **Death:** when health ≤ 0, emit `UnitDestroyed`, release the entity (`EntityAllocator::release`, generation bumped), and remove it from targeting. Wreck cleanup is out of scope.

RNG is not used by any slice rule (no randomness in movement/combat); the seeded streams exist for later systems and are part of the replay/hash state.

## 7. Win / termination condition

The match ends at the first tick where one team has zero living units. At that tick the simulation emits `MatchOver { winner, tick }` and stops advancing (the runner is not stepped further; the final state hash is recorded). **Winner: Team A** in the scripted scenario below (lower `EntityId`s are allocated to Team A first and win same-tick ties).

## 8. Scripted acceptance scenario ("Crossing assault")

Fixed master RNG seed (any fixed value; slice rules are RNG-free), `TickRate::DEFAULT` (20 Hz). Initial state at tick 0: both teams spawned as in §3.

Scripted commands (all submitted before tick 1; command IDs assigned in submission order):

| # | tick | actor | payload |
|---|---|---|---|
| 1–3 | 1 | Team A (`ActorId(1)`) | `AttackMove { destination: (100, 100) }` for each of the 3 strikers |
| 4–6 | 1 | Team A | `AttackMove { destination: (100, 100) }` for each of the 3 skirmishers |
| 7–9 | 1 | Team B (`ActorId(2)`) | `AttackMove { destination: (100, 100) }` for each of the 3 strikers |
| 10–12 | 1 | Team B | `Move { destination: (110, 100) }` for each of the 3 skirmishers (hold just east of the chokepoint) |

Both armies converge on the chokepoint at (100, 100); both striker groups travel the same 80 m at the same speed and arrive on the same tick. The only asymmetry is the `EntityId` tie-break in simultaneous fire (§6 item 4), which Team A wins because its units are allocated first and have lower IDs.

**Worked timing expectations** (derivable from §5–§6; the test computes them from the rules rather than hardcoding):

- Distance from each spawn pad center to (100, 100) is 80 m; strikers at 0.2 m/tick take 400 ticks to arrive if unobstructed; the chokepoint and congestion only delay, never change, the deterministic outcome.
- First `DamageDealt` occurs at the tick when the first striker pair is within 10 m. The spawn pads are 160 m apart (x = 20 vs x = 180) and both striker groups close at 0.2 m/tick each (0.4 m/tick combined), so first contact is ≈ tick 375: (160 − 10) m ÷ 0.4 m/tick = 375 ticks. The fixed spawn layout offsets individual units by up to 2 m per axis and the chokepoint rocks/congestion only delay, so the test computes the 375-tick estimate from the rules and asserts the recorded first `DamageDealt` tick lies in a window around it (the issue #13 implementation records tick 384); the exact per-tick state is pinned by the replay digests.
- Each striker kill takes 10 hits × 20 ticks cooldown = 200 ticks after first contact with that target. Skirmishers (60 HP, 6 damage) die in 10 hits as well.
- Match ends with `MatchOver { winner: Team A }` no later than tick 2000 (100 s of simulation); the exact end tick is whatever the deterministic rules produce and is asserted via the recorded events and hashes, not hardcoded.

## 9. Headless test observations and replay checkpoints

The scenario runs fully headless (no window/GPU; `phalanx_runtime --headless` path or a `cargo test` integration test):

1. **Determinism/replay (M1 exit):** record the scenario with `ReplayRecorder` (commands as above, seed fixed), then `replay_recording` into a fresh runner reproduces **identical per-tick canonical state hashes** on the same build/platform. Replay checkpoints: record the canonical hash every 100 ticks (plus the final tick) in the `ReplayRecording`.
2. **Divergence detection:** replaying the recording with one command's payload altered must report `first_divergent_tick = Some(<the altered command's target tick or the tick its effect first lands>)`.
3. **Event assertions:** the recorded event stream contains `DamageDealt` from the expected first-contact tick and `UnitDestroyed` for exactly the 6 Team B units followed by `MatchOver { winner: Team A, .. }`. Under the §6 nearest-target rule, Team B's strikers focus the nearest Team A striker and destroy it before being wiped, so one Team A unit dying is the rule-derived outcome (the issue #13 implementation records one Team A striker destroyed at tick 470; the lower-`EntityId` tie-break only decides simultaneous same-tick volleys, it does not prevent focus fire). The exact event stream and end tick are pinned by the recorded events and replay hashes, not hardcoded.
4. **Chokepoint/movement test:** a variant scenario with a single striker issued `Move { destination: (180, 100) }` must produce a path through the wall lane (all path grid cells within z∈(62,138) while x∈[55,145]), the unit never overlaps another unit or obstacle (separation invariant checked every tick), and arrival at the destination within the rule-derived tick bound (path length / 0.2 m per tick ± deterministic congestion bound). The full path is part of replay state.
5. **CI budget:** the full scenario (≤ 2000 ticks) executes in under 5 s wall-clock on hosted runners; the test is part of `cargo test --workspace` on both OS targets.

## 10. Acceptance criteria mapping (issue #11)

- Rules explicit enough to implement → §§2–7 define map, roster, commands, movement, combat, and termination with exact numbers and tie-breaks.
- Unit definitions and map objects listed with placeholder values → §2 table, §3 table.
- Movement/chokepoint test described → §9 item 4.
- Deterministic, testable combat outcome → §6 rules + §8 worked timing + §9 items 1–3.
- Headless completion criterion → §9 (hash-identical replay on same build/platform, divergence tick, event assertions, ≤ 2000 ticks, < 5 s CI).
- Small enough for fast CI → 200×200 m map, 12 units, ≤ 2000 ticks.
