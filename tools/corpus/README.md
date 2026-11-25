# corpus-bench

`corpus-bench` is the Rust harness that checks the corpus in [`corpus/`](../../corpus/README.md) and, in later steps of item 005, measures the service against it.

The crate is not part of the `convertd` workspace. It has its own `[workspace]` table and its own `Cargo.lock`, so the root `cargo build` never builds it, and its dependencies stay out of the service's lock file. Run Cargo here, or give `--manifest-path tools/corpus/Cargo.toml`.

## Contents

1. [Why a separate crate](#why-a-separate-crate)
2. [Build and test](#build-and-test)
3. [validate](#validate)
4. [Later subcommands](#later-subcommands)
5. [Results over time](#results-over-time)

<!-- memoria:section id="overview" files="Cargo.toml clippy.toml" -->
## Why a separate crate

The harness is an independent client of the service. Its later subcommands will read the API with their own lenient types, not with the server's types. Thus a server change cannot also change what the harness expects. Its future dependencies, such as an HTTP client and a PDF reader, stay out of the lock file of `convertd`.

`clippy.toml` in this folder replaces the root one. Clippy reads the nearest `clippy.toml` up the tree, and the root file forbids blocking waits on child processes for the reaper of `convertd`. This harness has no reaper, and `validate` waits on `git`.
<!-- /memoria:section -->

<!-- memoria:section id="setup" files="Cargo.toml src/main.rs" -->
## Build and test

The crate uses the Rust toolchain that `rust-toolchain.toml` pins at the repository root.

1. Go to `tools/corpus`.
2. Run `cargo fmt --check`.
3. Run `cargo clippy --locked --all-targets -- -D warnings`.
4. Run `cargo test --locked`.

The unit tests build small corpus folders in a temporary directory. They prove that `validate` fails on a missing file, a wrong SHA-256, an unlisted file under `out/` or `fixtures/`, and a sample of the schema and semantic rules. One test applies the schema and the semantic checks to the committed `corpus/expected.json`, without the generated files.
<!-- /memoria:section -->

<!-- memoria:section id="usage" files="src/main.rs src/validate.rs src/expected.rs" -->
## validate

`validate` checks `expected.json` against its schema and against the files on disk. It needs no server. Generate the corpus with `--hostile` first, as [the corpus README](../../corpus/README.md#generation) shows. Then run it from the repository root:

```sh
cargo run --manifest-path tools/corpus/Cargo.toml -- validate
```

The `--corpus` flag names the folder that the `path` fields are relative to, by default `corpus`. `--expected` and `--schema` override `<corpus>/expected.json` and `<corpus>/expected.schema.json`.

The checks run in this order:

1. The JSON Schema in `expected.schema.json`. If it fails, `validate` stops here.
2. The rules that a schema cannot state: unique ids, paths and case names, no `.` or `..` path parts, `min` not more than `max`, no blank page past an exact page count, known warning detail keys, and sorted requested fonts with no duplicates.
3. Each listed file exists as a regular file, not a link, and matches its `sha256`.
4. No file under `out/` or `fixtures/` is missing from `expected.json`. `validate` ignores `.DS_Store` files.
5. Git tracks nothing under `out/` or `private/`. Outside a git work tree, `validate` skips this check with a note.

`validate` prints each problem on its own line and exits 1 if it finds a problem. It prints notes, such as the cases whose page count is still `"unrecorded"`, without a failure. On a pass it prints the number of files and cases and exits 0.

A pass proves that the expectations are well formed and that the files on disk are the files that `expected.json` describes. It does not prove that the service gives those answers. That is the job of `check`.
<!-- /memoria:section -->

## Later subcommands

`validate` is the only subcommand at this time. The 005 plan adds these subcommands in later steps:

| Subcommand | What it will do | Plan step |
| --- | --- | --- |
| `check` | Runs every case against the service, with the canary listener on `127.0.0.1:47123`, and reads the `/BaseFont` names in each PDF to confirm font substitutions | 5 |
| `bench` | Runs the `overhead`, `spawn`, `formats` and `memory` suites and writes `summary.json` | 8, 9 |
| `exec` | Times an external command for each file, such as a cold `soffice` start | 10 |
| `soak` | Runs 10,000 conversions and samples memory, processes and the job directories | 11 |
| `compare` | Compares two `summary.json` files against the thresholds | 8 |

The harness will run in its own container that shares only the network namespace of the service, so that its traffic stays on loopback. The image and `run.sh` are step 6.

## Results over time

The plan keeps a summary of each public-corpus benchmark run in `bench/results/`, and the regression thresholds in `bench/thresholds.json`. The thresholds are absolute, such as 0.5 ms of growth in the p50 overhead, because the overhead can be near 1 to 2 ms, where a percentage is noise. `compare` will refuse two runs whose architecture, CPU count, memory or corpus hash differ. None of this exists yet, and no benchmark of the service has run.

Next: run `cargo test --locked` in `tools/corpus` before you change `validate`.
