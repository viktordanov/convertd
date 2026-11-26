# The convertd crate

`src/` is the `convertd` crate: one binary that runs as the supervisor (`convertd serve`) or as a worker (`convertd worker <role>`).

`convertd serve` runs LibreOffice and PDFium in worker pools, unless `TEST_ENGINE` selects a test fake. [Engine selection](#engine-selection) gives the details.

## Contents

1. [Process model](#process-model)
2. [Layers](#layers)
3. [Request path](#request-path)
4. [Warnings](#warnings)
5. [Lifecycle and metrics](#lifecycle-and-metrics)
6. [Engine selection](#engine-selection)
7. [Measurement](#measurement)
8. [Configuration](#configuration)

<!-- memoria:section id="overview" files="main.rs runtime.rs" -->
## Process model

The service runs one supervisor and two pools of workers, all from the same binary:

- **Supervisor**: `convertd serve`. It owns the HTTP API, the job directories, the disk budget and the pools. It never loads LibreOffice or PDFium.
- **Office worker**: `convertd worker office`. It runs LibreOffice through LibreOfficeKit.
- **Render worker**: `convertd worker render`. It runs PDFium to inspect, render and decrypt PDFs.

Each engine runs in a worker process because a hostile file can crash it, hang it or make it grow without limit. At the deadline, the pool kills the worker's process group, and the supervisor keeps serving. LibreOfficeKit allows one instance per process for the life of the process, and PDFium is not thread-safe. Thus one worker holds one engine and runs one job at a time.

The supervisor reads only enough of a file to route it. `detect/` reads the signature, the ZIP central directory, `[Content_Types].xml` and the ODF `mimetype` entry. For an OLE2 file it reads only the names in the root storage, with its own reader that never builds the whole FAT. All of it is safe Rust with fixed caps. Everything else that reads document content runs in a worker, including the pre-flight scan of office files.

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
- **`bundle/`**: the tar.zst writer for `images` and `bundle` output.

Lane 002 owns the worker side:

- **`engines/`**: the pools, process control, the engine adapters, the engine selection and the supervisor's end of each worker connection.
- **`worker/`**: the worker processes and the engines in them.
- **`ipc/`**: the frames and messages between the supervisor and a worker.

The rest of the crate supports both sides:

- **`main.rs`**: the command line and all the wiring. It starts the engines through `engines/select.rs` and builds the application state.
- **`config.rs`**, **`logging.rs`** and **`runtime.rs`**: the configuration, the log output and the Tokio runtime.
- **`preflight/`**: the pure scanners that the office worker calls before LibreOffice loads a file.
- **`observability/`**: the metric names and the Prometheus recorder.

The dependency rule keeps the domain at the center. `domain/` uses no other module of the crate, and no HTTP, async runtime, serde, IPC or FFI crate. The test `domain_has_no_forbidden_imports` in `domain/engine.rs` fails when a domain file imports one of them.

`detect/` and `jobs/` depend on `domain/` only. `preflight/` depends on `domain/`, on the ZIP reader in `detect/zip_scan.rs` and on the OLE2 reader in `detect/ole2.rs`. `rest/` depends on `domain/`, `detect/`, `jobs/`, `bundle/`, `observability/` and `config.rs`, and it imports `engines/` only in its tests. `rest/response.rs` is the only place that maps an error to an HTTP status. `rest/serialization/` holds the only serde types of the supervisor's request path.

`domain/engine.rs` is the seam between the two sides. The conversion service calls the `OfficeEngine` and `PdfEngine` traits. Lane 002's adapters `OfficeLok` and `PdfPdfium` in `engines/` implement them over the pools. `worker/` is the only place with engine calls, and it holds the `unsafe` code, with one exception. The reaper in `engines/process.rs` peeks at an exited child with `libc::waitid(P_ALL, WNOWAIT)` in two `unsafe` blocks. rustix 1.1.6 has `waitid`, but its result exposes no `si_pid`, so it cannot name the child (checked on 2026-11-26). Changes to `domain/engine.rs` go through the lead.
<!-- /memoria:section -->

<!-- memoria:section id="usage" files="detect/ole2.rs rest/server.rs rest/convert_handlers.rs rest/upload.rs rest/body.rs rest/timing.rs rest/throughput.rs rest/framing.rs rest/middleware/linger.rs domain/conversion_service.rs detect/mod.rs jobs/job_dir.rs jobs/disk_budget.rs bundle/tar_zst.rs rest/serialization/manifest.rs" -->
## Request path

One `POST /v1/convert` goes through these files:

```text
rest/server.rs                    layers, outermost first
├── middleware/linger.rs          drain an unread body after an early answer
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
    │   └── images or bundle      render the selected pages
    ├── bundle/tar_zst.rs         TarPlan: manifest, PDF, pages
    └── body.rs                   file_body() or tar_body()
```

An encrypted OOXML package is an OLE2 file whose type is inside the encryption. Without a password it answers 422 `PASSWORD_REQUIRED` before any engine call. With one, detection gives a provisional `docx`, and the document type that the office engine reports decides the format in `Convert-Source-Format` and the manifest. Whether LibreOfficeKit opens an encrypted spreadsheet or presentation named `source.docx` is not verified.

An encrypted PDF that needs a password to open answers 422 `PASSWORD_REQUIRED` without one and `INVALID_PASSWORD` with a wrong one. With the right one, the render worker writes a decrypted copy that the rest of the request reads. A PDF with only an owner password opens without one, so with no password it passes through unchanged. If a password is given for it, the worker checks it as the owner password: right, the PDF is decrypted; wrong, 422 `INVALID_PASSWORD`. Every decrypt keeps owner restrictions that the password did not unlock. When only the user password of a restricted PDF is given, the worker writes no copy. The original encrypted PDF then goes on: it is the `pdf` output, and the render opens it with the password.

The upload never sits whole in memory. multer buffers the bytes before the first boundary and each part's headers until it finds their end, so `rest/framing.rs` bounds both at 16 KiB and answers 400 past that. The handler sends the chunks of the file part through a queue of four to one blocking sink. The sink computes SHA-256 while it writes, then runs detection while the file is still in the page cache.

Some answers come while the client is still uploading: 401, 413, 415, 429 and 503. If the server then closed the socket with unread bytes, the kernel would send a reset. A client such as Go's `http.Transport` would then often report a broken pipe instead of the answer. So `middleware/linger.rs` wraps every request body. When a body is dropped before its end, a task of its own reads and discards the rest. The task stops at the end of the body, after 256 MiB, after 10 s, or after 2 s without data. At most 32 bodies drain at once, and the task holds no permit and no job directory.

The deadline is the end of the upload plus `timeout_seconds`. The engines enforce it inside each call, and the service makes sure that it has time left between calls. The handler drops the conversion 250 ms after the deadline and answers 504 `CONVERSION_TIMEOUT`. Dropping the future cancels the engine job.

For `images` and `bundle`, the response is a ustar archive in one zstd frame at level 1: `application/zstd`, named `<stem>.tar.zst`. Its entries come in this order: `manifest.json`, then `document.pdf` for a bundle, then `pages/NNNN.<ext>` in page order. Each entry is a regular file with mode 0644, mtime 0 and uid and gid 0, so the bytes depend only on the content. The sizes in the headers come from `fstat`. The compressed length is not known in advance, so the response is chunked, without `Content-Length`. The disk budget and `max_output_bytes` bound its size.

Each engine call that writes reserves its cap of the disk budget first. The render reserves what its selected pages can write, from their size in the inspection. Per page, that is the raw 4-byte-per-pixel bitmap at the request's DPI and edge cap, plus an eighth and 64 KiB. Each render adds 1 MiB. It is at most `max_output_bytes`, 768 MiB by default. The engine gets this estimate as its hard limit, so a render that writes more fails with 422 `LIMIT_EXCEEDED`. PNG and JPEG stay under 4 bytes per pixel in practice, but no real-engine run has measured the margin yet. After the last engine call, the handler measures the job directory with `fstat` and gives back the rest of the reservation. The tar headers take their sizes from this one walk, so each file gets one `fstat`. So a download that is still open holds only its files, and slow clients do not spend the budget of the next requests.

The response pump is one blocking task. It reads the PDF, or writes the tar.zst, in 64 KiB chunks and sends them to the response body through a queue of four. It reuses its chunk buffers: a buffer returns to the pump when hyper drops its chunk. The pump, not the body, owns the job directory and the permit. The body ends cleanly only when the pump sends its completion marker. A pump that stops early for any reason, a panic included, leaves the body with an error. Then hyper drops the connection instead of sending the chunked terminator. So a cut tar.zst never looks complete, and the access log records `complete=false`. If the client stops reading for `IO_IDLE_TIMEOUT_SECONDS`, the pump stops, logs `RESPONSE_STALLED` and releases both.

The idle timer alone lets a trickle hold a permit: one byte just inside each idle period resets it. So both directions also have a floor, measured over each window of `IO_IDLE_TIMEOUT_SECONDS`. An upload below `UPLOAD_MIN_BYTES_PER_SECOND` answers 400 "upload too slow". A response read below `RESPONSE_MIN_BYTES_PER_SECOND` is cut and logged as `RESPONSE_TOO_SLOW`. Both default to 16 KiB/s, and 0 turns a floor off. The deadline covers the time up to the response headers, so the handler checks it again before it plans a bundle, and once more after planning, before the 200.

Every 200 carries `Content-Disposition`, `X-Request-ID`, the `Convert-*` headers and `Server-Timing`, and a PDF also `Content-Length`. `Convert-Engine-Fingerprint` hashes everything that decides the output bytes for an input. That is the service version and commit, the engine versions and the font set. It also covers the settings `MAX_DPI`, `MAX_EDGE_PX`, `DEFAULT_PAPER`, `VERIFY_PROBE_RENDER` and `VERIFY_PROBE_MAX_PAGES`. Every error after detection also carries `Server-Timing`. An engine error carries no timing, so the service charges a call that failed in the queue (`QueueTimeout` or `QueueFull`) to `queue`: a 429 shows the wait. `rest/timing.rs` builds the header and computes the overhead and the strict overhead for the `conversion finished` log line. The `preflight` stage is the office worker's scan, which the worker times inside `convert`. Only office inputs carry it. It is not engine time, so it counts as overhead. The `conversion finished` line logs it as `preflight_us`, and `convert_stage_duration_seconds` records it as the stage `preflight`.
<!-- /memoria:section -->

<!-- memoria:section id="warnings" files="domain/verification.rs preflight/mod.rs preflight/package.rs preflight/docx.rs preflight/pptx.rs preflight/xlsx.rs preflight/odf.rs" -->
## Warnings

Warnings compare what an office source declares with what the engines produced. The algorithms are 005 §5. `preflight::scan` reads the source in the office worker, before LibreOffice loads it. It returns `SourceFacts`: the visible slide count of a deck, the requested Latin font families (at most 256, each at most 256 bytes), the requested families that a DOCX embeds and where macros are stored. `domain/verification.rs` holds the rules:

- **`PAGE_COUNT_MISMATCH`**: PPTX family only, when the declared count differs from the PDF. It is skipped when hidden slides or notes pages are included, and for a template with no slides.
- **`FONT_SUBSTITUTED`**: a requested family that `build-info.json` does not list, unless the source embeds it, or `fonts.json` marks it metric-compatible and its substitute is installed. At most 32 per request. A DOCX embeds a family when the font table that its main part links (`word/fontTable.xml`) has a `w:embedRegular`, `w:embedBold`, `w:embedItalic` or `w:embedBoldItalic` element for it. That element's relationship must point at a font part in the package whose first bytes, after deobfuscation with the element's `w:fontKey` when it has one, are the directory of a single font: an sfnt offset table with 1 to 1024 records, each pointing inside the part, that lists `cmap`, `head`, `hhea`, `hmtx`, `maxp` and outlines (`glyf` and `loca`, or CFF), each at least as long as the fixed part of its format. Only the transitional and strict OOXML font-table and font relationship types count. LibreOffice then uses that font, so nothing is substituted. The font check is skipped when the main part's relationships, the linked font table, its relationships or a font part is present but cut short, malformed or unreadable. It is also skipped when these reads exceed the font scan's 32 MiB. Without `build-info.json`, as outside the image, the check is skipped. The substitute that the warning names is the one fontconfig predicted when the image was built (`font_substitutes` and `default_substitute` in `build-info.json`). It is not read from the PDF, so it is right only while the fonts are those of that build. For a run on a host, `build-info.json` must come from that host's fontconfig, or the names can be wrong.
- **`BLANK_PAGE`**: a page with content that rendered as one flat color, from the rendered page or the probe render. Only `output=pdf` is probed. With `images` or `bundle`, every requested page is rendered and decides for itself, so a request over `MAX_RENDER_PAGES` is refused without a probe. For a PDF the worker probes at most `VERIFY_PROBE_MAX_PAGES` (200) pages. It also leaves out a page over 4 Mpx at the probe's 18 dpi. Both kinds are counted in `convert_verify_skipped_total{check="blank_page",reason="pages"}`.
- **`MACROS_IGNORED`**: a VBA project, an Excel 4 macro sheet, an ODF `Basic/` module or `Scripts/` entry, or an OLE2 macro storage.
- **`RENDER_CLAMPED`**: `MAX_DPI` or `MAX_EDGE_PX` lowered the requested resolution.

The scanners stream each part through quick-xml under fixed caps: 4 MiB per part, 32 MiB per font scan and 64 MiB per request. A part that is missing, malformed or over a cap skips its check, and `convert_verify_skipped_total` counts the skip by check and reason. A scanner must not panic, because a panic ends the office worker.
<!-- /memoria:section -->

<!-- memoria:section id="lifecycle" files="main.rs rest/server.rs rest/ops_handlers.rs observability/metrics.rs" -->
## Lifecycle and metrics

`convertd serve` creates `TMP_DIR` and takes an exclusive lock on `TMP_DIR/.lock` for its whole life. If another process holds that lock, it stops with an error before it touches anything. Then it removes the `job-*` directories that a crash left. On SIGTERM or SIGINT it stops in five steps:

1. It sets the draining flag: `/readyz` answers 503 with `draining`, and new conversions answer 503 `SERVICE_UNAVAILABLE` with `Retry-After: 5`.
2. It waits until no request holds a permit, at most `SHUTDOWN_GRACE_SECONDS`. The listener stays open during this time. A request admitted before the signal finishes, even if its upload is still arriving, and its later engine calls still get a worker.
3. It calls the engines' `drain`, so the pools lease no more workers.
4. It stops axum, which stops accepting and gives open connections 2 s more.
5. It calls the engines' `shutdown` with what is left of the deadline, or zero, then exits 0.

The deadline is set once, at the signal: `SHUTDOWN_GRACE_SECONDS` from then. Steps 2 and 5 share it, so a worker that hangs cannot stretch the exit past one grace plus the 2 s for connections.

`/metrics` renders one process-wide Prometheus recorder in the text format. 001 records `convert_requests_total`, `convert_stage_duration_seconds`, `convert_input_bytes`, `convert_warnings_total`, `convert_verify_skipped_total`, `convert_pages_rendered_total`, `convert_responses_cut_total` and `convert_disk_budget_bytes`. On Linux it adds the supervisor's RSS, peak RSS, open descriptors and threads from `/proc/self` at each scrape. A 200 whose body stalls or aborts counts once in `convert_requests_total` with code `OK` and once in `convert_responses_cut_total`. The pools and the adapters in `engines/` record their own metrics, which [engines/README.md](engines/README.md) lists. `METRICS_ENABLED=false` makes `/metrics` answer 404.
<!-- /memoria:section -->

<!-- memoria:section id="contract" files="main.rs" -->
## Engine selection

`main.rs` calls `engines::select::start` once, after it binds the listener and before it builds the application state. `TEST_ENGINE` picks the real pools, the level-B test worker or the level-A fakes. `config.rs` accepts `TEST_ENGINE` only in a build with the `test-hooks` feature. [Selection](engines/README.md#selection) gives the three modes and the readiness states.

Image inputs are out of v1 (owner, 2026-11-26). Detection still names a PNG, JPEG, TIFF, GIF, BMP or WebP file by its signature. So an image is never taken for a PDF inside it. Then it answers 415 `UNSUPPORTED_FORMAT`, and `/v1/capabilities` does not list image inputs. [Not built yet](engines/README.md#not-built-yet) lists the parts of the engines that are missing.
<!-- /memoria:section -->

<!-- memoria:section id="measurement" files="rest/timing.rs" -->
## Measurement

The crate has three targets of its own. They are 1 ms p50 of overhead, 30 MB of idle RSS, and memory per request that does not grow with the file. Three tools measure them with the level-A fakes:

- `examples/overhead.rs` sends keep-alive requests from a raw client, one at a time. It serves in-process with the fakes, or drives a running server or image with `--url`. It prints p50, p95 and p99 of every `Server-Timing` stage and of five figures: `overhead` (`total` − `queue` − Σ `*_engine`), `overhead_strict`, 001's share (`total` − Σ engine-stage walls), 002's share and the client time. Run it with `cargo run --release --example overhead -- --help`.
- `tests/peak_heap.rs` counts the live heap with its own allocator while it uploads PDFs of 1, 16 and 100 MiB.
- `tests/rss_linux.rs` reads `VmRSS` of `convertd serve` before and after 1 000 requests. It runs only on Linux, with `--ignored` and `--features test-hooks`.

The numbers that follow come from macOS on arm64 with the level-A fakes, measured on 2026-11-26 and recorded in commits `1eaf4e4` and `1eafd21`. The example used a release build, and the heap and allocation tests a debug build. Linux has not been measured.

| Measurement | Result |
| --- | --- |
| 001's share, 15 KB DOCX, engine time 30 ms | 0.15 ms p50, 0.51 ms p99 |
| 001's share, 15 KB PDF, engine time 30 ms | 0.26 ms p50, 0.61 ms p99 |
| `overhead`, 15 KB DOCX, engine time 0 | 0.27 ms p50, 0.45 ms p99 |
| `overhead`, 15 KB PDF, engine time 0 | 0.15 ms p50, 0.28 ms p99 |
| Peak heap per upload, 1 MiB / 16 MiB / 100 MiB | 3.1 MB / 3.6 to 4.1 MB / 3.6 to 4.1 MB |
| Allocations per 15 KB `output=pdf` request | 340 |

hyper reads up to about 496 KiB at a time, and up to four such chunks wait in the upload queue. A 1 MiB upload never fills the queue, so its peak is lower. With a queue of one chunk, every size peaks at 3.06 MB.

The benchmark of record ran the same example against the real image (`rs-convert-lead-runtime:wave2b`, linux/arm64) on 2026-11-26. The container had 4 of the host's 14 CPUs and 2 office and 2 render workers. It measured 6 000 warm requests per case, one at a time. The Mac ran on battery, at a load average of about 4.1 from other apps. [bench/results/2026-11-26-wave2b.md](../bench/results/2026-11-26-wave2b.md) has every stage and the conditions.

| Case | `overhead` p50 / p99 | `overhead_strict` p50 / p99 | 001's share p50 |
| --- | --- | --- | --- |
| One-page DOCX to PDF (the 5 ms gate) | 0.883 / 1.267 ms | 1.736 / 2.372 ms | 0.020 ms |
| PDF to PDF | 0.047 / 0.070 ms | 0.279 / 0.386 ms | 0.006 ms |
| One-page DOCX to bundle, JPEG at 72 dpi | 1.011 / 1.637 ms | 1.876 / 2.722 ms | 0.070 ms |

For a DOCX, most of `overhead` is the office worker's pre-flight scan, 0.73 ms at p50.
<!-- /memoria:section -->

<!-- memoria:section id="setup" files="config.rs" -->
## Configuration

`config.rs` parses every environment variable by hand when `convertd serve` starts. A bad value stops `convertd serve` before it listens, and the error names the variable. Lane 002's engine and pool variables are parsed in [engines/config.rs](engines/config.rs). The two parsers hold the names, the defaults and the checks. `convertd openapi` and `convertd worker` do not parse this configuration.

This is every variable that an operator can set. An empty value counts as unset.

| Variable | Default | Meaning |
| --- | --- | --- |
| `API_KEY` | required | The bearer key for every `/v1` route |
| `API_KEY_PREVIOUS` | unset | A second accepted key, for key rotation |
| `HOST` | `0.0.0.0` | The listen address |
| `PORT` | `8080` | The listen port; `convertd healthcheck` reads it too |
| `LOG_MODE` | `prod` | `prod` (JSON) or `debug` |
| `MAX_CONCURRENT_REQUESTS` | `16` | Conversions admitted at once; beyond it, 429 before the body is read |
| `MAX_UPLOAD_BYTES` | `125829120` (120 MiB) | The largest upload; beyond it, 413 |
| `TIMEOUT_DEFAULT_SECONDS` | `120` | The deadline when a request sets no `timeout_seconds` |
| `TIMEOUT_MAX_SECONDS` | `210` | The largest `timeout_seconds`; at most 210 |
| `MAX_RENDER_PAGES` | `500` | The most pages one request renders as images |
| `MAX_DPI` | `300` | Higher DPI is lowered to it, with `RENDER_CLAMPED` |
| `MAX_EDGE_PX` | `4096` | The longest page-image edge |
| `VERIFY_PROBE_RENDER` | `true` | The blank-page probe for `BLANK_PAGE` |
| `VERIFY_PROBE_MAX_PAGES` | `200` | The most pages the probe looks at |
| `IO_IDLE_TIMEOUT_SECONDS` | `30` | An upload or a response that moves nothing this long ends |
| `UPLOAD_MIN_BYTES_PER_SECOND` | `16384` | The upload floor over each idle window; `0` turns it off |
| `RESPONSE_MIN_BYTES_PER_SECOND` | `16384` | The response floor over each idle window; `0` turns it off |
| `SHUTDOWN_GRACE_SECONDS` | `30` | How long SIGTERM waits for requests in flight |
| `TMP_DIR` | `/tmp/convert` | Job directories and worker slots |
| `TMP_MAX_BYTES` | `3221225472` (3 GiB) | The disk budget under `TMP_DIR`; one job writes at most a quarter of it |
| `TOKIO_WORKER_THREADS` | `2` | The supervisor's async threads |
| `METRICS_ENABLED` | `true` | `false` makes `/metrics` answer 404 |
| `SWAGGER_ENABLED` | `true` | Serves the spec at `/swagger/doc.json` |
| `BUILD_INFO_PATH` | `/opt/convertd/build-info.json` | Engine versions and fonts, written at image build |
| `FONTS_PATH` | `/etc/convertd/fonts.json` | The metric-compatible font table |
| `OFFICE_WORKERS` | half of the CPUs, at least 1 | Office worker processes |
| `RENDER_WORKERS` | all of the CPUs | Render worker processes |
| `QUEUE_CAPACITY` | 2 × `OFFICE_WORKERS` | Requests that may wait for an office worker |
| `QUEUE_WAIT_SECONDS` | `10` | The longest wait in that queue; beyond it, 429 |
| `WORKER_MAX_JOBS` | `50` | Conversions before a worker is replaced. A `Ping` and an `Inspect` do not count |
| `OFFICE_MAX_JOBS` | `200`, or `WORKER_MAX_JOBS` when that is set | The same, for the office pool |
| `RENDER_MAX_JOBS` | `WORKER_MAX_JOBS` | The same, for the render pool |
| `WORKER_MAX_RSS_MB` | `1024` | Memory above which a worker is replaced after its job |
| `OFFICE_MAX_RSS_MB`, `RENDER_MAX_RSS_MB` | `WORKER_MAX_RSS_MB` | The same, for one pool |
| `WORKER_MAX_AGE_SECONDS` | `3600` | Age at which an idle worker is replaced |
| `WORKER_KILL_RSS_MB` | `2048` | Memory at which a worker is killed during a job |
| `OFFICE_KILL_RSS_MB`, `RENDER_KILL_RSS_MB` | `WORKER_KILL_RSS_MB` | The same, for one pool |
| `LO_PROGRAM_DIR` | `/opt/libreoffice/program` | The LibreOffice program folder |
| `LO_PROFILE_TEMPLATE` | unset | A read-only LibreOffice profile, copied per worker |
| `PDFIUM_LIB_DIR` | `/opt/pdfium/lib` | The folder of `libpdfium.so` |
| `POISON_TTL_SECONDS` | `600` | How long a file that crashed a worker is refused; `0` turns it off |
| `DEFAULT_PAPER` | `A4` | `A4` or `Letter`: the page of a document that leaves it to LibreOffice, such as a sheet without a page setup. A DOCX without a page size and an RTF without paper dimensions are always Letter |
| `TEST_ENGINE` | unset | `fake` or `worker`; only in a build with `test-hooks` |

`FONTS_PATH` (default `/etc/convertd/fonts.json`) names the `{family: metric_compatible}` table for `FONT_SUBSTITUTED`. A missing file only logs a warning, and then no family counts as metric-compatible.
<!-- /memoria:section -->
