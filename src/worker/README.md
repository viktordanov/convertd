<!-- memoria:section id="overview" files="mod.rs" -->
# Workers

`src/worker/` is the code that runs inside a `convertd worker <role>` process: one thread, one engine and one call at a time.

Four duties apply to every change in this folder:

- **Never drop the LibreOffice `Office` instance.** A second instance in one process is refused, and dropping it destroys the instance. A worker exits on its own only through `sys::exit`, which calls `_exit`, so no destructor runs.
- **Never call `trim_memory`.** On LibreOffice 26.8.3 the next document load then crashes with SIGBUS (`docs/m0-spikes.md`).
- **Call PDFium from one thread only.** PDFium is not thread safe. A worker has no tokio runtime and no thread of its own besides the main thread.
- **Keep every `unsafe` block and every engine call in this folder.** No other folder of the crate holds them.

`convertd worker office` runs LibreOffice through LibreOfficeKit, `convertd worker render` runs PDFium, and `convertd worker test` answers with fake results. The `test` role exists only in builds with the cargo feature `test-hooks`. Image assembly is out of v1. [Not built yet](#not-built-yet) lists what is missing.

The supervisor side is in [`src/engines/`](../engines/README.md), and the frames are in [`src/ipc/`](../ipc/README.md).

## Contents

1. [Start](#start)
2. [Calls](#calls)
3. [Leaving](#leaving)
4. [Output files](#output-files)
5. [Office worker](#office-worker)
6. [Render worker](#render-worker)
7. [Test role](#test-role)
8. [Not built yet](#not-built-yet)
<!-- /memoria:section -->

<!-- memoria:section id="lifecycle" files="mod.rs sys.rs" -->
## Start

`main.rs` dispatches `convertd worker <role>` to `worker::run(role)` before it builds a runtime, installs logging or reads the configuration. `run` never returns. It does these steps in order:

1. **Process set-up** (`sys::setup`). On Linux, it arms the parent-death signal (SIGKILL). Also on Linux, if `CONVERTD_SUPERVISOR_PID` is set and is not the parent, it exits with status 0. It sets SIGXFSZ to ignored, so a write past `RLIMIT_FSIZE` fails with `EFBIG`.
2. **Descriptor moves.** It copies fds 0 and 1 to new close-on-exec descriptors for the frames. Then it points fd 1 at stderr and fd 0 at `/dev/null`, so an engine that prints to stdout cannot corrupt a frame.
3. **Panic hook.** A panic writes its message to stderr and exits with status 101, with no unwinding through engine state.
4. **`Init`.** It reads the first frame. If the protocol version differs from `PROTOCOL_VERSION`, it sends `Fatal` and exits with status 1.
5. **Engine start.** It starts the engine of its role and times the start as `bind_us`. If the start fails, it sends `Fatal` and exits with status 1. Otherwise it sends `Ready` with its PID, the engine version, `bind_us` and its RSS.

The worker reads its RSS after every call. On Linux it reads `/proc/self/statm`, which it keeps open. On macOS it calls `proc_pidinfo` with `PROC_PIDTASKINFO`. On other systems the value is 0. `sys::rss_of(pid)` reads the RSS of another process in the same two ways, for the RSS watchdog of the supervisor in `src/engines/pool.rs`. It returns `None` when the process is gone or the system has no reader.

A worker reads no configuration. The supervisor gives it a fixed environment and the `Init` frame, as the [slot directory section](../engines/README.md#slot-directory) of the engines README describes.
<!-- /memoria:section -->

<!-- memoria:section id="usage" files="mod.rs" -->
## Calls

After `Ready`, the worker reads one `Call` at a time and writes one `Reply` with the same `id`. The worker answers `Ping` itself. It gives every other operation to its engine through the `Engine` trait, whose `handle` returns the result and the engine times.

The `job` field of a reply counts every call that this process served, `Ping` included. The `rss_bytes` field is the RSS after the call. The engine times run from just before the document is opened to just after the last engine call. Process start, the bind and IPC are outside them. If a reply does not fit in one 16 MiB frame, the worker sends a `LimitExceeded` error with the same `id` instead.
<!-- /memoria:section -->

<!-- memoria:section id="failure" files="mod.rs sys.rs" -->
## Leaving

A worker exits on its own only through `sys::exit`, which calls `_exit`. The exit status tells why:

| Status | Cause |
| --- | --- |
| 0 | EOF on the frame pipe, before `Init` or between calls. A failed frame write also exits with 0, because it means that the supervisor is gone. On Linux, also a dead supervisor at start-up. |
| 1 | `Fatal` sent: a protocol mismatch or a failed engine start. |
| 2 | The process set-up failed, the first frame was not `Init`, a second `Init` came, a frame read failed, or a frame did not decode. |
| 101 | A panic. |

The supervisor usually ends a worker with SIGKILL of its process group, for a timeout, a canceled call or recycling. The worker has no shutdown code of its own.
<!-- /memoria:section -->

<!-- memoria:section id="contract" files="output.rs" -->
## Output files

The files that a worker writes itself go through `OutputBudget`. The budget holds the `max_output_bytes` of one job and counts every file of the job. It refuses a file that does not fit before the file reaches the disk. When a job fails, `remove_written` deletes the files that the job wrote.

`classify_io` maps an I/O error to the kind that the reply carries:

- `ENOSPC` and `EDQUOT` give `DiskFull`. Other jobs filled the disk, so a retry can work.
- `EFBIG`, which the job's own `RLIMIT_FSIZE` causes, gives `LimitExceeded`. A file over the budget also gives `LimitExceeded`. This cap is final.
- Every other error gives `Io`.

For `DiskFull` and `LimitExceeded`, `io_error` writes a fixed message with no path in it.
<!-- /memoria:section -->

<!-- memoria:section id="office" files="office.rs warmup.rs" -->
## Office worker

`OfficeWorker::start` creates the one `Office` of the process and keeps it in a `ManuallyDrop` from the next line on, so no error path drops it. Then it does these steps:

1. It installs its own SIGXFSZ handler. LibreOffice's start-up installs a handler that turns a write past `RLIMIT_FSIZE` into a crash ("Fatal exception: Signal 25").
2. It turns on the password callback and registers it once.
3. With `Init.warmup`, it converts a DOCX, an XLSX and a PPTX to PDF before `Ready`. A failed warm-up file goes to stderr and does not stop the worker. `warmup.rs` builds the files in memory as stored ZIP archives, so no binary fixture is in the repository.

The DOCX and the XLSX are shaped like real documents. Both have a theme with Cambria headings and Calibri body text. The DOCX also has styles, a heading, a bullet list, Arial and Times New Roman runs and a table. The XLSX has Calibri styles, a bold header, a formula and two sheets. The PPTX has one text box and a theme with Calibri Light and Calibri. The aim is that the first real job finds the import paths, the substitute fonts and the PDF font embedding already loaded.

On 2026-11-26 a bench measured the warm-up in the dev image `rs-convert-003-dev:arm64`. It ran on Linux arm64 in the Docker VM of an Apple-silicon Mac, with a release build with `test-hooks`, `OFFICE_WORKERS=1` and `RENDER_WORKERS=2`. Over six fresh pools, the median first job took 1.04 times the third for `docx/small.docx`, 1.03 for `pptx/deck20.pptx` and 1.02 for `xlsx/sheet3.xlsx`. That meets the plan's target of 10 % on the median. The worst single pool gave 1.72, so one run is not a stable measure of the target. The PPTX stays simple, because its median of 1.03 already meets the target.

Compared with the earlier simple warm-up files, the richer warm-up adds about 10 to 45 ms to spawn-to-`Ready`, which is off the request path. Each figure is the median over the same six pools of the p50 of each pool, and the range covers the three bench runs.

A `Convert` runs `preflight::scan` on the source first, timed apart as `preflight_us`, and returns the facts in the reply. Then it loads the file and exports PDF with explicit JSON filter options, so no setting comes from the profile. The type that LibreOffice loaded picks the options, not the requested family. An encrypted upload arrives as `source.docx` with the document family, whatever it holds, and LibreOffice finds the real type from the content:

- **Presentations**: `ExportNotesPages`, `ExportOnlyNotesPages` and `ExportHiddenSlides`.
- **Spreadsheets**: `SinglePageSheets`.
- **Text documents**: `ExportNotes` off.

Only an encrypted DOCX has a test of this path (`tests/engines_office.rs`). An encrypted spreadsheet or presentation is not tested.

The callback counts the password prompts of one load. A failed load is `PasswordRequired` after one prompt without a password, `InvalidPassword` after a second prompt with a password, and `LoadFailed` otherwise.

The export runs with `RLIMIT_FSIZE` at `max_output_bytes`. The load runs under a looser cap, `max(4 × max_output_bytes, 1 GiB)`, because it writes LibreOffice's own temp files. The SIGXFSZ handler records a write past either cap, and the job is then `LimitExceeded`. A failed save with no such write is `DiskFull` when `statvfs` shows less than 1 MiB free, and `ExportFailed` otherwise.

A password with a NUL byte is `InvalidPassword` before any engine call, in both workers, because the bindings cannot pass it on. The office worker always answers a password prompt, since LibreOffice waits for the answer.
<!-- /memoria:section -->

<!-- memoria:section id="render" files="render.rs encode.rs" -->
## Render worker

`RenderWorker::start` binds `libpdfium.so` from the `Init` directory. With `Init.warmup`, it renders a one-page document with a text object at 72 DPI and encodes it as JPEG, all in memory. The worker opens one document for each operation and closes it before it answers. Thus it holds one document at a time, as PDFium requires.

The operations do this:

- **Inspect** reads the page count and the page sizes without loading the pages. `encrypted` comes from the security handler revision. `needs_password` comes from a second open without the password. With `probe_dpi`, it renders the pages that `probe_pages` names in grayscale and reports the object count and the flat check, timed apart as `probe_us`. `Only` names the pages of a selection, and numbers past the page count drop out. `All { max }` takes the first `max` pages and reports the others as `probe_skipped`.
- **Render** scales each page by `min(dpi / 72, max_edge / longer edge)` into a BGRA bitmap. pdfium-render reverses the byte order to RGBA by default, so every render in the worker turns the reversal off. JPEG takes the BGRA bytes directly. PNG (fast, adaptive filter) and lossy WebP take one pass to RGB. Before PDFium allocates the bitmap, a page whose long edge passes the format's limit is `LimitExceeded`: 65535 px for JPEG and 16383 px for WebP. Each file gets its CRC-32. The output budget covers all pages, and an error removes every file of the job.
- **Decrypt** calls `FPDF_SaveAsCopy` with `FPDF_NO_INCREMENTAL | FPDF_REMOVE_SECURITY` through the raw bindings, into a buffer under the output cap. The crate's own save cannot remove security. On PDFium 8179 the copy opens without a password and has no security handler (tested in the dev image). The worker also opens the copy again without a password. If the copy still has a security handler, the job is `ExportFailed`. With `require_owner`, the worker writes the copy only when the password unlocked the owner, and otherwise answers `InvalidPassword`. PDFium unlocks the owner only for a non-empty password that passes the owner check. Then `FPDF_GetDocPermissions` grants every bit except bits 1 and 2, which the Standard handler clears (read in the chromium/8179 source).

A missing password is `PasswordRequired`, and a wrong one is `InvalidPassword`. The flat check (`encode::flat`) is true when every pixel is within 4 levels per channel of pixel (0,0).
<!-- /memoria:section -->

<!-- memoria:section id="test-role" files="test_worker.rs" -->
## Test role

`convertd worker test` has no engine. It serves the pool tests, the `engine bench` command and level B (`TEST_ENGINE=worker`). `Init.test` can make its start fail (`fatal`) or slow (`ready_delay_ms`, which counts as `bind_us`).

It answers `Convert`, `Inspect`, `Render`, `Assemble` and `Decrypt` with small valid files. The `FAKE:` markers in the first 1 MiB of the input file control the answer, and the first marker wins:

- `FAKE:DELAY_MS=<n>` sleeps, then succeeds. The pool's deadline turns a long delay into a timeout.
- `FAKE:PAGES=<n>` succeeds with `n` pages. `FAKE:HANG` never answers. `FAKE:PANIC` ends the worker with status 101.
- `FAKE:PASSWORD_REQUIRED`, `FAKE:INVALID_PASSWORD`, `FAKE:LIMIT_EXCEEDED` and `FAKE:INTERNAL_ERROR` give those errors. `FAKE:CONVERSION_FAILED` gives `LoadFailed`.
- `FAKE:TOO_MANY_REQUESTS` and `FAKE:SERVICE_UNAVAILABLE` give `FakeQueueFull` and `FakeUnavailable`. Only the test role sends these two kinds.

The test role parses the markers with lane 001's `engines::fake_markers::parse`, which level A uses too.

A fake PDF has `n` empty A4 pages. When `n` is not 1, it carries a `FAKE:PAGES=<n>` comment, so an `Inspect` of a converted file sees the page count of the upload. `Render` writes a white 8 × 8 image per page as `out_dir/NNNN.<ext>` and reports those pixel sizes. A page number outside the document gives `LoadFailed`. `Decrypt` writes a fake PDF and does not copy the input, so the error markers of the input do not reach the copy.

`Op::Test` acts out the faults that the pool tests need:

| `TestOp` | What the worker does |
| --- | --- |
| `Sleep { ms }` | Sleeps, then answers. |
| `Hang` | Never answers. |
| `Abort` | Calls `abort()` and dies with SIGABRT. |
| `Exit { code }` | Exits with `code` and sends no reply. |
| `Allocate { mb }` | Allocates `mb` MiB, writes to each page, keeps it, and answers its RSS. |
| `WriteStdout { bytes }` | Writes junk to fd 1, which lands on stderr. |
| `SpawnSleeper` | Starts `sleep 1000` in its process group and answers its PID. |
| `Env` | Answers its environment, sorted. |
<!-- /memoria:section -->

## Not built yet

These parts of plan 002 do not exist yet:

- The font list of `Inspect` with `collect_fonts`. It is always empty.

Image assembly is out of v1, so the render worker answers `Assemble` with `Unsupported`.
