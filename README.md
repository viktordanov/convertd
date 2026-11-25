# rs-convert

rs-convert is an HTTP service that converts office documents, PDFs and images into a PDF and per-page images.

**Status: the service converts office documents and PDFs. It does not convert image inputs yet, and no container image is published.** `convertd serve` runs LibreOffice and PDFium in pools of worker processes. With the real engines, an image input (PNG, JPEG, TIFF, GIF, BMP or WebP) gets `422 CONVERSION_FAILED`. To run the real engines, build the `runtime` image from `docker/Dockerfile`. The quick start runs `convertd` from source with the level-A fake engine instead. The API is a draft (`1.0.0-draft.1`) and can change.

## Contents

1. [The problem](#the-problem)
2. [What it does not do](#what-it-does-not-do)
3. [Quick start](#quick-start)
4. [The API](#the-api)
5. [Boundaries](#boundaries)
6. [Documentation](#documentation)
7. [License](#license)

## The problem

A service that converts office files with `soffice --convert-to` starts a new LibreOffice process for each file. In the M0 spikes, a warm LibreOfficeKit instance converted small files 3 to 6 times faster than a cold `soffice` process. That measurement used an Apple-silicon laptop and a synthetic corpus ([docs/m0-spikes.md](docs/m0-spikes.md)).

LibreOffice and PDFium also parse files from callers that the service does not control. A file that hangs or crashes a parser must not stop the service.

rs-convert keeps LibreOffice loaded in long-lived worker processes through LibreOfficeKit, and it renders pages with PDFium. Each engine runs in a worker process that the supervisor can kill at a deadline and replace. The service is one Rust binary, `convertd`, in one image. It has no database and no job store.

## What it does not do

The service has a narrow scope:

- It does no OCR, text extraction or layout analysis.
- It does not convert HTML, web pages or email (`.eml`, `.msg`).
- It does not guarantee that its output matches Microsoft Office. LibreOffice lays out each document, and the layout can differ from Word or PowerPoint.
- It has no asynchronous jobs, webhooks or result storage. Each call is synchronous, and the caller owns retries.
- It does not edit PDFs: no merge, split, signature or PDF/A.

<!-- memoria:section id="setup" files="Cargo.toml rust-toolchain.toml" -->
## Quick start

This quick start builds `convertd` from source and runs it with the level-A fake engine, so it needs no LibreOffice or PDFium. To run the real engines, build the `runtime` target of `docker/Dockerfile`. [docker/README.md](docker/README.md) gives the targets and the build commands.

You need a Linux or macOS host with rustup. `rust-toolchain.toml` pins Rust 1.99.0, and rustup installs it on the first build. The test command uses [cargo-nextest](https://nexte.st).

1. Build `convertd` with the test hooks:

   ```bash
   cargo build --features test-hooks
   ```

2. Run the tests:

   ```bash
   cargo nextest run --features test-hooks
   ```

3. Start the server with the fake engine. `HOST=127.0.0.1` keeps it on the loopback interface, and it listens on port 8080:

   ```bash
   API_KEY=dev-key HOST=127.0.0.1 TEST_ENGINE=fake target/debug/convertd serve
   ```

4. In a second terminal, send a PDF of your own:

   ```bash
   curl -sS -D - -o out.pdf -H "Authorization: Bearer dev-key" -F file=@report.pdf http://127.0.0.1:8080/v1/convert
   ```

The fake engine returns a PDF unchanged and turns an office file into a one-page placeholder PDF. Detection, validation and the response headers come from the real request path. This response leaves out the `content-length` and `date` lines:

```text
HTTP/1.1 200 OK
content-type: application/pdf
content-disposition: attachment; filename="report.pdf"; filename*=UTF-8''report.pdf
convert-source-format: pdf
convert-page-count: 1
convert-warning-count: 0
convert-engine-fingerprint: sha256:62666a78c776
server-timing: recv;dur=0.978, detect;dur=0.006, queue;dur=0.000, lease;dur=0.000, spawn;dur=0.000, bind;dur=0.000, inspect;dur=0.134, inspect_engine;dur=0.000, probe_engine;dur=0.000, verify;dur=0.000, package;dur=0.012, total;dur=0.165
x-request-id: df997c7b55784260b1bbdf216c48ba0d
```

`TEST_ENGINE` works only in a build with the `test-hooks` feature, and the image never has it. Next: read [tests/README.md](tests/README.md) for the other test commands.
<!-- /memoria:section -->

<!-- memoria:section id="usage" files="api/openapi.yaml api/openapi.json" -->
## The API

One endpoint does the work. `POST /v1/convert` takes a multipart upload with a `file` part and an optional `options` part in JSON. The service detects the format from the bytes, never from the file name.

The `output` option selects the response: the PDF (`pdf`, the default), the page images (`images`), or the PDF and the page images (`bundle`). The service sends `images` and `bundle` as a tar archive in one zstd frame: `application/zstd`, named `<stem>.tar.zst`. The archive starts with a `manifest.json` that describes the pages and the warnings. In a bundle, `document.pdf` comes next. The pages follow as `pages/NNNN.<ext>`.

This call returns the PDF and JPEG page images at 150 DPI in one archive:

```bash
curl -sS -H "Authorization: Bearer $CONVERT_API_KEY" \
  -F file=@deck.pptx \
  -F 'options={"output":"bundle","images":{"format":"jpeg","dpi":150}};type=application/json' \
  -o deck.tar.zst \
  "$CONVERT_HOST/v1/convert"
```

`zstd -dc deck.tar.zst | tar -tf -` lists the entries. Legacy binary Office files (`.doc`, `.xls`, `.ppt`, `.pps`) go to LibreOffice, as OOXML files do. An encrypted file without the `password` option gets `422 PASSWORD_REQUIRED`.

Every `/v1` route needs the API key as a bearer token. The probes (`/healthz`, `/readyz`) and `/metrics` take no key. `GET /v1/capabilities` lists the formats, the limits and the engine versions of a deployment.

`convertd openapi` emits the OpenAPI 3.1 spec. The repository holds its output as [api/openapi.json](api/openapi.json), and the server serves the same bytes at `/swagger/doc.json`. `generate.sh` generates the Go client from this file. The hand-written [api/openapi.yaml](api/openapi.yaml), in OpenAPI 3.0.3, stays as the reference contract. `speccheck compare`, in `client/contract/`, compares the two.
<!-- /memoria:section -->

## Boundaries

Each folder in this list has a README for its own readers.

### Using the service

- [client/README.md](client/README.md) covers the Go client: a hand-written facade over a client generated from the spec.

### Deploying it

- [docker/README.md](docker/README.md) covers the image: the pinned LibreOffice, PDFium and fonts, and the build targets.
- [deploy/README.md](deploy/README.md) covers the manifests for Kubernetes and Azure Container Apps.
- `build_docker.sh` builds the image for linux/amd64 and linux/arm64 and pushes it to a repository that you name.

<!-- memoria:section id="contributing" files="generate.sh scripts/contract_test.sh" -->
### Contributing to it

- [src/README.md](src/README.md) covers the `convertd` crate: its process model and its layers.
- [tests/README.md](tests/README.md) covers the test levels, the two fakes, the nextest profiles and the commands that CI runs.
- [corpus/README.md](corpus/README.md) covers the generated test corpus and the expected result for each file.
- [tools/corpus/README.md](tools/corpus/README.md) covers `corpus-bench`, the tool that checks the corpus and its expectations.
- `generate.sh` emits the spec with `convertd openapi`, regenerates the Go client from it and tests the client. `scripts/contract_test.sh` runs the contract tests against a real `convertd`.
- Every push and pull request runs `memoria check` (`.github/workflows/memoria.yml`). The check fails while a README waits for review after a change to its sources. [Memoria in this repository](docs/documentation/memoria.md) gives the review steps.
<!-- /memoria:section -->

## Documentation

[docs/README.md](docs/README.md) maps the other documents: the measured spike results, the design plan, the writing guidance and the ledger of work.

## License

rs-convert uses the Apache-2.0 license (`LICENSE`). The image also bundles LibreOffice (MPL-2.0), PDFium (BSD-style) and fonts under their own licenses.
