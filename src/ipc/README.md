<!-- memoria:section id="overview" files="mod.rs" -->
# Supervisor-worker protocol

`src/ipc/` defines the frames that the supervisor and a worker send over the worker's stdin and stdout pipes. It is internal: no other lane depends on these types.

Only paths and options cross the pipe. The upload, the PDF and the page images stay in the job directory, where the worker reads and writes them. The types are serde types for the wire, not domain types. The engine adapters in `src/engines/` convert between the two.
<!-- /memoria:section -->

<!-- memoria:section id="contract" files="frame.rs" -->
## Frames

A frame is a `u32` little-endian body length and a UTF-8 JSON body of at most 16 MiB (`MAX_FRAME_BYTES`). The writer sends each frame with one `write_all`. It refuses a body above 16 MiB with `FrameError::TooLarge` before it writes a byte. The reader returns these results:

- **EOF before a length**: `Ok(None)`, a clean end.
- **EOF inside a frame**: `FrameError::Truncated`.
- **A length above 16 MiB**: `FrameError::TooLarge`, found before the body is allocated.
- **Invalid JSON**: `FrameError::Json`.

The worker uses the blocking `read_frame` and `write_frame`. The supervisor uses `read_frame_async` and `write_frame_async`. The async functions are not cancel safe: a dropped read or write leaves the stream out of step. Thus the pool kills a worker whose call future was dropped.
<!-- /memoria:section -->

<!-- memoria:section id="usage" files="protocol.rs" -->
## Messages

One call is in flight for each worker at a time, and the reply echoes the call's `id`. The messages are these:

- **`Request::Init`**: the first frame after the spawn. It carries `PROTOCOL_VERSION` (1), the engine paths, the warm-up flag and the start faults of the `test` role.
- **`Request::Call`**: one operation with its `id`. The operations are `Ping`, `Convert`, `Inspect`, `Render`, `Decrypt`, and `Test` for the `test` role only.
- **`Response::Ready`** or **`Response::Fatal`**: the answer to `Init`. `Ready` carries the PID, the engine version, `bind_us` and the RSS.
- **`Response::Reply`**: the answer to a call. It carries the conversion count of the process (`job`, without `Ping` and `Inspect`), the engine times, the RSS, and the result or a `WorkerError`.

A `Ping` call and its reply look like this on the wire:

```json
{"type":"call","id":7,"op":{"op":"ping"}}
{"type":"reply","id":7,"job":1,"engine":{"engine_us":0,"convert_us":0,"inspect_us":0,"probe_us":0,"render_us":0,"encode_us":0,"decrypt_us":0,"preflight_us":0},"rss_bytes":0,"result":{"Ok":{"output":"pong"}}}
```

`Convert` names the office family of the source: `document`, `spreadsheet` or `presentation`. Its reply carries the PDF size, LibreOffice's document type and the pre-flight facts as `SourceFactsWire`. Each `RenderedPage` carries the CRC-32 of its file.

A `WorkerError` has a `kind` and a message for the log. The message can hold paths, so it never reaches an HTTP body. `DiskFull` is apart from `LimitExceeded`: a full disk comes from other jobs, and `LimitExceeded` is the job's own cap. `FakeQueueFull` and `FakeUnavailable` come only from the `test` role.

`Secret` holds a password. Its `Debug` output is `***`, so a logged request never shows the password.
<!-- /memoria:section -->

<!-- memoria:section id="facts" files="facts.rs" -->
## Pre-flight facts

`facts.rs` converts the domain's `SourceFacts` to `SourceFactsWire` and back. `embedded_fonts` defaults to empty when a reply lacks it. The domain type keeps `DeclaredPages::source` as a `&'static str`, so the supervisor maps the wire string back to a static one. `ppt/presentation.xml` and `docProps/app.xml`, the sources that the scanners name, map to themselves. Any other source reads as `other`.
<!-- /memoria:section -->
