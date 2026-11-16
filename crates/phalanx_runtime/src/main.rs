use std::process::ExitCode;

use phalanx_core::hash::Digest;
use phalanx_core::scenario::{SCENARIO_SEED, scenario_commands, setup_scenario};
use phalanx_core::simulation::{SimulationRunner, TickRate};
use phalanx_render::{
    BackendError, ClearColor, DEFAULT_CLEAR, OffscreenTarget, init_headless, run_windowed,
    verify_clear_pixels,
};

fn main() -> ExitCode {
    match run(std::env::args().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

/// The result of one headless run: the final tick, the checkpoint digest of
/// the full authoritative state, and how many commands were applied.
struct HeadlessResult {
    tick: u64,
    digest: Digest,
    applied_commands: u64,
}

/// Runs the deterministic CI scenario through a real `SimulationRunner`
/// (issue #54): registers the scenario system, submits the scenario command
/// stream (exercising command ingress), advances exactly `ticks` ticks, and
/// reports the canonical checkpoint digest. No window, graphics API, or GPU
/// is involved.
fn headless_run(seed: u64, ticks: u64) -> Result<HeadlessResult, String> {
    let mut runner = SimulationRunner::new(TickRate::DEFAULT, seed);
    setup_scenario(&mut runner);
    for request in scenario_commands() {
        runner
            .submit_command(request)
            .map_err(|error| error.to_string())?;
    }
    runner
        .advance_ticks(ticks)
        .map_err(|error| error.to_string())?;
    Ok(HeadlessResult {
        tick: runner.simulation().tick(),
        digest: runner.canonical_digest(),
        applied_commands: runner.simulation().applied_command_count(),
    })
}

/// The offscreen verification frame size (issue #61).
const OFFSCREEN_SIZE: u32 = 64;

/// Runs graphical mode (issue #61). `--offscreen` renders `frames` clear
/// frames to an offscreen texture and verifies the readback pixels, so the
/// presentation path is exercised and verifiable without a display; without
/// `--offscreen` a native window presents frames until closed. On machines
/// without a GPU adapter the offscreen probe prints the structured
/// no-adapter result and exits 0 (CI convention from the graphics spike).
fn run_graphical(offscreen: bool, frames: u64, clear: ClearColor) -> Result<(), String> {
    if !offscreen {
        return run_windowed(clear, "Phalanx graphical mode").map_err(|error| error.to_string());
    }
    match init_headless() {
        Err(BackendError::NoAdapter(message)) => {
            println!("ADAPTER candidate=phalanx_render found=false error=\"{message}\"");
            println!("FRAME offscreen skipped: no GPU adapter (headless probe result)");
            Ok(())
        }
        Err(error) => Err(error.to_string()),
        Ok((backend, report)) => {
            println!("{report}");
            let target = OffscreenTarget::new(&backend.device, OFFSCREEN_SIZE, OFFSCREEN_SIZE)
                .map_err(|error| error.to_string())?;
            for _ in 0..frames {
                target.render_clear(&backend.device, &backend.queue, clear);
            }
            let pixels = target
                .read_back(&backend.device, &backend.queue)
                .map_err(|error| error.to_string())?;
            verify_clear_pixels(&pixels, target.width, target.height, clear)
                .map_err(|error| format!("offscreen frame verification failed: {error}"))?;
            println!(
                "FRAME offscreen frames={frames} size={}x{} clear=verified pixels ok",
                target.width, target.height
            );
            Ok(())
        }
    }
}

fn parse_u64(value: &str) -> Result<u64, String> {
    if let Some(hex) = value.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).map_err(|_| format!("invalid seed: {value}"))
    } else {
        value.parse().map_err(|_| format!("invalid seed: {value}"))
    }
}

