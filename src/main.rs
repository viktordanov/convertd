//! `convertd`: the command line and the wiring (lane 001; lane 002's
//! dispatch goes through the lead).

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use convertd::config::{self, Config, Env};
use convertd::engines::cli::EngineCommand;
use convertd::rest::state::{AppState, Engines};
use convertd::rest::{healthcheck, openapi, server};
use convertd::worker::Role;
use convertd::{engines, jobs, logging, runtime, worker};

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

/// The spec as pretty JSON and one newline on stdout, nothing else. Reads
/// no environment and starts no logger.
fn openapi() -> ExitCode {
    let mut out = std::io::stdout().lock();
    match writeln!(out, "{}", openapi::spec_json()).and_then(|()| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("convertd openapi: failed to write the spec: {err}");
            ExitCode::FAILURE
        }
    }
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
    prepare_tmp_dir(&config)?;
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
        let selected = convertd::engines::select::start(&config).await?;
        let engines = Engines {
            office: selected.office,
            pdf: selected.pdf,
            readiness: selected.readiness,
        };
        let state = AppState::new(&config, engines);
        convertd::observability::metrics::spawn_upkeep(METRICS_UPKEEP);
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        // In its own task, so that it keeps accepting (and answering 503)
        // while the drain below waits.
        let mut serving = tokio::spawn(server::serve(
            listener,
            server::router(state.clone()),
            async move {
                let _ = stopped.await;
            },
        ));
        let joined = |result: Result<std::io::Result<()>, tokio::task::JoinError>| match result {
            Ok(served) => served.map_err(|err| format!("failed to serve: {err}")),
            Err(err) => Err(format!("failed to serve: {err}")),
        };
        let served = tokio::select! {
            result = &mut serving => joined(result),
            () = shutdown => {
                // Drain: readyz says draining and new conversions get 503
                // while the platform removes the endpoint; requests in
                // flight finish within SHUTDOWN_GRACE_SECONDS.
                let grace = config.server.shutdown_grace;
                server::begin_drain(&state);
                selected.handle.drain().await;
                if !server::wait_idle(&state, grace).await {
                    tracing::warn!(grace_seconds = grace.as_secs(), "requests still in flight after the grace period");
                }
                let _ = stop.send(());
                // axum now stops accepting and finishes open connections.
                match tokio::time::timeout(CLOSE_GRACE, &mut serving).await {
                    Ok(result) => joined(result),
                    Err(_) => {
                        tracing::warn!("open connections did not close in time");
                        serving.abort();
                        Ok(())
                    }
                }
            }
        };
        selected.handle.shutdown(config.server.shutdown_grace).await;
        served?;
        tracing::info!("server stopped");
        Ok(())
    })
}

/// How often histogram samples are folded into their buckets.
const METRICS_UPKEEP: std::time::Duration = std::time::Duration::from_secs(5);

/// How long open connections get after the listener closes.
const CLOSE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Creates `TMP_DIR` and removes the job directories a crash left.
fn prepare_tmp_dir(config: &Config) -> Result<(), String> {
    let dir = &config.server.tmp_dir;
    std::fs::create_dir_all(dir)
        .map_err(|err| format!("failed to create TMP_DIR {}: {err}", dir.display()))?;
    match jobs::job_dir::sweep_stale(dir) {
        Ok(0) => {}
        Ok(removed) => tracing::info!(removed, "removed stale job directories"),
        Err(err) => tracing::warn!(error = %err, "failed to sweep TMP_DIR"),
    }
    Ok(())
}
