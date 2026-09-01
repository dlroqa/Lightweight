//! The `hermes` command: the original name, unchanged.

use std::process::ExitCode;

fn main() -> ExitCode {
    lightweight_cli::run_cli(lightweight_cli::Personality::Hermes)
}
