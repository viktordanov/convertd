# Tests

The tests of the `convertd` crate run on the host, with fakes in place of LibreOffice and PDFium.

Build the tests with the cargo feature `test-hooks`. Without it, `pool.rs`, `process.rs` and `worker_test_role.rs` compile to empty test binaries, and nothing in them runs. CI always runs the tests with the feature.

## Contents

1. [Running the tests](#running-the-tests)
2. [Test levels](#test-levels)
3. [Fakes](#fakes)
4. [The test-hooks feature](#the-test-hooks-feature)
5. [nextest profiles](#nextest-profiles)
6. [What CI runs](#what-ci-runs)

## Running the tests

1. Install [cargo-nextest](https://nexte.st). CI uses version 0.9.152.
2. Run the tests that CI runs on every push to `main` and on every pull request:

   ```bash
   cargo nextest run --features test-hooks --profile ci
   ```

3. Run the doc tests, because nextest does not run them:

   ```bash
   cargo test --features test-hooks --doc
   ```

4. Run the contract tests. They need Go:

   ```bash
   scripts/contract_test.sh
   ```

`cargo test --features test-hooks` also runs every test, but it has no per-test time limit and it runs the `slow_` tests. Next: read [Fakes](#fakes) before you write a test that needs an engine.

<!-- memoria:section id="levels" files="convert_pdf.rs detect.rs errors.rs openapi_spec.rs upload.rs pool.rs process.rs worker_test_role.rs" -->
## Test levels

The tests have three levels:

- **Unit tests** live in `#[cfg(test)] mod tests` blocks beside the code in `src/`. Each one tests one module, such as the configuration parser or the error-to-status table in `src/rest/response.rs`.
- **Integration tests** live in this folder. Each file is a separate test crate that uses the public API of the `convertd` library. Some of them start the `convertd` binary.
- **Contract tests** live in `client/contract/`, in Go, behind the `contract` build tag. They drive the generated client and the Go facade against a running `convertd` and check each response against the spec.

The integration tests:

| File | What it tests | Needs `test-hooks` |
| --- | --- | --- |
| `convert_pdf.rs` | `/v1/convert` with `output=pdf` through the level-A fakes: detection, the response, the backstop, the permit and the response stall | One test, for the markers |
| `detect.rs` | Format detection from the bytes, with archives that the test builds | No |
| `errors.rs` | Errors over HTTP: auth, request IDs, fallbacks and the panic layer | No |
| `openapi_spec.rs` | The cheap invariants of the spec that `convertd openapi` emits | No |
| `upload.rs` | The streamed upload: digests, part rules, caps, the disk budget, and stalled and vanished clients | No |
| `pool.rs` | The worker pool with `test` workers (level B) | Yes |
| `process.rs` | Spawning, scoped reaping and process-group kills. Some cases run on Linux only | Yes |
| `worker_test_role.rs` | `convertd worker test`, driven over its pipes | Yes |

`scripts/contract_test.sh` builds `convertd` with `test-hooks`, and the contract tests start it with `TEST_ENGINE=fake`. With `--image REF`, the script runs the tests against a container of that image instead. The contract tests prove that the server, the spec and the Go client agree. They do not prove that a conversion is correct, because the fake engine converts nothing.
<!-- /memoria:section -->

<!-- memoria:section id="fakes" files="convert_pdf.rs pool.rs process.rs worker_test_role.rs" -->
## Fakes

Two fakes replace the engines at two depths. Both read the same markers.

### Level A

Level A (`TEST_ENGINE=fake`) replaces the engines inside the supervisor process. `FakeOffice` and `FakePdf` in `src/engines/fake.rs` implement the engine traits. Level A has no pool, no queue, no worker process and no kill.

In-process tests set a `FakeBehavior` on the fake directly and need no feature. `convertd serve` with `TEST_ENGINE=fake` reads markers from each upload instead. Level A tests the request path: the upload, detection, the options, the status codes and the headers.

At level A, a `429` comes from a marker or from the service's own permit. A `504` comes from the deadline check of the fake or from the backstop of the handler. A test that proves queue or kill behavior belongs at level B.

### Level B

Level B (`TEST_ENGINE=worker`) runs `convertd worker test` as a real worker process behind the real pool. The test worker answers `Convert`, `Inspect`, `Render`, `Assemble` and `Decrypt` with small valid files, driven by the markers. The queue, the deadline and the kill are real. The test worker also acts out the faults that the pool tests need.

Today `pool.rs` and `process.rs` drive the test worker directly, through the pool and the spawner. `convertd serve` with `TEST_ENGINE=worker` is not wired yet, and it answers every conversion with `503`.

### Markers

A marker is a string anywhere in the first 1 MiB of the input file, and the first marker in the file wins. Level A reads markers with `engines::fake_markers::parse`. The level-B test worker reads them with an interim copy of the same rules in `src/worker/test_worker.rs`, until it changes to the shared parser. The markers are:

- `FAKE:DELAY_MS=<n>` answers after `n` milliseconds, never past the deadline.
- `FAKE:PAGES=<n>` gives a PDF with `n` pages.
- `FAKE:HANG` never answers.
- `FAKE:PANIC` panics. Level A answers `500`, and a level-B worker exits with code 101.
- `FAKE:<CODE>` fails with that error code: `PASSWORD_REQUIRED`, `INVALID_PASSWORD`, `LIMIT_EXCEEDED`, `CONVERSION_FAILED`, `TOO_MANY_REQUESTS`, `SERVICE_UNAVAILABLE` or `INTERNAL_ERROR`.

A fixture that must get to the office queue is a DOCX with the marker in a stored, uncompressed `word/document.xml`. Compression hides the marker bytes from the parser.
<!-- /memoria:section -->

## The test-hooks feature

`test-hooks` is the one cargo feature for test code in the crate. It is off by default, and the image build never turns it on. It controls three things:

- The `TEST_ENGINE` variable. If `TEST_ENGINE` is set in a build without the feature, `convertd serve` stops at start-up with `failed to parse TEST_ENGINE: test engines need a build with cargo feature test-hooks`.
- The `convertd worker test` subcommand.
- The `FAKE:` marker parsers. Without the feature, no code reads markers.

## nextest profiles

`.config/nextest.toml` defines three profiles. Each test runs in its own process. nextest marks a test as slow after one period and kills it after four periods.

| Profile | Used by | Tests | Killed after |
| --- | --- | --- | --- |
| `default` | Local runs | All | 2 min |
| `ci` | Every push to `main` and every pull request | All except `slow_*` | 60 s |
| `full` | The manual and monthly CI run | All | 2 min |

A test that does more than about 2 s of work, or sleeps past 5 s, has a name that starts with `slow_`. A test that waits on a real deadline, such as the 5 s minimum timeout, can stay in the push run. Today all `slow_` tests are in `process.rs`.

The `ci` profile leaves out these tests with the filter `not test(/^slow_/)`. The filter matches the start of the full test name. An integration test has its function name as its name. A unit test has its module path first, such as `config::tests::`, so the filter does not leave out a unit test named `slow_…`.

In `pool.rs`, `process.rs` and `worker_test_role.rs`, every child wait, pipe read and await has a deadline. A wait that passes its deadline fails the test with a message. In the other tests, the nextest time limit bounds an await that has no deadline of its own.

## What CI runs

The `lint` and `test` jobs of `.github/workflows/image.yml` run on every push to `main` and on every pull request:

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --all-targets --features test-hooks -- -D warnings
cargo nextest run --locked --features test-hooks --profile ci
cargo test --locked --features test-hooks --doc
```

The `test-full` job runs on a manual dispatch and once a month:

```bash
cargo nextest run --locked --features test-hooks --profile full
```

CI runs the tests only in the `test-hooks` build, because that build has every test of the plain build. Clippy compiles the plain build, so CI proves that it builds, but runs no tests on it. A change to Markdown files or to `docs/` only does not start this workflow.

CI does not run the contract tests, `generate.sh --check` or the corpus checks yet.
