//! The `lightweight` command: the same tool, with a welcome mark.

use std::process::ExitCode;

fn main() -> ExitCode {
    lightweight_cli::run_cli(lightweight_cli::Personality::Lightweight)
}
