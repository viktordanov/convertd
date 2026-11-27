<!-- memoria:section id="overview" files="mod.rs" -->
# Workers

`src/worker/` is the code that runs inside a `convertd worker <role>` process: one thread, one engine and one call at a time.

Four duties apply to every change in this folder:

- **Never drop the LibreOffice `Office` instance.** A second instance in one process is refused, and dropping it destroys the instance. A worker exits on its own only through `sys::exit`, which calls `_exit`, so no destructor runs.
- **Never call `trim_memory`.** On LibreOffice 26.8.3 the next document load then crashes with SIGBUS (`docs/m0-spikes.md`).
- **Call PDFium from one thread only.** PDFium is not thread safe. A worker has no tokio runtime and no thread of its own besides the main thread.
- **Keep every `unsafe` block and every engine call in this folder.** No other folder of the crate holds them.

`convertd worker office` runs LibreOffice through LibreOfficeKit, `convertd worker render` runs PDFium, and `convertd worker test` answers with fake results. The `test` role exists only in builds with the cargo feature `test-hooks`. Image inputs are out of v1, so no worker assembles images. [Not built yet](#not-built-yet) lists what is missing.

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

The worker reads its RSS after every call. On Linux that is its private memory, `RssAnon` + `RssShmem` from `/proc/self/status`, which it keeps open. `RssFile` is left out: the file pages, such as LibreOffice's libraries, are shared by every worker and the kernel can drop them. In a warm office worker they are about 156 MiB of a `VmRSS` of 248 MiB (2026-11-27). A kernel without `RssAnon` gives `VmRSS`. On macOS the worker calls `proc_pidinfo` with `PROC_PIDTASKINFO`, and on other systems the value is 0. `sys::rss_of(pid)` reads another process in the same ways, for the RSS watchdog of the supervisor in `src/engines/pool.rs`. It returns `None` when the process is gone or the system has no reader.

A worker reads no configuration. The supervisor gives it a fixed environment and the `Init` frame, as the [slot directory section](../engines/README.md#slot-directory) of the engines README describes.
<!-- /memoria:section -->

<!-- memoria:section id="usage" files="mod.rs" -->
## Calls

After `Ready`, the worker reads one `Call` at a time and writes one `Reply` with the same `id`. The worker answers `Ping` itself. It gives every other operation to its engine through the `Engine` trait, whose `handle` returns the result and the engine times.

The `job` field of a reply counts the conversions that this process served. A `Ping` and an `Inspect` do not count, so `WORKER_MAX_JOBS` counts conversions (`counts_as_job`). `Test` counts, because it stands in for a conversion at level B. The `rss_bytes` field is the RSS after the call. The engine times run from just before the document is opened to just after the last engine call. Process start, the bind and IPC are outside them. The `cpu_us` field is the CPU time of the whole process over the call, user plus system, from `getrusage(RUSAGE_SELF)` before and after the call. It covers every thread, so it includes LibreOffice's own threads and the pre-flight scan. It does not cover child processes. A worker serves one call at a time, so the difference is the CPU time of that call. If a reply does not fit in one 16 MiB frame, the worker sends a `LimitExceeded` error with the same `id` instead.
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

<!-- memoria:section id="office" files="office.rs warmup.rs docx_paper.rs" -->
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

The DOCX importer of LibreOffice 26.8.3 starts every section on Letter and never reads `LC_PAPER`. So for a DOCX, DOCM or DOTX, `docx_paper` gives each section without a `w:pgSz` one with the `DEFAULT_PAPER` size before the load. The worker learns the paper from its `LC_PAPER`. The sections are the `w:sectPr` of the body and of a paragraph's `w:pPr`, and a body without one gets one. The new element goes after the header and footer references, `w:footnotePr`, `w:endnotePr` and `w:type`, where the schema puts it. A package in which any section has a `w:pgSz` is loaded as it came. Its other sections may take their size from that one, as a continuous break does.

The worker writes the changed package into `.convertd-paper/` in the job directory. It renames the source into that folder and the copy into the source's place. LibreOffice thus loads the copy under the source's own name and folder, so a FILENAME field and relative links stay as they were. After the export, the source goes back over the copy and the folder is removed. The source is thus moved for the duration of the load. A retry after a crashed worker puts it back first, before the pre-flight reads it. Anything at `.convertd-paper` that is not a folder is left alone.

Only the main part changes, with its own compression method. Every other entry is copied byte for byte, data descriptors included. The copy may be at most the source size plus 1 MiB, which the supervisor reserves beside the PDF. With `DEFAULT_PAPER=Letter` there is no copy and no reservation, because Letter is the importer's own default.

The main part is inflated at most twice, once to plan and once to rewrite. Each pass reads at most 64 MiB, the size of the pre-flight's per-request cap, and the archive is read under detection's caps. A failed write of the copy, such as a full disk, fails the job with the worker's I/O error. A busy disk thus never changes the paper. A copy that cannot be removed fails the job too, because the supervisor gives its reservation back when the job answers. The time counts as `preflight_us`.

For these packages the worker writes no copy, and LibreOffice loads the original unchanged. Its sections without a page size then get the importer's Letter. The copy step itself raises no error for them; only a failed write or removal does. LibreOffice can still refuse a damaged package.

- ZIP64 anywhere: the end record, a directory record or a local header.
- An encrypted main part, overlapping records, or bytes after the counted directory records.
- A stored main part with a data descriptor, which LibreOffice refuses (corpus case `docx-stored-descriptor`). The copy would load, so the paper setting would decide whether the document converts.
- A main part whose local header, data descriptor, CRC or size disagrees with its directory record.
- A signed package: a signature-origin relationship, or an `_xmlsignatures/` folder.
- A root relationships part that is truncated or empty, or has another or a second root element.
- A root relationships part with a relationship below the root, a branch, an escaped namespace, or more than 8 levels.
- A main part over 64 MiB inflated, or a copy over the source size plus 1 MiB.
- A main part that is not UTF-8, by its bytes or its declaration.
- A main part deeper than 4096 elements, or with more than 512 namespace declarations in scope.
- A namespace spelled with a character reference, or a prefix over 64 bytes.
- More than 100 000 sections, or more than 16 distinct texts to insert.
- A section or body in an `mc:AlternateContent` branch, an `mc:AlternateContent` in a section, or a body that is not the root's child.
- A `w:pgSz` or an earlier section child in a wrapper inside a section, or a section in a wrapper under `mc:ProcessContent`.

The callback counts the password prompts of one load. A failed load is `PasswordRequired` after one prompt without a password, `InvalidPassword` after a second prompt with a password, and `LoadFailed` otherwise.

The export runs with `RLIMIT_FSIZE` at `max_output_bytes`. The load runs under a looser cap, `max(4 × max_output_bytes, 1 GiB)`, because it writes LibreOffice's own temp files. The SIGXFSZ handler records a write past either cap, and the job is then `LimitExceeded`. A failed save with no such write is `DiskFull` when `statvfs` shows less than 1 MiB free, and `ExportFailed` otherwise.

A password with a NUL byte is `InvalidPassword` before any engine call, in both workers, because the bindings cannot pass it on. The office worker always answers a password prompt, since LibreOffice waits for the answer.
<!-- /memoria:section -->

<!-- memoria:section id="render" files="render.rs encode.rs" -->
## Render worker

`RenderWorker::start` binds `libpdfium.so` from the `Init` directory. With `Init.warmup`, it renders a one-page document with a text object at 72 DPI and encodes it as JPEG, all in memory. The worker opens one document for each operation and closes it before it answers. Thus it holds one document at a time, as PDFium requires.

The operations do this:

- **Inspect** reads the page count and the page sizes without loading the pages. `encrypted` comes from the security handler revision. `needs_password` comes from a second open without the password. With `probe_dpi`, it renders the pages that `probe_pages` names in grayscale and reports the object count and the flat check, timed apart as `probe_us`. `Only` names the pages of a selection, and numbers past the page count drop out. `All { max }` takes the first `max` pages and reports the others as `probe_skipped`. A page whose probe bitmap would pass 4 Mpx is left out before PDFium allocates it, and is counted in `probe_skipped` too.
- **Render** scales each page by `min(dpi / 72, max_edge / longer edge)` into a BGRA bitmap. pdfium-render reverses the byte order to RGBA by default, so every render in the worker turns the reversal off. Each page renders into one buffer of the job, which PDFium writes through `PdfBitmap::from_bytes`, so no copy of the bitmap is made, and the buffer keeps its allocation from page to page. JPEG reads the BGRA bytes directly. PNG uses the adaptive filter. A page whose stored PNG would be at most 1 MiB takes image's whole-page encoder (fast, fdeflate), with one RGB copy of at most 1 MiB, so its bytes stay as before. A larger page converts one row at a time to RGB and streams it into png's `StreamWriter` with zlib level 2, so no RGB copy of it exists. These large pages changed encoder: png's stream gives fdeflate the filter byte and the row in two calls, and fdeflate's output then depends on the page width (+43 % for a white 93 × 3777 px page). zlib's output does not depend on the calls. Level 2 makes blank and text pages about a quarter to a half of the old size (a blank A4 page at 150 dpi: 10 161 B against 36 294 B), for about 1.5 times the old encode time. zlib stores what does not compress, so noise stays within 0.2 % of the old size. The pixels are the same (unit test). An A4 page at 72 dpi is already past 1 MiB stored, so `docker/ab-pages.sh` sees new page hashes for nearly every page after this change. On the corpus, all 192 pages were pixel-identical to the previous encoder, and the PNG bytes fell from 104.0 MB to 87.9 MB (arm64, 2026-11-26). If the allocator refuses a bitmap, the render fails with an error and the worker does not abort. Before PDFium allocates the bitmap, a page whose long edge passes the format's limit is `LimitExceeded`: 65535 px for JPEG. The output budget covers all pages, and an error removes every file of the job.
- **Decrypt** calls `FPDF_SaveAsCopy` with `FPDF_NO_INCREMENTAL | FPDF_REMOVE_SECURITY` through the raw bindings. The copy streams into the output file through a 64 KiB buffer, under the output cap, and an error removes the file. The whole PDF is never in memory. The crate's own save cannot remove security. On PDFium 8179 the copy opens without a password and has no security handler (tested in the dev image). The worker also opens the copy again without a password. If the copy still has a security handler, the job is `ExportFailed`. With `check_owner`, the worker never drops owner restrictions that the caller did not unlock. It writes the copy only when the user permissions already grant everything or the password unlocked the owner. Otherwise it writes nothing. A password that opened the file is the user password, and the answer is `Restricted`. Without a password, the file is owner-only, and the answer is `InvalidPassword`. PDFium unlocks the owner only for a non-empty password that passes the owner check. Then `FPDF_GetDocPermissions` grants every bit except bits 1 and 2, which the Standard handler clears (0xFFFFFFFC), and it differs from `FPDF_GetDocUserPermissions`, which keeps the file's restricted P. The chromium/8179 source shows this, and the engine test checks it.

A missing password is `PasswordRequired`, and a wrong one is `InvalidPassword`. The flat check (`encode::flat`) is true when every pixel is within 4 levels per channel of pixel (0,0).
<!-- /memoria:section -->

<!-- memoria:section id="test-role" files="test_worker.rs" -->
## Test role

`convertd worker test` has no engine. It serves the pool tests and level B (`TEST_ENGINE=worker`). `Init.test` can make its start fail (`fatal`) or slow (`ready_delay_ms`, which counts as `bind_us`).

It answers `Convert`, `Inspect`, `Render` and `Decrypt` with small valid files. The `FAKE:` markers in the first 1 MiB of the input file control the answer, and the first marker wins:

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
| `Burn { ms }` | Spins until the process used `ms` more CPU time, then answers. It stops after 10 × `ms` of wall time. |
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

