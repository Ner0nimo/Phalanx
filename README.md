# Phalanx

Phalanx is an original large-scale real-time strategy game and engine prototype written in Rust. The project is past its initial headless-runtime bootstrap: it now has a deterministic CPU-authoritative simulation, a movement/combat vertical slice, replay and state-hash infrastructure, and an interactive graphical battle prototype.

The current graphical renderer is still an early presentation prototype. It uses wgpu/winit and 2D orthographic debug geometry for the Crossing battle; it is not yet the production 3D/PBR renderer. The next rendering steps are tracked in [#104](https://github.com/Ner0nimo/Phalanx/issues/104), [#98](https://github.com/Ner0nimo/Phalanx/issues/98), and [#96–#102](https://github.com/Ner0nimo/Phalanx/issues).

## Current capabilities

- Deterministic fixed-tick, CPU-authoritative simulation that runs without a window or graphics device.
- A* navigation on the Crossing map, basic local movement/avoidance, command ingress, combat, destruction, and autonomous opponent behavior.
- Canonical state hashing, in-memory snapshot/restore, and deterministic replay of the same-build scenario.
- Graphical battle mode with basic unit selection, camera pan/zoom, Attack/AttackMove commands, command queue append/cancel, health bars, selection rings, and order markers.
- Windows/Linux CI for formatting, Clippy, tests, workspace builds, headless smoke, dependency audit, and secret scanning.

The larger operational RTS systems (territory, supply, economy, production, formations, fog of war, and skirmish AI) are planned but not yet implemented. See [RtsEngineDoc.md](RtsEngineDoc.md) and the [open issues](https://github.com/Ner0nimo/Phalanx/issues).

## Requirements

- Stable Rust (the repository toolchain file selects the stable channel).
- Cargo, installed with the standard Rust toolchain.

CPU-only headless simulation does not require a graphics SDK, window system, or GPU. Graphical mode requires a working window system and a wgpu-compatible graphics adapter/backend.

## Build and test

From the repository root:

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Run headless

Run the deterministic CPU-only scenario:

```sh
cargo run -p phalanx_runtime -- --headless
cargo run -p phalanx_runtime -- --headless --ticks 2000 --seed 42
```

Headless execution never initializes wgpu or creates a window.

## Run the graphical battle

On a desktop with a working graphics adapter:

```sh
cargo run -p phalanx_runtime -- --graphical --battle
```

To enable the developer FPS/frametime overlay:

```sh
cargo run -p phalanx_runtime -- --graphical --battle --perf-overlay
```

Current controls:

- **WASD / arrow keys:** pan the camera.
- **Mouse wheel:** zoom.
- **Left click:** select the nearest friendly unit.
- **Left drag:** select friendly units in a rectangle.
- **Right click:** Attack a clicked enemy or AttackMove to the ground location.
- **Shift + right click:** append the order to the selected units' queues.
- **Delete / Backspace:** cancel selected units' active and queued commands.
- **Escape:** clear selection.

The current HUD is geometry-based and does not yet include a text renderer or full RTS interface. Camera rotation, edge-pan, control groups, additive selection, and strategic overlays are also future work.

## Run the offscreen rendering probe

```sh
cargo run -p phalanx_runtime -- --graphical --offscreen --battle --frames 60 --ticks 2000 --screenshot crossing.ppm
```

This exercises offscreen rendering and pixel verification when an adapter is available. On GPU-less machines, the runtime reports a structured no-adapter result while still running the headless battle path.

## Architecture and roadmap

- [RtsEngineDoc.md](RtsEngineDoc.md) — product, simulation, gameplay, rendering, and milestone architecture.
- [docs/vertical-slice-scenario.md](docs/vertical-slice-scenario.md) — deterministic Crossing battle specification and acceptance test.
- [docs/adr/](docs/adr/) — accepted architecture decisions and technical constraints.
- [docs/ISSUE_LABELS.md](docs/ISSUE_LABELS.md) — issue type, priority, area, and phase conventions.

The immediate work is to finish the M1 3D presentation proof, then build the operational RTS systems in M2 before scaling/content/product-readiness work in M3–M5.
