//! `corpus-bench`: checks the rs-convert corpus and, in later steps, the
//! service against it (ledger item 005). This step has `validate` only.

mod expected;
mod validate;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "corpus-bench", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check expected.json against its schema and the files on disk. Needs no server.
    Validate {
        /// The directory the `path` fields are relative to.
        #[arg(long, default_value = "corpus")]
        corpus: PathBuf,
        /// Defaults to <corpus>/expected.json.
        #[arg(long)]
        expected: Option<PathBuf>,
        /// Defaults to <corpus>/expected.schema.json.
        #[arg(long)]
        schema: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Validate {
            corpus,
            expected,
            schema,
        } => {
            let mut paths = validate::Paths::in_corpus(&corpus);
            if let Some(expected) = expected {
                paths.expected = expected;
            }
            if let Some(schema) = schema {
                paths.schema = schema;
            }
            let outcome = validate::run(&paths);
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
    }
}
