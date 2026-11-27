# Go client

`github.com/viktordanov/rs-convert/client` is the Go module for calling the convert service, and `client/convert` is its only public package.

Do these four things in every caller:

- Close `PDF.Body` after `ConvertToPDF`.
- Call `Bundle.Close` after `ConvertToBundle` or `ConvertToImages`. It deletes the temp directory of the bundle.
- Set a deadline on the context. The default HTTP client has no timeout of its own.
- Open the file again for a retry. The client reads the file once and never retries.

## Contents

1. [Install](#install)
2. [Convert a file](#convert-a-file)
3. [Methods and options](#methods-and-options)
4. [Errors and retries](#errors-and-retries)
5. [Uploads, results and bundles](#uploads-results-and-bundles)
6. [For contributors](#for-contributors)

<!-- memoria:section id="setup" files="go.mod" -->
## Install

The module needs Go 1.26 or later.

```bash
go get github.com/viktordanov/rs-convert/client
```

No version is tagged yet, so `go get` records a pseudo-version of the default branch. Releases get tags of the form `client/vX.Y.Z`. The module stays on v0 while the API can still change.
<!-- /memoria:section -->

<!-- memoria:section id="usage" files="convert/client.go convert/convert.go" -->
## Convert a file

1. Set `CONVERT_HOST` to the address of the service, for example `http://convert.internal:8080`.
2. Set `CONVERT_API_KEY` to the API key of the service.
3. Run the program that follows. It converts `deck.pptx` to `deck.pdf`.

```go
package main

import (
	"context"
	"io"
	"log"
	"os"

	"github.com/viktordanov/rs-convert/client/convert"
)

func main() {
	c, err := convert.New(os.Getenv("CONVERT_HOST"), os.Getenv("CONVERT_API_KEY"))
	if err != nil {
		log.Fatal(err)
	}

	in, err := os.Open("deck.pptx")
	if err != nil {
		log.Fatal(err)
	}
	defer in.Close()

	pdf, err := c.ConvertToPDF(context.Background(), "deck.pptx", in, convert.Options{})
	if err != nil {
		log.Fatal(err)
	}
	defer pdf.Body.Close()

	out, err := os.Create("deck.pdf")
	if err != nil {
		log.Fatal(err)
	}
	defer out.Close()

	if _, err := io.Copy(out, pdf.Body); err != nil {
		log.Fatal(err)
	}
	log.Printf("%d pages, request %s", pdf.Info.PageCount, pdf.Info.RequestID)
}
```

The program writes `deck.pdf` and logs the page count and the request ID, for example `12 pages, request 7f3c2a9e-4b1d-4f6a-9a51-0c2e8d7b6a14`.

Next: read [Errors and retries](#errors-and-retries) before you call the client from a queue handler.

## Methods and options

| Method | Returns |
| --- | --- |
| `New(host, apiKey, opts...)` | A `*Client`, safe for concurrent use. It returns an error when `host` or `apiKey` is empty |
| `ConvertToPDF` | A `*PDF`: the streamed `Body`, and `Info` from the response headers |
| `ConvertToBundle` | A `*Bundle`: the manifest, the PDF and the page images |
| `ConvertToImages` | A `*Bundle` with the manifest and the page images, and no PDF |
| `Capabilities` | The accepted formats, the limits, the engine versions and the fonts. The answer is fixed for a deployment |
| `Ready` | `true` when `/readyz` answers 200, `false` when it answers 503. Any other status is an error |

`New` takes three options. `WithHTTPClient` sets the HTTP client. `WithTempDir` sets the parent directory for unpacked bundles, and the default is the system temp directory. `WithMaxBundleBytes` caps the disk space that one bundle may take: the sum of the file sizes in its manifest. A bundle over the cap fails before any file is written. The default, 0, sets no cap, and a negative value makes `New` fail.

Each conversion takes a `convert.Options`. The zero value asks for the server defaults, because the client leaves every zero field out of the request:

- `Pages` selects the pages to render as images, for example `"1-5,8,10-"`. The PDF always holds every page.
- `Password` opens an encrypted input.
- `Timeout` is the server-side budget for the whole call. The client rounds it up to whole seconds.
- `RequestID` goes to the service as `X-Request-ID`. When it is empty, the service makes one.
- `Images`, `Slides` and `Sheets` hold the image settings and the options for presentations and spreadsheets. `go doc` lists their fields.
<!-- /memoria:section -->

<!-- memoria:section id="failure" files="convert/errors.go convert/convert.go" -->
## Errors and retries

Every error for a non-200 response wraps exactly one sentinel, so branch with `errors.Is`. The package has no error types. The message also carries the server's message, the status and the request ID, for example `failed to convert document: service busy: the conversion queue is full (status 429, request 7f3c2a9e)`.

The client picks the sentinel from the `code` in the error body. When the body has no known code, for example when a proxy answers, the status decides.

| Sentinel | Code | Status without a code | Retry |
| --- | --- | --- | --- |
| `ErrInvalidRequest` | `BAD_REQUEST`, `VALIDATION_ERROR` | 400, 404, 422 | No |
| `ErrUnauthorized` | `UNAUTHORIZED` | 401, 403 | No |
| `ErrFileTooLarge` | `FILE_TOO_LARGE` | 413 | No |
| `ErrUnsupportedFormat` | `UNSUPPORTED_FORMAT` | 415 | No |
| `ErrPasswordRequired` | `PASSWORD_REQUIRED` | | No. Send `Options.Password` |
| `ErrInvalidPassword` | `INVALID_PASSWORD` | | No |
| `ErrLimitExceeded` | `LIMIT_EXCEEDED` | | No |
| `ErrConversionFailed` | `CONVERSION_FAILED` | | No |
| `ErrTimeout` | `CONVERSION_TIMEOUT` | 504 | Once at most |
| `ErrBusy` | `TOO_MANY_REQUESTS` | 429 | Yes, after a delay |
| `ErrUnavailable` | `SERVICE_UNAVAILABLE` | 502, 503 | Yes, after a delay |
| `ErrInternal` | `INTERNAL_ERROR` | Every other status | Yes, with backoff |

`convert.Retryable(err)` is true for `ErrBusy`, `ErrUnavailable` and `ErrInternal`. It is false for `ErrTimeout`, for the final errors and for errors that wrap no sentinel. A queue handler can return a retryable error and let its own backoff retry the job.

A network error or a canceled context wraps no sentinel. It comes back as `failed to send request: …`, and the caller decides whether to retry it.

The service can answer before the upload ends, for example with 401, 413 or 415. It then reads and discards the rest of the upload, so the client gets the answer and not a write error. The service stops that after 256 MiB, after 10 s, or after 2 s without data. A larger or slower upload can still end in a write error such as `broken pipe`. That error wraps no sentinel, so `Retryable` is false for it.

The service sends `Retry-After` with 429 and 503, but the client does not expose that header. Use your own backoff for `ErrBusy` and `ErrUnavailable`.
<!-- /memoria:section -->

<!-- memoria:section id="contract" files="convert/convert.go convert/bundle.go convert/serialization.go" -->
## Uploads, results and bundles

The upload is streamed. The client writes the multipart body through a pipe, so it never holds the file in memory. It sends the `options` part before the `file` part, so that the service can reject a bad request before the upload ends.

`ConvertToPDF` streams the PDF too. `PDF.Body` is the response body, so you can copy it straight to storage.

A bundle needs a temp directory. The service sends `images` and `bundle` as a tar archive in one zstd stream (`application/zstd`, named `<stem>.tar.zst`). The archive can only be read front to back, so `ConvertToBundle` and `ConvertToImages` unpack the whole response into a temp directory before they return. `Bundle.Close` deletes that directory. `Bundle.PDF()` and `Bundle.Page(n)` return `(reader, found, err)`. `found` is false when the bundle has no PDF or the page was not rendered, and that is not an error. Close each reader that you open.

The client accepts only the layout that the service writes. `manifest.json` comes first, then `document.pdf` for a bundle, then `pages/NNNN.<ext>`. Each entry is a regular file that the manifest names, each comes once, and each has the size that the manifest gives. A path with `..`, a link, an extra or a missing file, a wrong size, a page path that the manifest gives twice, or a manifest that is not first fails the call. The client compares the size in each entry's tar header with the manifest before it writes the entry, so an entry larger than the manifest says never reaches the disk. The zstd decoder accepts a window of at most 8 MiB, and the client reads at most 1 MiB of a capabilities answer. After the tar end, the client accepts only zero padding, at most one 10240-byte tar record, and then requires the end of the zstd stream. Thus a cut stream, a bad checksum or other data after the archive fails the call. A failed call leaves no temp directory.

```go
bundle, err := c.ConvertToImages(ctx, name, file, convert.Options{
	Pages:  "1-5",
	Images: convert.ImageOptions{Format: convert.JPEG, DPI: 72, MaxEdge: 320},
})
if err != nil {
	return err
}
defer bundle.Close()

for _, page := range bundle.Manifest.Pages {
	image, found, err := bundle.Page(page.Number)
	if err != nil {
		return err
	}
	if found {
		// store image under page.Path, then close it
		image.Close()
	}
}
```

Both result types carry `Info` from the response headers. `Info` holds the request ID, the source format, the page count, the warning count and the engine fingerprint. The fingerprint changes when the output can change, so put it in cache keys.

`Manifest.Warnings` holds the quality signals of a bundle, such as `FONT_SUBSTITUTED`. A warning never fails a conversion. `GLYPHS_MISSING` means that some characters of the text have no font in the image. They render as boxes or not at all. Its `Detail` holds `missing` (a number), `code_points` (a `[]any` of strings such as `"U+0E01"`) and `scripts` (a `map[string]any` of counts). The `WarningCode` schema in the spec lists every code.

With `Slides.IncludeHidden`, the PDF and the pages also hold the slides that are marked hidden, in slide order. `Page.Hidden` is true for each page of such a slide. The service sets it only when the slide list predicts the page count exactly. A legacy `.ppt` never sets it.

The client ignores manifest fields that it does not know, and it keeps every enum value as a plain string. Thus a newer server with the same API version does not break an older client.
<!-- /memoria:section -->

<!-- memoria:section id="contributing" files="oapi-codegen.yaml convert/api_surface_test.go contract/go.mod contract/harness_test.go contract/contract_test.go contract/levelb_test.go contract/cmd/speccheck/main.go" -->
## For contributors

`convert` wraps the generated client in `internal/api/client.go`. oapi-codegen generates that file from `../api/openapi.json` with the settings in `oapi-codegen.yaml`. `convertd openapi` emits `openapi.json` from the server code, and the committed file is the contract. Go's internal rule lets only code under `client/` import the generated package. No exported identifier in `convert` uses a generated type, and `convert/api_surface_test.go` fails when one does.

When you change the API in the server code:

1. Run `../generate.sh`. It emits and validates `openapi.json`, regenerates the client and runs `go vet` and `go test -race`.
2. Commit `api/openapi.json` and `internal/api/client.go` together.
3. Commit a facade change in a separate commit.
4. Run `../generate.sh --check`. It fails when a generated file differs from what generation produces or from `HEAD`. It also fails on an untracked file under `api/` or `client/`. CI runs it in the `test` job.

`contract/` is a nested module that is never published or tagged. It holds `speccheck`, the validating transport that checks each response against the spec, the test harness, the live contract cases and the self-tests. `speccheck validate` checks the spec: its structure and examples, no null types, no integer arrays, the `pages` pattern and the error codes named in its description.

`../scripts/contract_test.sh` builds convertd with the `test-hooks` feature and runs the contract tests against the level A fake. The timeout and queue cases start their own level B servers (`TEST_ENGINE=worker`), so their 504 and 429 come from the real pool. In image mode these two cases skip, because they need a server of their own. CI runs it in the `test` job. `../scripts/contract_test.sh --image REF` runs them against an image. There, the cases that need a fake engine or a server of their own skip. `cd contract && go test ./...` runs only the spec tools and the self-tests, and it needs no server.

The script prints the skipped cases with their reasons and the declared `(operationId, status)` pairs that no case observed.

The live cases prove that the service, the spec and this client agree on the responses that the cases exercise. They do not prove the quality of a conversion. In binary mode, both test levels replace the engines. In image mode, the engines are real, but no case compares the output with a reference.
<!-- /memoria:section -->