fn run(args: impl Iterator<Item = String>) -> Result<(), String> {
    let arguments: Vec<String> = args.collect();
    let mut headless = false;
    let mut graphical = false;
    let mut offscreen = false;
    let mut frames = 3_u64;
    let mut clear = DEFAULT_CLEAR;
    let mut ticks = 8_u64;
    let mut seed = SCENARIO_SEED;
    let mut index = 0;

    while index < arguments.len() {
        match arguments[index].as_str() {
            "--headless" => headless = true,
            "--graphical" => graphical = true,
            "--offscreen" => offscreen = true,
            "--frames" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or("--frames requires a non-negative integer")?;
                frames = value
                    .parse()
                    .map_err(|_| format!("invalid frame count: {value}"))?;
            }
            "--clear" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or("--clear requires RRGGBB hex (e.g. 0x18304a)")?;
                clear = ClearColor::parse(value)?;
            }
            "--ticks" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or("--ticks requires a non-negative integer")?;
                ticks = value
                    .parse()
                    .map_err(|_| format!("invalid tick count: {value}"))?;
            }
            "--seed" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or("--seed requires an unsigned integer")?;
                seed = parse_u64(value)?;
            }
            "--help" | "-h" => {
                println!(
                    "Phalanx runtime (bootstrap)\n\nUSAGE:\n    phalanx_runtime --headless [--ticks COUNT] [--seed SEED]\n    phalanx_runtime --graphical [--clear RRGGBB]\n    phalanx_runtime --graphical --offscreen [--frames N] [--clear RRGGBB]\n\n--headless runs the deterministic CPU-only scenario through SimulationRunner (no graphics initialization). --graphical presents wgpu frames in a native window (phalanx_render); on headless machines use --graphical --offscreen, which renders clear frames to an offscreen texture and verifies the pixels."
                );
                return Ok(());
            }
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
        index += 1;
    }

    if graphical {
        return run_graphical(offscreen, frames, clear);
    }
    if !headless {
        return Err(
            "this runtime supports --headless or --graphical [--offscreen] (see --help)".to_owned(),
        );
    }

    let result = headless_run(seed, ticks)?;
    let hex: String = result
        .digest
        .value()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    println!(
        "Phalanx headless run: ticks={}, seed=0x{seed:016x}, digest=0x{hex}, applied_commands={}",
        result.tick, result.applied_commands
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::run;

    #[test]
    fn headless_runtime_advances_without_graphics_initialization() {
        assert!(
            run(["--headless", "--ticks", "5"]
                .into_iter()
                .map(str::to_owned))
            .is_ok()
        );
    }

    #[test]
    fn runtime_requires_explicit_headless_mode() {
        let error = run(std::iter::empty()).expect_err("missing mode must be rejected");
        assert!(error.contains("--headless"));
    }

    #[test]
    fn runtime_rejects_invalid_tick_count() {
        let error = run(["--headless", "--ticks", "many"]
            .into_iter()
            .map(str::to_owned))
        .expect_err("invalid tick count must be rejected");
        assert!(error.contains("invalid tick count"));
    }

    #[test]
    fn graphical_offscreen_mode_runs_without_a_display() {
        // With a GPU adapter this renders and verifies clear frames; without
        // one it prints the structured no-adapter probe result. Both exit 0.
        assert!(
            run(["--graphical", "--offscreen", "--frames", "2"]
                .into_iter()
                .map(str::to_owned))
            .is_ok()
        );
    }

    #[test]
    fn graphical_mode_rejects_invalid_clear_color() {
        let error = run(["--graphical", "--offscreen", "--clear", "nope"]
            .into_iter()
            .map(str::to_owned))
        .expect_err("invalid clear color must be rejected");
        assert!(error.contains("invalid clear color"));
    }

    #[test]
    fn offscreen_requires_graphical_mode() {
        let error = run(["--offscreen"].into_iter().map(str::to_owned))
            .expect_err("--offscreen without --graphical must be rejected");
        assert!(error.contains("--graphical"));
    }

    #[test]
    fn headless_run_uses_the_real_runner_and_is_deterministic() {
        use super::{SCENARIO_SEED, headless_run, scenario_commands};

        // 16 ticks: all 8 scenario commands (target ticks 2..=16) are applied.
        let first = headless_run(SCENARIO_SEED, 16).expect("headless run");
        let second = headless_run(SCENARIO_SEED, 16).expect("headless run");
        assert_eq!(first.tick, 16);
        assert_eq!(first.digest, second.digest, "identical seed and inputs");
        assert_eq!(
            first.applied_commands,
            scenario_commands().len() as u64,
            "the scenario command stream went through command ingress"
        );
        let other = headless_run(SCENARIO_SEED + 1, 16).expect("headless run");
        assert_ne!(first.digest, other.digest, "a different seed diverges");
    }
}
