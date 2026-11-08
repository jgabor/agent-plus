//! The `ap` command.

use std::process::ExitCode;

const USAGE: &str = "usage: ap --version";

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && args[0] == "--version" {
        println!("ap {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    eprintln!("{USAGE}");
    ExitCode::from(2)
}
