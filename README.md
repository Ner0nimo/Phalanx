# Phalanx

Phalanx is an original large-scale real-time strategy project. Its current
repository milestone establishes a small Rust workspace and a CPU-only
headless runtime path.

## Requirements

- Stable Rust (the repository toolchain file selects the stable channel).
- Cargo, installed with the standard Rust toolchain.

No graphics SDK, window system, GPU, or renderer is required for the headless
bootstrap.

## Workspace

- `phalanx_core`: dependency-free CPU-side simulation primitives.
- `phalanx_runtime`: executable entry point; currently exposes the headless
  infrastructure smoke scenario.

The smoke scenario validates that the core can advance deterministic ticks
without graphics initialization. It is infrastructure scaffolding, not a
gameplay simulation. The authoritative simulation schedule and gameplay
systems will be introduced in subsequent milestones.

## Build and test

From the repository root:

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Run headless

Run the deterministic CPU-only smoke scenario (8 ticks by default):

```sh
cargo run -p phalanx_runtime -- --headless
```

Choose an explicit tick count:

```sh
cargo run -p phalanx_runtime -- --headless --ticks 32
```

### Windows PowerShell

The same Cargo commands work in PowerShell from the repository root:

```powershell
cargo build --workspace
cargo test --workspace
cargo run -p phalanx_runtime -- --headless --ticks 32
```

### Linux

The same commands work in a terminal from the repository root. A C/C++
toolchain or graphics development packages are not required by this workspace.

## Continuous integration

GitHub Actions runs formatting, Clippy, tests, a workspace build, and the
headless smoke scenario on both Windows and Linux for pushes to `main` and
pull requests targeting `main`.
