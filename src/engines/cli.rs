//! `convertd engine …`: the engines without HTTP, for debugging and benchmarks (lane 002).

use std::ffi::OsString;
use std::process::ExitCode;

/// The hidden `engine` subcommands (INTERFACES.md §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineCommand {
    Convert,
    Inspect,
    Render,
    Assemble,
    Decrypt,
    Bench,
    Soak,
}

impl EngineCommand {
    /// The name used on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            EngineCommand::Convert => "convert",
            EngineCommand::Inspect => "inspect",
            EngineCommand::Render => "render",
            EngineCommand::Assemble => "assemble",
            EngineCommand::Decrypt => "decrypt",
            EngineCommand::Bench => "bench",
            EngineCommand::Soak => "soak",
        }
    }
}

/// Runs one engine command. `args` are the arguments after the command
/// name, unparsed, so this lane defines its own flags without touching
/// `main.rs`.
///
/// Skeleton stub: prints "not implemented" and exits with status 2.
pub fn run(command: EngineCommand, args: Vec<OsString>) -> ExitCode {
    let _ = args;
    eprintln!("convertd engine {}: not implemented", command.as_str());
    ExitCode::from(2)
}
