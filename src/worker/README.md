<!-- memoria:section id="overview" files="mod.rs" -->
# Workers

`src/worker/` is the code that runs inside a `convertd worker <role>` process: one thread, one engine and one call at a time.

Four duties apply to every change in this folder:

- **Never drop the LibreOffice `Office` instance.** A second instance in one process is refused, and dropping it destroys the instance. A worker leaves only through `sys::exit`, which calls `_exit`, so no destructor runs.
- **Never call `trim_memory`.** On LibreOffice 26.8.3 the next document load then crashes with SIGBUS (`docs/m0-spikes.md`).
- **Call PDFium from one thread only.** PDFium is not thread safe. A worker has no tokio runtime and no thread of its own besides the main thread.
- **Keep every `unsafe` block and every engine call in this folder.** No other folder of the crate holds them.

Only the `test` role works today. `convertd worker office` and `convertd worker render` answer `Init` with `Fatal` ("not built yet") and exit with status 1. The `test` role exists only in builds with the cargo feature `test-hooks`. [Not built yet](#not-built-yet) lists what is missing.

The supervisor side is in [`src/engines/`](../engines/README.md), and the frames are in [`src/ipc/`](../ipc/README.md).

## Contents

1. [Start](#start)
2. [Calls](#calls)
3. [Leaving](#leaving)
4. [Output files](#output-files)
5. [Test role](#test-role)
6. [Not built yet](#not-built-yet)
<!-- /memoria:section -->

<!-- memoria:section id="lifecycle" files="mod.rs sys.rs" -->
## Start

`main.rs` dispatches `convertd worker <role>` to `worker::run(role)` before it builds a runtime, installs logging or reads the configuration. `run` never returns. It does these steps in order:

1. **Process set-up** (`sys::setup`). On Linux, it arms the parent-death signal (SIGKILL). If `CONVERTD_SUPERVISOR_PID` is set and is not the parent, it exits with status 0. It sets SIGXFSZ to ignored, so a write past `RLIMIT_FSIZE` fails with `EFBIG`.
2. **Descriptor moves.** It copies fds 0 and 1 to new close-on-exec descriptors for the frames. Then it points fd 1 at stderr and fd 0 at `/dev/null`, so an engine that prints to stdout cannot corrupt a frame.
3. **Panic hook.** A panic writes its message to stderr and exits with status 101, with no unwinding through engine state.
4. **`Init`.** It reads the first frame. If the protocol version differs from `PROTOCOL_VERSION`, it sends `Fatal` and exits with status 1.
5. **Engine start.** It starts the engine of its role and times the start as `bind_us`. If the start fails, it sends `Fatal` and exits with status 1. Otherwise it sends `Ready` with its PID, the engine version, `bind_us` and its RSS.

The worker reads its RSS after every call. On Linux it reads `/proc/self/statm`, which it keeps open. On macOS it calls `proc_pidinfo` with `PROC_PIDTASKINFO`. On other systems the value is 0.

A worker reads no configuration. The supervisor gives it a fixed environment and the `Init` frame, as the [slot directory section](../engines/README.md#slot-directory) of the engines README describes.
<!-- /memoria:section -->

<!-- memoria:section id="usage" files="mod.rs" -->
## Calls

After `Ready`, the worker reads one `Call` at a time and writes one `Reply` with the same `id`. The worker answers `Ping` itself. It gives every other operation to its engine through the `Engine` trait, whose `handle` returns the result and the engine times.

The `job` field of a reply counts every call that this process served, `Ping` included. The `rss_bytes` field is the RSS after the call. The engine times run from just before the document is opened to just after the last engine call. Process start, the bind and IPC are outside them.
<!-- /memoria:section -->

<!-- memoria:section id="failure" files="mod.rs sys.rs" -->
## Leaving

A worker leaves only through `sys::exit`, which calls `_exit`. The exit status tells why:

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

<!-- memoria:section id="test-role" files="test_worker.rs" -->
## Test role

`convertd worker test` has no engine. It serves the pool tests, the `engine bench` command and level B. `Init.test` can make its start fail (`fatal`) or slow (`ready_delay_ms`, which counts as `bind_us`).

It answers `Convert`, `Inspect`, `Render`, `Assemble` and `Decrypt` with small valid files. The `FAKE:` markers in the first 1 MiB of the input file control the answer, and the first marker wins:

- `FAKE:DELAY_MS=<n>` sleeps, then succeeds. The pool's deadline turns a long delay into a timeout.
- `FAKE:PAGES=<n>` succeeds with `n` pages. `FAKE:HANG` never answers. `FAKE:PANIC` ends the worker with status 101.
- `FAKE:PASSWORD_REQUIRED`, `FAKE:INVALID_PASSWORD`, `FAKE:LIMIT_EXCEEDED` and `FAKE:INTERNAL_ERROR` give those errors. `FAKE:CONVERSION_FAILED` gives `LoadFailed`.
- `FAKE:TOO_MANY_REQUESTS` and `FAKE:SERVICE_UNAVAILABLE` give `FakeQueueFull` and `FakeUnavailable`. Only the test role sends these two kinds.

The test role parses the markers with its own interim copy of lane 001's rules. Lane 001's parser, `engines::fake_markers::parse`, is on `main`, but the test role does not call it yet. A `TODO(001 fake_markers)` in `test_worker.rs` marks the switch.

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

- The office worker (step 8). It must keep `Office` alive until `_exit`, register the password callback once, convert the warm-up files before `Ready`, and set `RLIMIT_FSIZE` for each job.
- The render worker with PDFium (step 7): inspect, render, decrypt and the image encoders, and image assembly (step 11).
- The pre-flight (step 8). The office worker must run lane 001's scanners in `src/preflight/` on the source file before LibreOffice loads it, and return the facts in the reply.
