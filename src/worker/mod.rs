//! Engine worker processes (lane 002): the only place with `unsafe` and engine calls.
//!
//! `main.rs` dispatches `convertd worker <role>` here before any runtime,
//! logging or configuration exists.

/// The role a worker process runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// LibreOfficeKit: office documents to PDF.
    Office,
    /// PDFium: inspect, render, assemble and decrypt PDFs.
    Render,
    /// Fake answers driven by `FAKE:` markers (INTERFACES.md §10).
    #[cfg(feature = "test-hooks")]
    Test,
}

impl Role {
    /// The name used on the command line and in logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Office => "office",
            Role::Render => "render",
            #[cfg(feature = "test-hooks")]
            Role::Test => "test",
        }
    }
}

/// Runs a worker until it leaves. A worker never returns: it ends with `_exit`.
///
/// Skeleton stub: prints "not implemented" and exits with status 2.
pub fn run(role: Role) -> ! {
    eprintln!("convertd worker {}: not implemented", role.as_str());
    std::process::exit(2)
}
