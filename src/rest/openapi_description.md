Converts office documents, PDFs and images into a standard PDF and per-page
images.

One call does the whole job. Upload a file, say what you want back, and
receive either the PDF itself or a ZIP bundle holding the PDF, the page
images and a `manifest.json` that describes them.

## Authentication

Every `/v1` route needs the service API key as a bearer token:

```
Authorization: Bearer <API_KEY>
```

The probes (`/healthz`, `/readyz`) take no key.

## Calling it

```bash
# Office document to PDF (the default output)
curl -sS -H "Authorization: Bearer $CONVERT_API_KEY" \
  -F file=@deck.pptx \
  -o deck.pdf \
  "$CONVERT_HOST/v1/convert"

# PDF plus page images at 150 DPI, as one ZIP bundle
curl -sS -H "Authorization: Bearer $CONVERT_API_KEY" \
  -F file=@deck.pptx \
  -F 'options={"output":"bundle","images":{"format":"jpeg","dpi":150}};type=application/json' \
  -o deck.zip \
  "$CONVERT_HOST/v1/convert"

# First page of an existing PDF as a small thumbnail
curl -sS -H "Authorization: Bearer $CONVERT_API_KEY" \
  -F file=@report.pdf \
  -F 'options={"output":"images","pages":"1","images":{"format":"webp","dpi":72,"max_edge":320}};type=application/json' \
  -o thumb.zip \
  "$CONVERT_HOST/v1/convert"
```

## What comes back

The `output` option decides the response. Each value maps to exactly one
content type:

| `output` | Response | Contents |
| --- | --- | --- |
| `pdf` (default) | `application/pdf` | The PDF. Metadata travels in `Convert-*` headers. |
| `images` | `application/zip` | `manifest.json` and `pages/NNNN.<ext>` |
| `bundle` | `application/zip` | `manifest.json`, `document.pdf` and `pages/NNNN.<ext>` |

The response is only sent once the whole job has succeeded, so a `200`
always carries a complete result and every failure has a proper status
code and JSON error body.

## Errors and retries

Every error body is `{"error": "<message>", "code": "<CODE>"}`.

| Status | Codes | Retry? |
| --- | --- | --- |
| 400 | `BAD_REQUEST` | No. The request is malformed. |
| 401 | `UNAUTHORIZED` | No. |
| 413 | `FILE_TOO_LARGE` | No. |
| 415 | `UNSUPPORTED_FORMAT` | No. |
| 422 | `VALIDATION_ERROR`, `PASSWORD_REQUIRED`, `INVALID_PASSWORD`, `LIMIT_EXCEEDED`, `CONVERSION_FAILED` | No. The same input will fail the same way. |
| 429 | `TOO_MANY_REQUESTS` | Yes, after `Retry-After` seconds. |
| 500 | `INTERNAL_ERROR` | Yes, with backoff. |
| 503 | `SERVICE_UNAVAILABLE` | Yes, after `Retry-After` seconds. |
| 504 | `CONVERSION_TIMEOUT` | Once at most. A second timeout means the document is too heavy for the budget. |

The service already retries a crashed engine once on a fresh worker, so
`CONVERSION_FAILED` is final.

## Determinism and caching

The service keeps no state and stores nothing after the response is sent.
The same file, options and engine build produce the same output, so callers
can cache results under `sha256(file) + options + Convert-Engine-Fingerprint`.
The fingerprint changes whenever output could change: a new service
version, LibreOffice version, PDFium version or font set.
