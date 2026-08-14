#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    deepseek_harness_cli::cli::entry()
}
