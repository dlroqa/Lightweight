//! The `lightweight` command: the inference gateway CLI.

use std::process::ExitCode;

fn main() -> ExitCode {
    lightweight_cli::run_cli()
}
