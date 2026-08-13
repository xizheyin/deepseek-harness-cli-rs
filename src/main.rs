use std::{ffi::OsString, process::ExitCode};

const HELP: &str = "dsh - Rust CLI foundation for DeepSeek Harness

Usage: dsh [OPTIONS]

Options:
  -h, --help       Print help
  -V, --version    Print version

The interactive agent is not implemented yet.
";

fn main() -> ExitCode {
    let arguments = match std::env::args_os()
        .skip(1)
        .map(OsString::into_string)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(arguments) => arguments,
        Err(_) => {
            eprintln!("error: command-line arguments must be valid Unicode");
            return ExitCode::from(2);
        }
    };

    match arguments.as_slice() {
        [argument] if argument == "--help" || argument == "-h" => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        [argument] if argument == "--version" || argument == "-V" => {
            println!("dsh {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        [] => {
            eprintln!("error: the interactive agent is not implemented yet; use --help");
            ExitCode::from(2)
        }
        arguments => {
            eprintln!("error: unsupported arguments: {}", arguments.join(" "));
            eprintln!("use --help to see the currently available options");
            ExitCode::from(2)
        }
    }
}
