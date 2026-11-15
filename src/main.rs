mod accounting;
mod clear;
mod cli;
mod data_dir;
mod lineage;
mod recorder;
mod report;
mod rollup;
mod size;
mod snapshot;
mod sort_key;
mod span;
mod store;
mod threshold;

use std::process::ExitCode;

use anyhow::Result;

use crate::cli::{Command, USAGE, parse_args};

fn main() -> ExitCode {
    let command = match parse_args(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("pw: {message}");
            return ExitCode::from(2);
        }
    };
    match run(command) {
        Ok(output) => {
            print!("{output}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("pw: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the command, answering what it prints; recording runs until stopped.
fn run(command: Command) -> Result<String> {
    match command {
        Command::Help => Ok(USAGE.to_owned()),
        Command::Version => Ok(format!("pw {}\n", env!("CARGO_PKG_VERSION"))),
        Command::Record(opts) => recorder::record(&opts).map(|()| String::new()),
        Command::Clear(dir) => clear::clear(dir.as_deref()),
        Command::Report(opts) => report::report(&opts),
    }
}
