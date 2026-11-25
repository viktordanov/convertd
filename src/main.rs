//! `convertd`: the command line and the wiring (lane 001; lane 002's
//! dispatch goes through the lead).

use std::ffi::OsString;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use convertd::config::{self, Config, Env};
use convertd::engines::cli::EngineCommand;
use convertd::rest::{healthcheck, server};
use convertd::worker::Role;
use convertd::{engines, logging, runtime, worker};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(name = "convertd", version, about = "Document conversion service")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP server.
    Serve,
    /// Print the OpenAPI 3.1 document as JSON on stdout.
    Openapi,
    /// Exit 0 when GET /readyz on 127.0.0.1:$PORT answers 200 within 2 s.
    Healthcheck,
    /// Run an engine worker (internal; spawned by the supervisor).
    #[command(hide = true)]
    Worker {
        #[arg(value_enum)]
        role: WorkerRole,
    },
    /// Run an engine without HTTP (debugging and benchmarks).
    #[command(hide = true)]
    Engine {
        #[arg(value_enum)]
        command: EngineArg,
        /// Passed unparsed to the engine command.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum WorkerRole {
    Office,
    Render,
    #[cfg(feature = "test-hooks")]
    Test,
}

impl From<WorkerRole> for Role {
    fn from(role: WorkerRole) -> Self {
        match role {
            WorkerRole::Office => Role::Office,
            WorkerRole::Render => Role::Render,
            #[cfg(feature = "test-hooks")]
            WorkerRole::Test => Role::Test,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum EngineArg {
    Convert,
    Inspect,
    Render,
    Assemble,
    Decrypt,
    Bench,
    Soak,
}

impl From<EngineArg> for EngineCommand {
    fn from(arg: EngineArg) -> Self {
        match arg {
            EngineArg::Convert => EngineCommand::Convert,
            EngineArg::Inspect => EngineCommand::Inspect,
            EngineArg::Render => EngineCommand::Render,
            EngineArg::Assemble => EngineCommand::Assemble,
            EngineArg::Decrypt => EngineCommand::Decrypt,
            EngineArg::Bench => EngineCommand::Bench,
            EngineArg::Soak => EngineCommand::Soak,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        // First, and alone: a worker gets no runtime, logging or configuration.
        Command::Worker { role } => worker::run(role.into()),
        Command::Engine { command, args } => engines::cli::run(command.into(), args),
        Command::Openapi => openapi(),
        Command::Healthcheck => healthcheck(),
        Command::Serve => match serve() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("convertd: {err}");
                ExitCode::FAILURE
            }
        },
    }
}

/// Skeleton: an empty document until lane 001 step 3 emits the spec.
fn openapi() -> ExitCode {
    println!("{{}}");
    ExitCode::SUCCESS
}

fn healthcheck() -> ExitCode {
    let port = match config::port(&Env::from_process()) {
        Ok(port) => port,
        Err(err) => {
            eprintln!("convertd healthcheck: {err}");
            return ExitCode::FAILURE;
        }
    };
    match healthcheck::check(port, healthcheck::TIMEOUT) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("convertd healthcheck: {err}");
            ExitCode::FAILURE
        }
    }
}

fn serve() -> Result<(), String> {
    let config = Config::from_process().map_err(|err| err.to_string())?;
    logging::init(config.server.log_mode).map_err(|err| err.to_string())?;
    let runtime = runtime::serve(config.server.tokio_worker_threads)
        .map_err(|err| format!("failed to build the runtime: {err}"))?;

    runtime.block_on(async move {
        let shutdown = server::shutdown_signal()
            .map_err(|err| format!("failed to install the signal handlers: {err}"))?;
        let (host, port) = (config.server.host, config.server.port);
        let listener = server::bind(host, port)
            .await
            .map_err(|err| format!("failed to bind {host}:{port}: {err}"))?;
        let addr = listener
            .local_addr()
            .map_err(|err| format!("failed to read the bound address: {err}"))?;
        tracing::info!(addr = %addr, "server listening");
        server::serve(listener, server::router(), shutdown)
            .await
            .map_err(|err| format!("failed to serve: {err}"))?;
        tracing::info!("server stopped");
        Ok(())
    })
}
