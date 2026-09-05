//! The `lightagent` command: an interactive agent harness with a welcome mark.

use std::process::ExitCode;

fn main() -> ExitCode {
    lightagent::run_cli()
}
