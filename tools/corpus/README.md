# corpus-bench

`corpus-bench` is the Rust harness that checks the corpus in [`corpus/`](../../corpus/README.md) and runs it against the service. Later steps of item 005 add the measurements.

The crate is not part of the `convertd` workspace. It has its own `[workspace]` table and its own `Cargo.lock`, so the root `cargo build` never builds it, and its dependencies stay out of the service's lock file. Run Cargo here, or give `--manifest-path tools/corpus/Cargo.toml`.

## Contents

1. [Why a separate crate](#why-a-separate-crate)
2. [Build and test](#build-and-test)
3. [validate](#validate)
4. [check](#check)
5. [Later subcommands](#later-subcommands)
6. [Results over time](#results-over-time)

<!-- memoria:section id="overview" files="Cargo.toml clippy.toml" -->
## Why a separate crate

The harness is an independent client of the service. `check` reads the API with its own lenient types, not with the server's types. Thus a server change cannot also change what the harness expects. Its dependencies, such as the HTTP client `reqwest`, the PDF reader `lopdf` and the bundle readers `tar` and `zstd`, stay out of the lock file of `convertd`.

`clippy.toml` in this folder replaces the root one. Clippy reads the nearest `clippy.toml` up the tree, and the root file forbids blocking waits on child processes for the reaper of `convertd`. This harness has no reaper, and `validate` waits on `git`.
<!-- /memoria:section -->

<!-- memoria:section id="setup" files="Cargo.toml src/main.rs" -->
## Build and test

The crate uses the Rust toolchain that `rust-toolchain.toml` pins at the repository root.

1. Go to `tools/corpus`.
2. Run `cargo fmt --check`.
3. Run `cargo clippy --locked --all-targets -- -D warnings`.
4. Run `cargo test --locked`.

The unit tests of `validate` build small corpus folders in a temporary directory. They prove that `validate` fails on a missing file, a wrong SHA-256, an unlisted file under `out/` or `fixtures/`, fixtures over the budget, and a sample of the schema and semantic rules. One test applies the schema and the semantic checks to the committed `corpus/expected.json`, without the generated files.

The unit tests of `check` compare hand-built answers with expectations, because the level-A fake cannot produce warnings, blank pages or real page counts. They cover the warning multiset, `"*"` and `unless_installed`, blank pages, the three page-count forms, status lists, the invariants, the `/BaseFont` arbiter, the listener and the exit status.
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
4. No file under `out/` or `fixtures/` is missing from `expected.json`, except `fixtures/SOURCES.md`. `validate` ignores `.DS_Store` files. All files under `fixtures/` together hold at most 2 MiB.
5. Git tracks nothing under `out/` or `private/`. Outside a git work tree, `validate` skips this check with a note.

`validate` prints each problem on its own line and exits 1 if it finds a problem. It prints notes, such as the cases whose page count is still `"unrecorded"`, without a failure. On a pass it prints the number of files and cases and exits 0.

A pass proves that the expectations are well formed and that the files on disk are the files that `expected.json` describes. It does not prove that the service gives those answers. That is the job of `check`.
<!-- /memoria:section -->

<!-- memoria:section id="check" files="src/main.rs src/check/mod.rs src/check/evaluate.rs src/check/listener.rs src/check/fonts.rs" -->
## check

`check` sends every case of `expected.json` to a running service and compares each answer with its expectation. Generate the corpus with `--hostile` and start the service first. Then run it from the repository root:

```sh
CONVERT_API_KEY=<key> cargo run --manifest-path tools/corpus/Cargo.toml -- check --target http://127.0.0.1:8080
```

`--target` (or `CONVERT_HOST`) gives the base URL, by default `http://127.0.0.1:8080`. `CONVERT_API_KEY` (or `--api-key`) gives the bearer key. `--tags` and `--ids` take comma-separated lists and select the files to run. `--report FILE` writes every result as JSON. The corpus flags are the same as for `validate`.

The harness first applies the schema and semantic checks of `validate`, then asks `/v1/capabilities` for the installed fonts and the limits. It sends one case at a time. Each case gets the `timeout_seconds` of its options, or the service default, plus 15 s.

The canary listener binds the `listener` address of `expected.json`, `127.0.0.1:47123`. If the bind fails, `check` stops: without the listener, zero connections proves nothing. A connection during a case, or within 200 ms after it, fails that case. After each case, `/healthz` must answer 200.

Each answer must meet these invariants:

- An error answer has a JSON body with a `code`.
- The `Content-Type` matches `output`, and the PDF starts with `%PDF-` and holds `%%EOF`.
- For `images` and `bundle`, the body is a tar stream in one zstd frame (`application/zstd`). Its entries are `manifest.json`, then `document.pdf` for a bundle, then the page images in the order of the manifest.
- In such an answer, `Convert-Page-Count` and `Convert-Warning-Count` agree with `manifest.json`, and `pages` lists the requested pages.
- No page image has an edge longer than `max_edge_px` from `/v1/capabilities`.

Then `check` compares the fields that the expectation gives. For each `FONT_SUBSTITUTED` warning, it also reads the `/BaseFont` names of the PDF: the requested family must be absent, and the claimed substitute must be present. A disagreement points to the `font_substitutes` entry in `build-info.json`.

`check` prints `PASS` or `FAIL` for each case, with each failure and the observed value. It prints the observed page count of an `"unrecorded"` case as a note. Then it prints the passed and run cases for each tag. It exits 0 only if every selected case passed.

The canary listener must share the network namespace of the service, because LibreOffice connects to the loopback address of its own container. The run of record used a harness container started with `--network container:<service>`. A harness on the host behind `-p` does not see such a connection.

On 2026-11-26, `check` ran that way against the image `rs-convert-lead-runtime:wave2a`, with real engines, and 47 of 55 cases passed. The listener saw no connection, and the `/BaseFont` arbiter agreed with each `FONT_SUBSTITUTED` warning. No positive control has run yet, so zero connections does not yet prove that the canaries can connect. The eight failures are open questions about the expectations and the warning rules.
<!-- /memoria:section -->

## Later subcommands

`validate` and `check` exist at this time. The 005 plan adds these subcommands in later steps:

| Subcommand | What it will do | Plan step |
| --- | --- | --- |
| `bench` | Runs the `overhead`, `spawn`, `formats` and `memory` suites and writes `summary.json` | 8, 9 |
| `exec` | Times an external command for each file, such as a cold `soffice` start | 10 |
| `soak` | Runs 10,000 conversions and samples memory, processes and the job directories | 11 |
| `compare` | Compares two `summary.json` files against the thresholds | 8 |

The harness will run in its own container that shares only the network namespace of the service, so that its traffic stays on loopback. The image and `run.sh` are step 6.

## Results over time

The plan keeps a summary of each public-corpus benchmark run in `bench/results/`, and the regression thresholds in `bench/thresholds.json`. The thresholds are absolute, such as 0.5 ms of growth in the p50 overhead, because the overhead can be near 1 to 2 ms, where a percentage is noise. `compare` will refuse two runs whose architecture, CPU count, memory or corpus hash differ. None of this exists yet, and no benchmark of the service has run.

Next: run `cargo test --locked` in `tools/corpus` before you change `validate` or `check`.
