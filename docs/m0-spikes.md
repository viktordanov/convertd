# M0 spike results

Run on 25 November 2026 on an Apple-silicon laptop. Linux results come from
Docker Desktop (linux/arm64, `--cpus 2 --memory 2g` unless stated). The corpus
is synthetic (`spikes/corpus/make_corpus.py`), so absolute numbers are a
lower bound for real documents. Use them for the decisions below, not as
performance targets.

| # | Question | Result |
| --- | --- | --- |
| 1 | Does the Go facade build against the real generated client? | **Go** |
| 2 | Can the Rust server emit the spec that oapi-codegen consumes? | **Go, with three rules** |
| 3 | Does LibreOfficeKit from Rust work on LibreOffice 26.8? | **Go** |
| 4 | PDFium throughput per image format | **Go**; the plan's encoding assumptions change |
| 5 | Cold `soffice` against warm LibreOfficeKit | Warm is 3 to 6 times faster on small files |
| - | Kernel sandbox on the target platform | **Still unknown.** Cannot be tested here |

## 1. Go client against the generated code

- oapi-codegen v2.8.2 and v2.9.0 both generate from `api/openapi.yaml`.
- The facade in `client/convert/` builds and vets with no changes, and passes
  all 42 tests under `-race`.
- The generated code now lives in `client/internal/api/`, so other modules
  cannot import it. No exported identifier in `convert/` uses a generated type.
- The module path is `github.com/viktordanov/rs-convert/client`.

## 2. Code-first spec: axum + utoipa → oapi-codegen

Spike: `spikes/openapi-codefirst/`. A stub axum server with the real DTOs
emits the spec, and oapi-codegen generates Go from it.

What works:

- utoipa 6.0.0 with utoipa-axum 0.3.0 and axum 0.8.9.
- The multipart body with per-part `encoding` (`file` as octet-stream,
  `options` as JSON).
- `#[serde(deny_unknown_fields)]` becomes `additionalProperties: false`.
- Schemas that no path references, such as `Manifest`, are kept through
  `components(schemas(...))` together with `skip-prune` in the generator config.
  The generated `Manifest`, `PageInfo`, `PdfInfo` and `Warning` look right.
  `Warning.detail` becomes `*map[string]any`.
- Several content types and response headers on one response
  (`application/pdf` and `application/zip`).
- The global bearer scheme, with `security(())` opening the probes.
- oapi-codegen v2.9.0 generates Go that passes `go vet`.

Rules the server code must follow:

1. **utoipa 6 emits OpenAPI 3.1.0 only.** It has no 3.0 output. Use
   oapi-codegen v2.9.0 or later. `generate.sh` is pinned to it.
2. **Every `Option<T>` field needs `#[schema(nullable = false)]`.** Without it,
   utoipa emits `type: [T, "null"]`, and the Go fields lose `omitempty`, so
   the client sends explicit `null`s. A test should fail on any `"null"` type
   in the emitted spec.
3. **Binary fields need `#[schema(value_type = String, format = Binary)]`.**
   A plain `Vec<u8>` becomes an array of integers, and Go gets `[]int32`
   instead of `openapi_types.File`.

One difference to expect: `u32` fields come out as `int32` and `u64` as
`int64`, where the hand-written spec gives plain `int`. The facade converts
numbers explicitly, but a few conversions in `client/convert/` will need to
change.

Not yet proven: the full DTO set. When the real server exists, `generate.sh`
must emit the spec from `convertd openapi`, regenerate the client and build
the facade. CI then catches any drift. The hand-written `api/openapi.yaml`
stays as the reference until then.

## 3. LibreOfficeKit 26.8.3 from Rust

Spike: `spikes/lok/`. `libreofficekit` 0.5.0 crate, TDF 26.8.3 `.deb` set on
`debian:trixie-slim`.

| Check | Result |
| --- | --- |
| Load and PDF export | Works for DOCX, PPTX, XLSX |
| Per-worker profile | The crate calls `libreofficekit_hook` without a profile argument, but the `UserInstallation=file:///…` environment variable works. No custom FFI needed |
| Export options | JSON filter options work in `save_as` (notes pages, hidden slides) |
| Password callback | No password: fails after 1 callback. Wrong password: fails after 2. Right password: converts. The callback count separates `PASSWORD_REQUIRED` from `INVALID_PASSWORD` |
| Second instance in one process | Refused by the crate (`already another active instance`) |
| SIGKILL mid-conversion | The worker is one process with no child processes. Killing its process group leaves nothing behind. A fresh worker starts on the same profile |
| Memory | About 96 MB after init and 240 to 290 MB warm. It stayed flat over 300 conversions of simple files |
| `trim_memory` | **Unusable.** A trim followed by the next document load crashes the worker with SIGBUS every time, and the trim freed no memory. A trim followed by dropping `Office` also crashes. Without a trim, dropping `Office` exits cleanly (3 of 3 runs). Workers must never call it; recycling is the only way to reclaim memory |
| Shutdown | Workers should still leave with `std::mem::forget` and `_exit`, so no LibreOffice teardown code runs |

