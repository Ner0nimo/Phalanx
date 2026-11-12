use std::process::ExitCode;

use phalanx_core::HeadlessSmokeScenario;

fn main() -> ExitCode {
    match run(std::env::args().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: impl Iterator<Item = String>) -> Result<(), String> {
    let arguments: Vec<String> = args.collect();
    let mut headless = false;
    let mut ticks = 8_u64;
    let mut index = 0;

    while index < arguments.len() {
        match arguments[index].as_str() {
            "--headless" => headless = true,
            "--ticks" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or("--ticks requires a non-negative integer")?;
                ticks = value
                    .parse()
                    .map_err(|_| format!("invalid tick count: {value}"))?;
            }
            "--help" | "-h" => {
                println!(
                    "Phalanx runtime (bootstrap)\n\nUSAGE:\n    phalanx_runtime --headless [--ticks COUNT]\n\nRuns the deterministic CPU-only infrastructure smoke scenario."
                );
                return Ok(());
            }
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
        index += 1;
    }

    if !headless {
        return Err("this bootstrap runtime currently supports only --headless".to_owned());
    }

    let mut scenario = HeadlessSmokeScenario::new();
    for _ in 0..ticks {
        scenario.advance_tick().map_err(|error| error.to_string())?;
    }

    println!(
        "Phalanx headless smoke run: ticks={}, digest=0x{:016x}",
        scenario.tick(),
        scenario.digest()
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
}
