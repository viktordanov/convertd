# The convertd crate

`src/` is the `convertd` crate: one binary that runs as the supervisor (`convertd serve`) or as a worker (`convertd worker <role>`).

The engines are not wired into `convertd serve` yet. Without the level-A fakes, every conversion answers 503, as [Not wired yet](#not-wired-yet) explains.

## Contents

1. [Process model](#process-model)
2. [Layers](#layers)
3. [Request path](#request-path)
4. [Not wired yet](#not-wired-yet)
5. [Configuration](#configuration)

<!-- memoria:section id="overview" files="main.rs runtime.rs" -->
## Process model

The design runs one supervisor and two pools of workers, all from the same binary:

- **Supervisor**: `convertd serve`. It owns the HTTP API, the job directories, the disk budget and the pools. It never loads LibreOffice or PDFium.
- **Office worker**: `convertd worker office`. It runs LibreOffice through LibreOfficeKit.
- **Render worker**: `convertd worker render`. It runs PDFium to inspect, render, assemble and decrypt PDFs.

Each engine runs in a worker process because a hostile file can crash it, hang it or make it grow without limit. At the deadline, the pool kills the worker's process group, and the supervisor keeps serving. LibreOfficeKit allows one instance per process for the life of the process, and PDFium is not thread-safe. Thus one worker holds one engine and runs one job at a time.

The supervisor reads only enough of a file to route it. `detect/` reads the signature, the ZIP central directory, `[Content_Types].xml` and the ODF `mimetype` entry, in safe Rust with fixed caps. In the design, everything else that reads document content runs in a worker, including the pre-flight scan of office files.

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
- **`bundle/`**: the stored-ZIP writer for `images` and `bundle` output. It is empty at this time.

Lane 002 owns the worker side:

- **`engines/`**: the pools, process control and the supervisor's end of each worker connection.
- **`worker/`**: the worker processes and the engines in them.
- **`ipc/`**: the frames and messages between the supervisor and a worker.

The rest of the crate supports both sides:

- **`main.rs`**: the command line and all the wiring. Only `main.rs` picks the engines and builds the application state.
- **`config.rs`**, **`logging.rs`** and **`runtime.rs`**: the configuration, the log output and the Tokio runtime.
- **`preflight/`**: the pure scanners that the office worker will call. It is empty at this time.
- **`observability/`**: the metric names. It is empty at this time.

The dependency rule keeps the domain at the center. `domain/` uses no other module of the crate, and no HTTP, async runtime, serde, IPC or FFI crate. The test `domain_has_no_forbidden_imports` in `domain/engine.rs` fails when a domain file imports one of them.

`detect/` and `jobs/` depend on `domain/` only. `rest/` depends on `domain/`, `detect/`, `jobs/` and `config.rs`, and it imports `engines/` only in its tests. `rest/response.rs` is the only place that maps an error to an HTTP status. `rest/serialization/` holds the only serde types of the supervisor's request path.

`domain/engine.rs` is the seam between the two sides. The conversion service calls the `OfficeEngine` and `PdfEngine` traits. Lane 002's adapters in `engines/` will implement them, and they are not written yet. `worker/` is the only place with `unsafe` code and engine calls. Changes to `domain/engine.rs` go through the lead.
<!-- /memoria:section -->

<!-- memoria:section id="usage" files="rest/server.rs rest/convert_handlers.rs rest/upload.rs rest/body.rs rest/timing.rs domain/conversion_service.rs detect/mod.rs jobs/job_dir.rs jobs/disk_budget.rs" -->
## Request path

One `POST /v1/convert` with `output=pdf` goes through these files:

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
    │   └── image                 assemble, then inspect
    └── body.rs                   file_body(): the response pump
```

The upload never sits whole in memory. The handler sends the chunks of the file part through a queue of four to one blocking sink. The sink computes SHA-256 and CRC-32 while it writes, then runs detection while the file is still in the page cache.

The deadline is the end of the upload plus `timeout_seconds`. The engines enforce it inside each call, and the service makes sure that it has time left between calls. The handler drops the conversion 250 ms after the deadline and answers 504 `CONVERSION_TIMEOUT`. Dropping the future cancels the engine job.

The response pump is one blocking task. It reads the PDF in 64 KiB chunks and sends them to the response body through a queue of four. The pump, not the body, owns the job directory and the permit. If the client stops reading for `IO_IDLE_TIMEOUT_SECONDS`, the pump stops, logs `RESPONSE_STALLED` and releases both.

Every 200 carries `Content-Length`, `Content-Disposition`, `X-Request-ID`, the `Convert-*` headers and `Server-Timing`. A detection error and a conversion error also carry `Server-Timing`. `rest/timing.rs` builds the header and computes the overhead and the strict overhead for the `conversion finished` log line.
<!-- /memoria:section -->

<!-- memoria:section id="contract" files="main.rs rest/upload.rs detect/mod.rs rest/ops_handlers.rs rest/server.rs bundle/mod.rs preflight/mod.rs observability/mod.rs" -->
## Not wired yet

These parts of the design do not work yet:

- **Engines**: `convertd serve` uses the `Unwired` engines. Every conversion answers 503 `SERVICE_UNAVAILABLE`, and `/readyz` answers 503 with `starting`. In a build with the `test-hooks` feature, `TEST_ENGINE=fake` selects the level-A fakes instead. `TEST_ENGINE=worker` is not wired either, and the office and render worker roles stop with "not built yet".
- **ZIP output**: `output=images` and `output=bundle` answer 422 `VALIDATION_ERROR` with "output images is not available yet" or "output bundle is not available yet".
- **OLE2 files**: `doc`, `xls`, `ppt`, `pps` and encrypted OOXML answer 415 with "unsupported format: ole2". `/v1/capabilities` already lists `doc`, `xls`, `ppt` and `pps`.
- **Warnings**: the pre-flight scanners and the warning rules that compare their facts with the PDF are not written yet. `RENDER_CLAMPED` needs image output, so the service raises no warning. `Convert-Warning-Count` is always 0.
- **Metrics and drain**: `/metrics` answers an empty body. On SIGTERM or SIGINT, the server stops accepting and finishes its open connections, with no drain step before that.
<!-- /memoria:section -->

<!-- memoria:section id="setup" files="config.rs" -->
## Configuration

`config.rs` parses every environment variable by hand when `convertd serve` starts. A bad value stops `convertd serve` before it listens, and the error names the variable. Lane 002's engine and pool variables are parsed in [engines/config.rs](engines/config.rs). The two parsers hold the names, the defaults and the checks. `convertd openapi` and `convertd worker` do not parse this configuration.
<!-- /memoria:section -->