Timings, warm worker (ms, third round):

| Document | Load | Export | Total | Cold `soffice --convert-to` |
| --- | --- | --- | --- | --- |
| small.docx (1 page) | 22 | 3 | 25 | 154 |
| long.docx (35 pages) | 40 | 27 | 67 | 202 |
| sheet.xlsx (3 sheets) | 18 | 25 | 43 | 169 |
| deck.pptx (20 slides) | 94 | 490 | 584 | 728 |

Init itself takes about 60 ms. The first document then costs about 90 ms
more than later ones (filter and font loading). So a worker is warm after
about 150 ms plus one small conversion.

**What this means for the "single-use workers" proposal:** a spare worker
costs about 150 ms of CPU and about 100 to 200 MB of memory per conversion,
paid off the request path. Recycling after N jobs costs less, and no leak
showed up. Measure again on the real corpus before choosing.

A deck of 10 full-slide photos (5.9 MB PDF) takes 797 ms.

The Impress PDF export dominates for decks (about 25 ms per slide).
Profile it before tuning anything else.

## 4. PDFium rendering and encoding

Spike: `spikes/pdfium/`. `pdfium-render` 0.9.6 with bblanchon
`chromium/8179`, one process, one thread, one CPU.

- Binding the library takes about 1 ms and opening a PDF is close to 0,
  measured inside a process that was already running. The cost of starting
  a process (spawn and exec) was not measured, so these numbers alone do
  not show that a process per request is cheap.
- Rendering takes 1 to 8 ms per page. **Encoding dominates.**

Per page, ms (KB). Timed in microseconds, averaged per page. `photos` is
10 slides that are each one full-bleed noisy gradient image, the hard case
for lossless formats.

| Format | 150 DPI text | 150 DPI slide | 150 DPI photo | 300 DPI text | 300 DPI photo |
| --- | --- | --- | --- | --- | --- |
| Render only | 2.4 | 2.6 | 16.9 | 8.9 | 24.3 |
| JPEG q85, `image` | 12.7 (397) | 8.0 (71) | 19.6 (794) | 45.6 (1068) | 62.5 (2036) |
| JPEG q85, `jpeg-encoder` | 7.5 (378) | 4.8 (51) | 10.6 (610) | 27.9 (979) | 34.9 (1575) |
| PNG, fast compression | 3.3 (783) | 1.3 (58) | 9.2 (3920) | 11.1 (1867) | 41.0 (11516) |
| WebP q85, libwebp | 95.3 (219) | 48.5 (13) | 176.3 (522) | 285.5 (484) | 553.5 (1095) |
| WebP lossless, `image` | 4.6 (281) | 3.0 (15) | 11.9 (3930) | 19.6 (679) | 56.6 (11823) |

What this changes:

- **Keep JPEG as the default, for size rather than speed.** On photos, PNG
  and lossless WebP are 5 to 7 times larger than JPEG. On text pages they
  are fast and compact.
- **PNG with fast compression is not 20 times slower than JPEG** on these
  inputs. It is about as fast or faster. The plan's figure came from default
  PNG compression.
- **Lossy WebP is the slow format**, 10 to 16 times slower than
  `jpeg-encoder`. Keep it but document the cost, or cap its DPI.
- **Use `jpeg-encoder`** rather than `image`'s JPEG encoder. It is about 40%
  faster and produces smaller files.
- Recheck on the real corpus. This one is synthetic.

## 5. Baseline

The cold-against-warm numbers above come from this machine, not from the
existing production pipeline, which is not available here. They are a
stand-in for the baseline. M0 still needs the real baseline on real documents.

## Still unknown

- **Kernel sandbox (seccomp and Landlock) on the deployment platform.**
  Docker Desktop's kernel says nothing about the target platform. Test on the
  target with `SANDBOX=auto` and read the start-up log.
- **x86_64.** The same `.deb` set and PDFium build exist for x86_64 but have
  not been run.
- **Real documents.** Fidelity, and leaks on complex files.
- **Macros.** The corpus has no macro document yet, so `MACROS_IGNORED` and
  macro blocking are untested.
- **Other encryption.** The password test covers OOXML (agile) encryption
  only. Legacy `.doc`, `.xls` and ODF encryption are untested.

## Image notes

- The full TDF `.deb` set will not install on its own: `postgresql-sdbc`
  needs LibreOffice Base. Install an allow-list instead: `ure`, the main
  package, `core`, `writer`, `calc`, `impress`, `draw`, `graphicfilter`,
  `images`, `ooofonts`, `en-us` and `libreofficekit-data`.
- Spike image size: 708 MB with LibreOffice, fonts and the spike binary.
- Pinned: LibreOffice 26.8.3 aarch64 tarball SHA-256
  `378a0766e4a815a5a96fb7473f346d072e23e3cc28606bdc8cd6f834cfaa14db`; PDFium
  `chromium/8179` linux-arm64 SHA-256
  `18bb6d568257b28d14f6c3b8bfb48caf73d4615b319ca4d43b424c2a03431d9f`.
