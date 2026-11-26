# corpus-bench

`corpus-bench` is the Rust harness that checks the corpus in [`corpus/`](../../corpus/README.md) and runs it against the service. It does not measure the service: the benchmark of record uses `examples/overhead.rs`.

The crate is not part of the `convertd` workspace. It has its own `[workspace]` table and its own `Cargo.lock`, so the root `cargo build` never builds it, and its dependencies stay out of the service's lock file. Run Cargo here, or give `--manifest-path tools/corpus/Cargo.toml`.

## Contents

1. [Why a separate crate](#why-a-separate-crate)
2. [Build and test](#build-and-test)
3. [validate](#validate)
4. [check](#check)
5. [listen](#listen)
6. [Later subcommands](#later-subcommands)
7. [Results over time](#results-over-time)

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

The canary listener binds the `listener` address of `expected.json`, `127.0.0.1:47123`. If the bind fails, `check` stops: without the listener, zero connections proves nothing. Before the first case, `check` connects to the listener itself, and it stops unless the listener counts exactly that one connection. A connection during a case, or within 200 ms after it, fails that case. After each case, `/healthz` must answer 200.

Each answer must meet these invariants:

- An error answer has a JSON body with a `code`.
- The `Content-Type` matches `output`, and the PDF starts with `%PDF-` and holds `%%EOF`.
- For `images` and `bundle`, the body is a tar stream in one zstd frame (`application/zstd`). Its entries are `manifest.json`, then `document.pdf` for a bundle, then the page images in the order of the manifest.
- In such an answer, `Convert-Page-Count` and `Convert-Warning-Count` agree with `manifest.json`, and `pages` lists the requested pages.
- No page image has an edge longer than `max_edge_px` from `/v1/capabilities`.

Then `check` compares the fields that the expectation gives. For each `FONT_SUBSTITUTED` warning, it also reads the `/BaseFont` names of the PDF: the requested family must be absent, and the claimed substitute must be present. A disagreement points to the `font_substitutes` entry in `build-info.json`.

`check` prints `PASS` or `FAIL` for each case, with each failure and the observed value. It prints the observed page count of an `"unrecorded"` case as a note. Then it prints the passed and run cases for each tag. It exits 0 only if every selected case passed.

The canary listener must share the network namespace of the service, because LibreOffice connects to the loopback address of its own container. The run of record used a harness container started with `--network container:<service>`. A harness on the host behind `-p` does not see such a connection.

On 2026-11-26, `check` ran that way against the image `rs-convert-lead-runtime:wave2a`, with LibreOffice 26.8.3.1 and PDFium 8179, in arm64 Docker on an Apple-silicon Mac. 47 of the 55 cases of that time passed. The listener saw no connection, and the `/BaseFont` arbiter agreed with each `FONT_SUBSTITUTED` warning. No positive control has run yet, so zero connections does not yet prove that the canaries can connect. The lead then ruled on the eight failures: five expectations changed to what LibreOffice does, and the three image cases now expect the 415 that lane 001 gives for image inputs, which are out of v1. `expected.json` also gained `docx-one-page` with two cases, so it now holds 57 cases. Its 1-page count comes from the same image, by a direct request.

On 2026-11-26, `check` ran the same way against `rs-convert-lead-runtime:wave2b`, built from `main` at `1eafd5ee`, and all 57 cases passed, with exit status 0. The listener saw no connection, and the service log had no error and no worker restart. The full run took about 31 s of request time. The slowest cases were `xlsx-100k` at about 7 to 8 s each and `pptx-photos60` at about 4 to 5 s each. Still no positive control had run, so the canaries proved only that no connection was made.

Later on 2026-11-26, with two data-descriptor cases added, `check` ran again against `rs-convert-lead-runtime:wave2b`, and all 59 cases passed. The listener control passed inside the namespace of the service. `docx-stored-descriptor` gave `422 CONVERSION_FAILED` and `docx-deflated-descriptor` gave 200 with 2 pages, as expected.
<!-- /memoria:section -->

<!-- memoria:section id="listen" files="src/main.rs src/check/listener.rs" -->
## listen

`listen` binds the canary listener address alone, by default `127.0.0.1:47123`. It prints each connection with its first request line, and it exits 0 when it has counted `--expect` connections (default 1), or 1 after `--timeout` seconds (default 60).

It is the listener for two controls that `check` cannot run by itself. In both, the harness container shares the network namespace of the service:

- **Shared namespace:** a process in the service container connects, for example `docker exec <service> env PORT=47123 /usr/local/bin/convertd healthcheck`. One connection proves that the harness sees the loopback of the service.
- **Canary positive control (plan step 13):** plain `soffice` runs the three canaries with a permissive profile, and each must connect. Until this control passes, zero connections in `check` proves that the listener works, not that the canaries can connect.

On 2026-11-26 the shared-namespace control passed. `listen` counted one connection (`GET /readyz`) from `convertd healthcheck` run inside the `rs-convert-lead-runtime:wave2b` container with `PORT=47123`.

The canary positive control failed in every condition tried. No canary connected:

- LibreOffice 26.8.3.1 in that image, `soffice --convert-to pdf` with a permissive profile (macro security 0, untrusted links allowed, Calc recalculation on load and link updates on), on the three generated canaries.
- LibreOffice 26.8.3.1 on macOS, the same profile and command.
- LibreOffice 25.2.3 from Debian trixie in a throwaway image, on the three canaries and on three references that LibreOffice wrote itself through pyuno. It tried `--convert-to` with the permissive profile, and a load with `MacroExecutionMode` 4 (always run) through UNO.

In the throwaway image even a direct call of the Basic macro through the script provider did not run it: a line that writes a marker file left no file. The ODT and XLSX references carry the same event binding and the same `_xlfn.WEBSERVICE` formula as the generated canaries. The formula evaluated to `#VALUE!` with no connection. LibreOffice's own DOCX export writes the external image relationship but no `r:link`, so the generated canary keeps the form that Word writes. The cause of the failure is not known.
<!-- /memoria:section -->

## Later subcommands

`validate`, `check` and `listen` exist at this time. The 005 plan listed four more subcommands. None of them is planned now, because other tools do the work:

| Subcommand | What the plan wanted | Status |
| --- | --- | --- |
| `bench` | The `overhead`, `spawn`, `formats` and `memory` suites, with `summary.json` | Not planned. `examples/overhead.rs` made the benchmark of record |
| `exec` | Times an external command for each file, such as a cold `soffice` start | Not planned |
| `soak` | 10,000 conversions with samples of memory, processes and job directories | Not planned. Lane 002 runs the soak |
| `compare` | Compares two `summary.json` files against absolute thresholds | Not planned while no `summary.json` exists |

In CI, `docker/corpus-check.sh` runs `check` against the image, with the harness in a container that shares the network namespace of the service.

## Results over time

The benchmark of record is [`bench/results/2026-11-26-wave2b.md`](../../bench/results/2026-11-26-wave2b.md). `examples/overhead.rs` made it against `rs-convert-lead-runtime:wave2b`, and it gives `overhead` 0.883 ms at p50 for the one-page DOCX, against the 5 ms gate. That file records its conditions. A later run is compared with it by hand, because no `compare` subcommand and no `bench/thresholds.json` exist.

Next: run `cargo test --locked` in `tools/corpus` before you change `validate` or `check`.
