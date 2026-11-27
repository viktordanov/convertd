//! `corpus-bench`: checks the rs-convert corpus and runs it against the
//! service. It has `validate` and `check`; the bench,
//! soak and compare subcommands come in later steps.

mod check;
mod expected;
mod validate;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "corpus-bench", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct CorpusArgs {
    /// The directory the `path` fields are relative to.
    #[arg(long, default_value = "corpus")]
    corpus: PathBuf,
    /// Defaults to <corpus>/expected.json.
    #[arg(long)]
    expected: Option<PathBuf>,
    /// Defaults to <corpus>/expected.schema.json.
    #[arg(long)]
    schema: Option<PathBuf>,
}

impl CorpusArgs {
    fn paths(self) -> validate::Paths {
        let mut paths = validate::Paths::in_corpus(&self.corpus);
        if let Some(expected) = self.expected {
            paths.expected = expected;
        }
        if let Some(schema) = self.schema {
            paths.schema = schema;
        }
        paths
    }
}

#[derive(Subcommand)]
enum Command {
    /// Check expected.json against its schema and the files on disk. Needs no server.
    Validate {
        #[command(flatten)]
        corpus: CorpusArgs,
    },
    /// Wait for connections on the canary listener address: the positive
    /// control for the canaries. Exits 0 once it has
    /// counted --expect connections, 1 at the timeout.
    Listen {
        #[arg(long, default_value = "127.0.0.1:47123")]
        addr: String,
        #[arg(long, default_value_t = 1)]
        expect: u64,
        /// Seconds to wait.
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
    /// Run every case against a service, with the canary listener up.
    Check {
        #[command(flatten)]
        corpus: CorpusArgs,
        /// The service's base URL.
        #[arg(long, env = "CONVERT_HOST", default_value = "http://127.0.0.1:8080")]
        target: String,
        /// The bearer key.
        #[arg(long, env = "CONVERT_API_KEY", hide_env_values = true)]
        api_key: String,
        /// Run only the files with one of these tags (comma-separated).
        #[arg(long, value_delimiter = ',')]
        tags: Vec<String>,
        /// Run only these file ids (comma-separated).
        #[arg(long, value_delimiter = ',')]
        ids: Vec<String>,
        /// Write every case's result to this JSON file.
        #[arg(long)]
        report: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Validate { corpus } => validate_command(&corpus.paths()),
        Command::Listen {
            addr,
            expect,
            timeout,
        } => listen_command(&addr, expect, timeout),
        Command::Check {
            corpus,
            target,
            api_key,
            tags,
            ids,
            report,
        } => check::run(check::Args {
            paths: corpus.paths(),
            target,
            api_key,
            tags,
            ids,
            report,
        }),
    }
}

fn listen_command(addr: &str, expect: u64, timeout: u64) -> ExitCode {
    let listener = match check::listener::Listener::start(addr) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("listen: {err}");
            return ExitCode::FAILURE;
        }
    };
    println!("listen: listening on {}", listener.addr());
    let count = listener.wait_for(expect, std::time::Duration::from_secs(timeout));
    // Let the thread store the line of the last connection.
    std::thread::sleep(std::time::Duration::from_millis(200));
    for line in listener.seen() {
        println!("listen: connection from {line}");
    }
    if count >= expect {
        println!("listen: {count} connection(s), expected at least {expect}");
        ExitCode::SUCCESS
    } else {
        eprintln!("listen: {count} connection(s) in {timeout} s, expected at least {expect}");
        ExitCode::FAILURE
    }
}

fn validate_command(paths: &validate::Paths) -> ExitCode {
    let outcome = validate::run(paths);
    for note in &outcome.notes {
        eprintln!("validate: note: {note}");
    }
    for problem in &outcome.problems {
        eprintln!("validate: {problem}");
    }
    if outcome.passed() {
        println!(
            "validate: {} files and {} cases in {} pass",
            outcome.files,
            outcome.cases,
            paths.expected.display()
        );
        ExitCode::SUCCESS
    } else {
        eprintln!("validate: {} problems", outcome.problems.len());
        ExitCode::FAILURE
    }
}
