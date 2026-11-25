# The convertd crate

`src/` is the `convertd` crate: one binary that runs as the supervisor (`convertd serve`) or as a worker (`convertd worker <role>`).

The engines are not wired into `convertd serve` yet. Without the level-A fakes, every conversion answers 503, as [Not wired yet](#not-wired-yet) explains.

## Contents

1. [Process model](#process-model)
2. [Layers](#layers)
3. [Request path](#request-path)
4. [Warnings](#warnings)
5. [Lifecycle and metrics](#lifecycle-and-metrics)
6. [Not wired yet](#not-wired-yet)
7. [Configuration](#configuration)

<!-- memoria:section id="overview" files="main.rs runtime.rs" -->
## Process model

The design runs one supervisor and two pools of workers, all from the same binary:

- **Supervisor**: `convertd serve`. It owns the HTTP API, the job directories, the disk budget and the pools. It never loads LibreOffice or PDFium.
- **Office worker**: `convertd worker office`. It runs LibreOffice through LibreOfficeKit.
- **Render worker**: `convertd worker render`. It runs PDFium to inspect, render, assemble and decrypt PDFs.

Each engine runs in a worker process because a hostile file can crash it, hang it or make it grow without limit. At the deadline, the pool kills the worker's process group, and the supervisor keeps serving. LibreOfficeKit allows one instance per process for the life of the process, and PDFium is not thread-safe. Thus one worker holds one engine and runs one job at a time.

The supervisor reads only enough of a file to route it. `detect/` reads the signature, the ZIP central directory, `[Content_Types].xml`, the ODF `mimetype` entry and the stream names of an OLE2 file, in safe Rust with fixed caps. In the design, everything else that reads document content runs in a worker, including the pre-flight scan of office files.

`main.rs` dispatches `convertd worker` first, before it builds a runtime, starts logging or reads the configuration. For `convertd serve`, `runtime.rs` builds a Tokio runtime with 2 worker threads, unless `TOKIO_WORKER_THREADS` sets another count. The blocking pool has at most 32 threads, and an idle blocking thread exits after 10 s.

Lane 002 documents the worker side:

- [engines/README.md](engines/README.md): the pools, from the supervisor's side.
- [worker/README.md](worker/README.md): a worker process, from the inside.
- [ipc/README.md](ipc/README.md): the protocol between the supervisor and a worker.
<!-- /memoria:section -->

<!-- memoria:section id="layers" files="lib.rs domain/mod.rs domain/engine.rs rest/response.rs rest/serialization/mod.rs" -->
## Layers

The supervisor's request path uses five layers:

- **`domain/`**: the formats, the validated request, the page selection, the limits, the warnings, the engine traits and the conversion service.
- **`rest/`**: the axum router, the middleware, the handlers, the streamed upload, the response pump, the error mapping and the DTOs.
- **`detect/`**: format detection from the bytes, with the zip-bomb guard.
- **`jobs/`**: the job directory under `TMP_DIR` and the disk budget (`TMP_MAX_BYTES`).
- **`bundle/`**: the stored-ZIP writer for `images` and `bundle` output.

Lane 002 owns the worker side:

- **`engines/`**: the pools, process control and the supervisor's end of each worker connection.
- **`worker/`**: the worker processes and the engines in them.
- **`ipc/`**: the frames and messages between the supervisor and a worker.

The rest of the crate supports both sides:

- **`main.rs`**: the command line and all the wiring. Only `main.rs` picks the engines and builds the application state.
- **`config.rs`**, **`logging.rs`** and **`runtime.rs`**: the configuration, the log output and the Tokio runtime.
- **`preflight/`**: the pure scanners that the office worker calls before LibreOffice loads a file.
- **`observability/`**: the metric names and the Prometheus recorder.

The dependency rule keeps the domain at the center. `domain/` uses no other module of the crate, and no HTTP, async runtime, serde, IPC or FFI crate. The test `domain_has_no_forbidden_imports` in `domain/engine.rs` fails when a domain file imports one of them.

`detect/` and `jobs/` depend on `domain/` only. `rest/` depends on `domain/`, `detect/`, `jobs/` and `config.rs`, and it imports `engines/` only in its tests. `rest/response.rs` is the only place that maps an error to an HTTP status. `rest/serialization/` holds the only serde types of the supervisor's request path.

`domain/engine.rs` is the seam between the two sides. The conversion service calls the `OfficeEngine` and `PdfEngine` traits. Lane 002's adapters in `engines/` will implement them, and they are not written yet. `worker/` is the only place with `unsafe` code and engine calls. Changes to `domain/engine.rs` go through the lead.
<!-- /memoria:section -->

<!-- memoria:section id="usage" files="detect/ole2.rs rest/server.rs rest/convert_handlers.rs rest/upload.rs rest/body.rs rest/timing.rs domain/conversion_service.rs detect/mod.rs jobs/job_dir.rs jobs/disk_budget.rs bundle/zip_stored.rs rest/serialization/manifest.rs" -->
## Request path

One `POST /v1/convert` goes through these files:

```text
rest/server.rs                    layers, outermost first
├── middleware/request_id.rs      accept or mint X-Request-ID
├── middleware/access_log.rs      one "http request" line
├── CatchPanicLayer               a panic becomes a 500
├── RequestBodyTimeoutLayer       upload idle timeout, /v1 only
├── middleware/auth.rs            bearer key, /v1 only
└── convert_handlers.rs           convert()
    ├── inflight permit           429 before the body is read
    ├── jobs/job_dir.rs           TMP_DIR/job-<32 hex>, mode 0700
    ├── upload.rs                 receive()
    │   ├── Content-Length cap    413 before the body is read
    │   ├── jobs/disk_budget.rs   reserve the upload, else 503
    │   ├── options part          validated when it arrives
    │   └── blocking sink         hash and write the file part
    │       └── detect/mod.rs     detect_file(), then source.<ext>
    ├── conversion_service.rs     convert() under the backstop
    │   ├── office file           convert, then inspect
    │   ├── PDF                   inspect, then decrypt if needed
    │   ├── image                 assemble, then inspect
    │   └── images or bundle      render the selected pages
    ├── bundle/zip_stored.rs      ZipPlan: manifest, PDF, pages
    └── body.rs                   file_body() or zip_body()
```

An encrypted OOXML package is an OLE2 file whose type is inside the encryption. Without a password it answers 422 `PASSWORD_REQUIRED` before any engine call. With one, detection gives a provisional `docx`, and the document type that the office engine reports decides the format in `Convert-Source-Format` and the manifest. Whether LibreOfficeKit opens an encrypted spreadsheet or presentation named `source.docx` is not verified.

The upload never sits whole in memory. The handler sends the chunks of the file part through a queue of four to one blocking sink. The sink computes SHA-256 and CRC-32 while it writes, then runs detection while the file is still in the page cache.

The deadline is the end of the upload plus `timeout_seconds`. The engines enforce it inside each call, and the service makes sure that it has time left between calls. The handler drops the conversion 250 ms after the deadline and answers 504 `CONVERSION_TIMEOUT`. Dropping the future cancels the engine job.

For `images` and `bundle`, the response is a stored ZIP: `manifest.json`, then `document.pdf` for a bundle, then `pages/NNNN.<ext>`. Its sizes come from `fstat` and every CRC-32 is known before the headers, so `Content-Length` is exact. A ZIP that would need ZIP64 (4 GiB, or 65 535 entries) answers 422 `LIMIT_EXCEEDED`.

The response pump is one blocking task. It reads the PDF, or writes the ZIP, in 64 KiB chunks and sends them to the response body through a queue of four. The pump, not the body, owns the job directory and the permit. If the client stops reading for `IO_IDLE_TIMEOUT_SECONDS`, the pump stops, logs `RESPONSE_STALLED` and releases both.

Every 200 carries `Content-Length`, `Content-Disposition`, `X-Request-ID`, the `Convert-*` headers and `Server-Timing`. A detection error and a conversion error also carry `Server-Timing`. `rest/timing.rs` builds the header and computes the overhead and the strict overhead for the `conversion finished` log line.
<!-- /memoria:section -->

<!-- memoria:section id="warnings" files="domain/verification.rs preflight/mod.rs preflight/package.rs preflight/docx.rs preflight/pptx.rs preflight/xlsx.rs preflight/odf.rs" -->
## Warnings

Warnings compare what an office source declares with what the engines produced. The algorithms are 005 §5. `preflight::scan` reads the source in the office worker, before LibreOffice loads it, and returns `SourceFacts`: the visible slide count of a deck, the requested Latin font families and where macros are stored. `domain/verification.rs` holds the rules:

- **`PAGE_COUNT_MISMATCH`**: PPTX family only, when the declared count differs from the PDF. It is skipped when hidden slides or notes pages are included, and for a template with no slides.
- **`FONT_SUBSTITUTED`**: a requested family that `build-info.json` does not list, unless `fonts.json` marks it metric-compatible and its substitute is installed. At most 32 per request. Without `build-info.json`, as outside the image, the check is skipped.
- **`BLANK_PAGE`**: a page with content that rendered as one flat colour, from the rendered page or the probe render.
- **`MACROS_IGNORED`**: a VBA project, an Excel 4 macro sheet, an ODF `Basic/` module or `Scripts/` entry, or an OLE2 macro storage.
- **`RENDER_CLAMPED`**: `MAX_DPI` or `MAX_EDGE_PX` lowered the requested resolution.

The scanners stream each part through quick-xml under fixed caps: 4 MiB per part, 32 MiB per font scan and 64 MiB per request. A part that is missing, malformed or over a cap skips its check, and the skip is logged. A scanner must not panic, because a panic ends the office worker.
<!-- /memoria:section -->

<!-- memoria:section id="lifecycle" files="main.rs rest/server.rs rest/ops_handlers.rs observability/metrics.rs" -->
## Lifecycle and metrics

`convertd serve` creates `TMP_DIR` and removes the `job-*` directories that a crash left. On SIGTERM or SIGINT it stops in four steps:

1. It sets the draining flag: `/readyz` answers 503 with `draining`, and new conversions answer 503 `SERVICE_UNAVAILABLE` with `Retry-After: 5`.
2. It calls the engines' `drain`, and waits until no request holds a permit, at most `SHUTDOWN_GRACE_SECONDS`. The listener stays open during this time.
3. It stops axum, which stops accepting and gives open connections 2 s more.
4. It calls the engines' `shutdown` with `SHUTDOWN_GRACE_SECONDS`, then exits 0.

`/metrics` renders one process-wide Prometheus recorder in the text format. 001 records `convert_requests_total`, `convert_stage_duration_seconds`, `convert_input_bytes`, `convert_warnings_total`, `convert_verify_skipped_total`, `convert_pages_rendered_total`, `convert_responses_cut_total` and `convert_disk_budget_bytes`. On Linux it adds the supervisor's RSS, peak RSS, open descriptors and threads from `/proc/self` at each scrape. A 200 whose body stalls or aborts counts once in `convert_requests_total` with code `OK` and once in `convert_responses_cut_total`. `METRICS_ENABLED=false` makes `/metrics` answer 404.
<!-- /memoria:section -->

<!-- memoria:section id="contract" files="main.rs" -->
## Not wired yet

This part of the design does not work yet:

- **Engines**: `convertd serve` uses the `Unwired` engines. Every conversion answers 503 `SERVICE_UNAVAILABLE`, and `/readyz` answers 503 with `starting`. In a build with the `test-hooks` feature, `TEST_ENGINE=fake` selects the level-A fakes instead. `TEST_ENGINE=worker` is not wired either, and the office and render worker roles stop with "not built yet".
<!-- /memoria:section -->

<!-- memoria:section id="setup" files="config.rs" -->
## Configuration

`config.rs` parses every environment variable by hand when `convertd serve` starts. A bad value stops `convertd serve` before it listens, and the error names the variable. Lane 002's engine and pool variables are parsed in [engines/config.rs](engines/config.rs). The two parsers hold the names, the defaults and the checks. `convertd openapi` and `convertd worker` do not parse this configuration.
<!-- /memoria:section -->
