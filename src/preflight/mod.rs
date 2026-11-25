//! Pure pre-flight scanners over an office source (lane 001 writes them).
//!
//! The office worker (lane 002) calls [`scan`] on the source file before
//! LibreOffice loads it and returns the facts in `Converted.facts`
//! (INTERFACES.md §13). The signature is fixed; lane 001 fills the body from
//! 005 §5 with the caps in its plan. No tokio, no network, no engine calls.

use std::path::Path;

use crate::domain::document::Family;
use crate::domain::verification::SourceFacts;

/// Reads the facts that the warning rules need from an office source.
///
/// Never fails: a part that is missing, malformed or over a cap is recorded
/// in `SourceFacts::skipped`, and the check it feeds is skipped.
pub fn scan(source: &Path, family: Family) -> SourceFacts {
    let _ = (source, family);
    SourceFacts::default()
}
