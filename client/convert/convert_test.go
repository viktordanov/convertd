package convert_test

import (
	"archive/tar"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/klauspost/compress/zstd"

	"github.com/viktordanov/rs-convert/client/convert"
)

const apiKey = "test-key"

// received is what the fake service saw in one convert request.
type received struct {
	authorization string
	requestID     string
	partOrder     []string
	options       map[string]any
	filename      string
	file          []byte
}

// newFakeService starts a stand-in for the Convert API. respond writes the
// response to each convert request. It returns a client for it, the last
// request received, and the temp dir the client keeps bundles in.
func newFakeService(t *testing.T, respond http.HandlerFunc) (*convert.Client, *received, string) {
	t.Helper()
	got := &received{}
	tempDir := t.TempDir()

	mux := http.NewServeMux()
	mux.HandleFunc("POST /v1/convert", func(w http.ResponseWriter, r *http.Request) {
		got.authorization = r.Header.Get("Authorization")
		got.requestID = r.Header.Get("X-Request-ID")

		parts, err := r.MultipartReader()
		if err != nil {
			t.Errorf("request is not multipart: %v", err)
			return
		}
		for {
			part, err := parts.NextPart()
			if errors.Is(err, io.EOF) {
				break
			}
			if err != nil {
				t.Errorf("failed to read part: %v", err)
				return
			}
			data, _ := io.ReadAll(part)
			got.partOrder = append(got.partOrder, part.FormName())
			switch part.FormName() {
			case "options":
				if ct := part.Header.Get("Content-Type"); ct != "application/json" {
					t.Errorf("options part has content type %q", ct)
				}
				if err := json.Unmarshal(data, &got.options); err != nil {
					t.Errorf("options part is not JSON: %v", err)
				}
			case "file":
				got.filename = part.FileName()
				got.file = data
			}
		}
		respond(w, r)
	})

	server := httptest.NewServer(mux)
	t.Cleanup(server.Close)

	c, err := convert.New(server.URL, apiKey, convert.WithTempDir(tempDir))
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	return c, got, tempDir
}

func writeError(w http.ResponseWriter, status int, code, message string) {
	w.Header().Set("Content-Type", "application/json")
	w.Header().Set("X-Request-ID", "req-1")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(map[string]string{"error": message, "code": code})
}

// testManifest is a manifest as the service writes it. withPDF false gives
// the shape of output=images.
func testManifest(withPDF bool) map[string]any {
	manifest := map[string]any{
		"manifest_version": 1,
		"request_id":       "req-1",
		"source": map[string]any{
			"filename": "deck.pptx", "format": "pptx", "family": "presentation",
			"size_bytes": 11, "sha256": strings.Repeat("ab", 32), "encrypted": false,
		},
		"page_count": 2,
		"images":     map[string]any{"format": "jpeg", "dpi": 150, "quality": 85},
		"pages": []map[string]any{
			{"number": 1, "path": "pages/0001.jpg", "width_px": 2000, "height_px": 1125,
				"width_pt": 960, "height_pt": 540, "size_bytes": 6, "blank": false, "hidden": true},
		},
		"warnings": []map[string]any{
			{"code": "FONT_SUBSTITUTED", "message": "Aptos was replaced by Carlito",
				"detail": map[string]any{"requested": "Aptos", "substitute": "Carlito"}},
			{"code": "BLANK_PAGE", "message": "page 2 is blank", "page": 2},
			{"code": "GLYPHS_MISSING", "message": "2 characters have no font in this image: Thai 2",
				"detail": map[string]any{"missing": 2, "code_points": []string{"U+0E01", "U+0E32"},
					"scripts": map[string]any{"Thai": 2}}},
		},
		"engine": map[string]any{
			"service_version": "1.0.0", "libreoffice": "26.8.3.2", "pdfium": "8125",
			"fingerprint": "sha256:abc",
		},
		"timings_ms": map[string]any{"queue": 3, "convert": 812, "render": 240, "total": 1071},
	}
	if withPDF {
		manifest["pdf"] = map[string]any{"path": "document.pdf", "size_bytes": 8, "origin": "converted"}
	}
	return manifest
}

// entry is one file of a test archive. typ 0 means a regular file.
type entry struct {
	name string
	data []byte
	typ  byte
}

func manifestEntry(t *testing.T, manifest map[string]any) entry {
	t.Helper()
	raw, err := json.Marshal(manifest)
	if err != nil {
		t.Fatalf("encode manifest: %v", err)
	}
	return entry{name: "manifest.json", data: raw}
}

// tarZst builds a tar archive in one zstd frame, as the service streams it.
func tarZst(t *testing.T, entries ...entry) []byte {
	t.Helper()
	return tarZstWithTail(t, nil, entries...)
}

// tarZstWithTail is tarZst with tail written after the tar end-of-archive
// marker, inside the same zstd frame.
func tarZstWithTail(t *testing.T, tail []byte, entries ...entry) []byte {
	t.Helper()
	var buf bytes.Buffer
	zw, err := zstd.NewWriter(&buf, zstd.WithEncoderLevel(zstd.SpeedFastest))
	if err != nil {
		t.Fatalf("zstd writer: %v", err)
	}
	tw := tar.NewWriter(zw)
	for _, e := range entries {
		hdr := &tar.Header{Name: e.name, Mode: 0o644, Size: int64(len(e.data)), Typeflag: tar.TypeReg, Format: tar.FormatUSTAR}
		if e.typ != 0 {
			hdr.Typeflag, hdr.Size, hdr.Linkname = e.typ, 0, "/etc/passwd"
		}
		if err := tw.WriteHeader(hdr); err != nil {
			t.Fatalf("tar header %s: %v", e.name, err)
		}
		if e.typ == 0 {
			if _, err := tw.Write(e.data); err != nil {
				t.Fatalf("tar write %s: %v", e.name, err)
			}
		}
	}
	if err := tw.Close(); err != nil {
		t.Fatalf("tar close: %v", err)
	}
	if _, err := zw.Write(tail); err != nil {
		t.Fatalf("zstd write tail: %v", err)
	}
	if err := zw.Close(); err != nil {
		t.Fatalf("zstd close: %v", err)
	}
	return buf.Bytes()
}

// bundleEntries are the entries of a valid bundle, as bundleArchive writes them.
func bundleEntries(t *testing.T) []entry {
	t.Helper()
	return []entry{
		manifestEntry(t, testManifest(true)),
		{name: "document.pdf", data: []byte("%PDF-1.7")},
		{name: "pages/0001.jpg", data: []byte("page-1")},
	}
}

// bundleArchive builds a valid bundle (or images output when withPDF is
// false) as the service would.
func bundleArchive(t *testing.T, withPDF bool) []byte {
	t.Helper()
	entries := []entry{manifestEntry(t, testManifest(withPDF))}
	if withPDF {
		entries = append(entries, entry{name: "document.pdf", data: []byte("%PDF-1.7")})
	}
	entries = append(entries, entry{name: "pages/0001.jpg", data: []byte("page-1")})
	return tarZst(t, entries...)
}

func respondWith(body []byte) http.HandlerFunc {
	return func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "application/zstd")
		w.Header().Set("X-Request-ID", "req-1")
		w.Header().Set("Convert-Page-Count", "2")
		w.Header().Set("Convert-Warning-Count", "2")
		_, _ = w.Write(body)
	}
}

func respondWithBundle(t *testing.T, withPDF bool) http.HandlerFunc {
	return respondWith(bundleArchive(t, withPDF))
}

func readAll(t *testing.T, r io.ReadCloser) string {
	t.Helper()
	defer r.Close()
	data, err := io.ReadAll(r)
	if err != nil {
		t.Fatalf("read: %v", err)
	}
	return string(data)
}

func TestConvertToPDF(t *testing.T) {
	c, got, _ := newFakeService(t, func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "application/pdf")
		w.Header().Set("X-Request-ID", "req-1")
		w.Header().Set("Convert-Source-Format", "docx")
		w.Header().Set("Convert-Page-Count", "12")
		w.Header().Set("Convert-Warning-Count", "1")
		w.Header().Set("Convert-Engine-Fingerprint", "sha256:abc")
		_, _ = w.Write([]byte("%PDF-1.7 body"))
	})

	pdf, err := c.ConvertToPDF(context.Background(), "report.docx", strings.NewReader("docx bytes"), convert.Options{})
	if err != nil {
		t.Fatalf("ConvertToPDF: %v", err)
	}

	t.Run("streams the PDF body", func(t *testing.T) {
		if body := readAll(t, pdf.Body); body != "%PDF-1.7 body" {
			t.Errorf("body = %q", body)
		}
	})

	t.Run("reads the metadata headers", func(t *testing.T) {
		want := convert.Info{RequestID: "req-1", SourceFormat: "docx", PageCount: 12, WarningCount: 1, Fingerprint: "sha256:abc"}
		if pdf.Info != want {
			t.Errorf("info = %+v, want %+v", pdf.Info, want)
		}
	})

	t.Run("sends the key, the file and the options", func(t *testing.T) {
		if got.authorization != "Bearer "+apiKey {
			t.Errorf("authorization = %q", got.authorization)
		}
		if got.filename != "report.docx" || string(got.file) != "docx bytes" {
			t.Errorf("file = %q %q", got.filename, got.file)
		}
		if strings.Join(got.partOrder, ",") != "options,file" {
			t.Errorf("part order = %v, want options before file", got.partOrder)
		}
	})

	t.Run("zero options send only the output", func(t *testing.T) {
		if len(got.options) != 1 || got.options["output"] != "pdf" {
			t.Errorf("options = %v", got.options)
		}
	})
}

func TestConvertToBundle(t *testing.T) {
	c, got, tempDir := newFakeService(t, respondWithBundle(t, true))

	bundle, err := c.ConvertToBundle(context.Background(), "deck.pptx", strings.NewReader("pptx bytes"), convert.Options{})
	if err != nil {
		t.Fatalf("ConvertToBundle: %v", err)
	}

	t.Run("asks for the bundle output", func(t *testing.T) {
		if got.options["output"] != "bundle" {
			t.Errorf("output = %v", got.options["output"])
		}
	})

	t.Run("reads the manifest", func(t *testing.T) {
		m := bundle.Manifest
		if m.RequestID != "req-1" || m.PageCount != 2 {
			t.Errorf("manifest = %+v", m)
		}
		if m.Source.Format != "pptx" || m.Source.Family != "presentation" || m.Source.Filename != "deck.pptx" || m.Source.SizeBytes != 11 {
			t.Errorf("source = %+v", m.Source)
		}
		if m.PDF == nil || m.PDF.Path != "document.pdf" || m.PDF.Origin != "converted" {
			t.Errorf("pdf = %+v", m.PDF)
		}
		if len(m.Pages) != 1 || m.Pages[0].Number != 1 || m.Pages[0].WidthPx != 2000 || m.Pages[0].WidthPt != 960 || !m.Pages[0].Hidden {
			t.Errorf("pages = %+v", m.Pages)
		}
		if m.Engine.LibreOffice != "26.8.3.2" || m.Engine.PDFium != "8125" || m.Engine.Fingerprint != "sha256:abc" {
			t.Errorf("engine = %+v", m.Engine)
		}
		if m.Timings.Convert != 812*time.Millisecond || m.Timings.Total != 1071*time.Millisecond {
			t.Errorf("timings = %+v", m.Timings)
		}
	})

	t.Run("reads the warnings", func(t *testing.T) {
		w := bundle.Manifest.Warnings
		if len(w) != 3 {
			t.Fatalf("warnings = %+v", w)
		}
		if w[0].Code != "FONT_SUBSTITUTED" || w[0].Page != 0 || w[0].Detail["requested"] != "Aptos" {
			t.Errorf("warning 0 = %+v", w[0])
		}
		if w[1].Code != "BLANK_PAGE" || w[1].Page != 2 || w[1].Detail != nil {
			t.Errorf("warning 1 = %+v", w[1])
		}
		// GLYPHS_MISSING carries an array and an object in its detail.
		points, _ := w[2].Detail["code_points"].([]any)
		scripts, _ := w[2].Detail["scripts"].(map[string]any)
		if w[2].Code != "GLYPHS_MISSING" || len(points) != 2 || points[0] != "U+0E01" || scripts["Thai"] != float64(2) {
			t.Errorf("warning 2 = %+v", w[2])
		}
	})

	t.Run("opens the PDF and a rendered page", func(t *testing.T) {
		pdf, found, err := bundle.PDF()
		if err != nil || !found {
			t.Fatalf("PDF: found=%v err=%v", found, err)
		}
		if body := readAll(t, pdf); body != "%PDF-1.7" {
			t.Errorf("pdf = %q", body)
		}

		page, found, err := bundle.Page(1)
		if err != nil || !found {
			t.Fatalf("Page(1): found=%v err=%v", found, err)
		}
		if body := readAll(t, page); body != "page-1" {
			t.Errorf("page = %q", body)
		}
	})

	t.Run("a page that was not rendered is not an error", func(t *testing.T) {
		page, found, err := bundle.Page(2)
		if err != nil || found || page != nil {
			t.Errorf("Page(2): page=%v found=%v err=%v", page, found, err)
		}
	})

	t.Run("Close deletes the temp directory and can be called twice", func(t *testing.T) {
		if dirs, _ := filepath.Glob(filepath.Join(tempDir, "convert-bundle-*")); len(dirs) != 1 {
			t.Fatalf("temp directories before Close = %v", dirs)
		}
		if err := bundle.Close(); err != nil {
			t.Fatalf("Close: %v", err)
		}
		if err := bundle.Close(); err != nil {
			t.Fatalf("second Close: %v", err)
		}
		if dirs, _ := filepath.Glob(filepath.Join(tempDir, "convert-bundle-*")); len(dirs) != 0 {
			t.Errorf("temp directories after Close = %v", dirs)
		}
		if _, _, err := bundle.PDF(); err == nil {
			t.Error("PDF after Close succeeded")
		}
	})
}

func TestBundleZeroPaddingAfterTarEnd(t *testing.T) {
	// A tar writer may pad the archive to a whole 10240-byte record.
	for _, n := range []int{512, 9216, 10240} {
		c, _, _ := newFakeService(t, respondWith(tarZstWithTail(t, make([]byte, n), bundleEntries(t)...)))
		b, err := c.ConvertToBundle(context.Background(), "a.docx", strings.NewReader("x"), convert.Options{})
		if err != nil {
			t.Fatalf("%d zero bytes after the tar end: %v", n, err)
		}
		_ = b.Close()
	}
}

func TestConvertToImages(t *testing.T) {
	c, got, _ := newFakeService(t, respondWithBundle(t, false))

	bundle, err := c.ConvertToImages(context.Background(), "deck.pptx", strings.NewReader("pptx bytes"), convert.Options{})
	if err != nil {
		t.Fatalf("ConvertToImages: %v", err)
	}
	defer bundle.Close()

	t.Run("asks for the images output", func(t *testing.T) {
		if got.options["output"] != "images" {
			t.Errorf("output = %v", got.options["output"])
		}
	})

	t.Run("a bundle without a PDF is not an error", func(t *testing.T) {
		pdf, found, err := bundle.PDF()
		if err != nil || found || pdf != nil {
			t.Errorf("PDF: pdf=%v found=%v err=%v", pdf, found, err)
		}
	})
}

func TestBundleFailures(t *testing.T) {
	page := entry{name: "pages/0001.jpg", data: []byte("page-1")}
	pdf := entry{name: "document.pdf", data: []byte("%PDF-1.7")}
	badPDFPath := testManifest(true)
	badPDFPath["pdf"] = map[string]any{"path": "../document.pdf", "size_bytes": 8, "origin": "converted"}
	badPagePath := testManifest(false)
	badPagePath["pages"] = []map[string]any{{"number": 1, "path": "pages/../../x.jpg", "width_px": 1,
		"height_px": 1, "width_pt": 1, "height_pt": 1, "size_bytes": 1, "blank": false}}
	valid := bundleArchive(t, true)

	cases := []struct {
		name string
		body []byte
		want string
	}{
		{"not zstd", []byte("not a tar.zst"), "failed to read archive"},
		{"cut short", valid[:len(valid)/2], "failed to read archive"},
		{"empty archive", tarZst(t), "it is empty"},
		{"manifest not first", tarZst(t, pdf, manifestEntry(t, testManifest(true)), page), `the first entry is "document.pdf"`},
		{"a path with ..", tarZst(t, manifestEntry(t, testManifest(true)), entry{name: "pages/../document.pdf", data: []byte("x")}), "has a path with .."},
		{"an unexpected name", tarZst(t, manifestEntry(t, testManifest(true)), pdf, page, entry{name: "notes.txt", data: []byte("x")}), `unexpected entry "notes.txt"`},
		{"a symlink", tarZst(t, manifestEntry(t, testManifest(true)), pdf, entry{name: "pages/0001.jpg", typ: tar.TypeSymlink}), "is not a regular file"},
		{"an entry the manifest does not name", tarZst(t, manifestEntry(t, testManifest(true)), pdf, page, entry{name: "pages/0002.jpg", data: []byte("x")}), "is not in the manifest"},
		{"an entry twice", tarZst(t, manifestEntry(t, testManifest(true)), pdf, page, page), "appears twice"},
		{"a named file missing", tarZst(t, manifestEntry(t, testManifest(true)), page), `names "document.pdf", which is not in the archive`},
		{"a manifest PDF path outside the bundle", tarZst(t, manifestEntry(t, badPDFPath), pdf, page), "the PDF path is"},
		{"a manifest page path outside the bundle", tarZst(t, manifestEntry(t, badPagePath)), "page 1 has the path"},
		{"a manifest that is not JSON", tarZst(t, entry{name: "manifest.json", data: []byte("{")}), "failed to decode manifest"},
		{"a PDF shorter than its manifest size", tarZst(t, manifestEntry(t, testManifest(true)), entry{name: "document.pdf", data: []byte("%PDF")}, page),
			`entry "document.pdf" has 4 bytes, the manifest says 8`},
		{"a page longer than its manifest size", tarZst(t, manifestEntry(t, testManifest(true)), pdf, entry{name: "pages/0001.jpg", data: []byte("page-10")}),
			`entry "pages/0001.jpg" has 7 bytes, the manifest says 6`},
		{"trailing data after the zstd frame", append(append([]byte{}, valid...), []byte("trailing junk")...), "failed to read the end of the archive"},
		{"32 MiB of data after the tar end", tarZstWithTail(t, bytes.Repeat([]byte("A"), 32<<20), bundleEntries(t)...), "failed to read the end of the archive"},
		{"one non-zero byte after the tar end", tarZstWithTail(t, []byte("x"), bundleEntries(t)...), "non-zero data follows the tar end"},
		{"more zero padding than a tar record", tarZstWithTail(t, make([]byte, 10240+1), bundleEntries(t)...), "more than 10240 bytes follow the tar end"},
	}
	for _, tc := range cases {
		t.Run(tc.name+" fails and leaves no temp directory", func(t *testing.T) {
			c, _, tempDir := newFakeService(t, respondWith(tc.body))
			_, err := c.ConvertToBundle(context.Background(), "a.docx", strings.NewReader("x"), convert.Options{})
			if err == nil {
				t.Fatal("expected an error")
			}
			if !strings.Contains(err.Error(), tc.want) {
				t.Errorf("error = %v, want it to contain %q", err, tc.want)
			}
			if files, _ := os.ReadDir(tempDir); len(files) != 0 {
				t.Errorf("temp files left behind: %v", files)
			}
		})
	}
}

func TestOptions(t *testing.T) {
	c, got, _ := newFakeService(t, func(w http.ResponseWriter, _ *http.Request) {
		_, _ = w.Write([]byte("%PDF"))
	})

	opts := convert.Options{
		Pages:     "1-5,8",
		Password:  "secret",
		Timeout:   90500 * time.Millisecond,
		RequestID: "caller-id",
		Images:    convert.ImageOptions{Format: convert.JPEG, DPI: 200, Quality: 70, MaxEdge: 2576},
		Slides:    convert.SlideOptions{IncludeHidden: true, Notes: convert.NotesAppend},
		Sheets:    convert.SheetOptions{OnePagePerSheet: true},
	}
	pdf, err := c.ConvertToPDF(context.Background(), "a.pptx", strings.NewReader("x"), opts)
	if err != nil {
		t.Fatalf("ConvertToPDF: %v", err)
	}
	pdf.Body.Close()

	t.Run("every option reaches the service under its API name", func(t *testing.T) {
		want := map[string]any{
			"output":          "pdf",
			"pages":           "1-5,8",
			"password":        "secret",
			"timeout_seconds": float64(91), // rounded up to whole seconds
			"images":          map[string]any{"format": "jpeg", "dpi": float64(200), "quality": float64(70), "max_edge": float64(2576)},
			"slides":          map[string]any{"include_hidden": true, "notes": "append"},
			"sheets":          map[string]any{"one_page_per_sheet": true},
		}
		gotJSON, _ := json.Marshal(got.options)
		wantJSON, _ := json.Marshal(want)
		if string(gotJSON) != string(wantJSON) {
			t.Errorf("options =\n  %s\nwant\n  %s", gotJSON, wantJSON)
		}
	})

	t.Run("the request ID is sent as a header", func(t *testing.T) {
		if got.requestID != "caller-id" {
			t.Errorf("X-Request-ID = %q", got.requestID)
		}
	})
}

func TestErrors(t *testing.T) {
	cases := []struct {
		name   string
		status int
		code   string
		want   error
	}{
		{"bad request", 400, "BAD_REQUEST", convert.ErrInvalidRequest},
		{"validation error", 422, "VALIDATION_ERROR", convert.ErrInvalidRequest},
		{"unauthorized", 401, "UNAUTHORIZED", convert.ErrUnauthorized},
		{"file too large", 413, "FILE_TOO_LARGE", convert.ErrFileTooLarge},
		{"unsupported format", 415, "UNSUPPORTED_FORMAT", convert.ErrUnsupportedFormat},
		{"password required", 422, "PASSWORD_REQUIRED", convert.ErrPasswordRequired},
		{"invalid password", 422, "INVALID_PASSWORD", convert.ErrInvalidPassword},
		{"limit exceeded", 422, "LIMIT_EXCEEDED", convert.ErrLimitExceeded},
		{"conversion failed", 422, "CONVERSION_FAILED", convert.ErrConversionFailed},
		{"queue full", 429, "TOO_MANY_REQUESTS", convert.ErrBusy},
		{"internal error", 500, "INTERNAL_ERROR", convert.ErrInternal},
		{"not ready", 503, "SERVICE_UNAVAILABLE", convert.ErrUnavailable},
		{"timeout", 504, "CONVERSION_TIMEOUT", convert.ErrTimeout},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			c, _, _ := newFakeService(t, func(w http.ResponseWriter, _ *http.Request) {
				writeError(w, tc.status, tc.code, "server says no")
			})

			_, err := c.ConvertToPDF(context.Background(), "a.docx", strings.NewReader("x"), convert.Options{})
			if !errors.Is(err, tc.want) {
				t.Fatalf("error = %v, want it to wrap %v", err, tc.want)
			}
			for _, fragment := range []string{"failed to convert document", "server says no", "req-1"} {
				if !strings.Contains(err.Error(), fragment) {
					t.Errorf("error %q does not mention %q", err, fragment)
				}
			}
		})
	}

	t.Run("a non-JSON answer from a proxy is mapped by its status", func(t *testing.T) {
		c, _, _ := newFakeService(t, func(w http.ResponseWriter, _ *http.Request) {
			w.WriteHeader(http.StatusGatewayTimeout)
			_, _ = w.Write([]byte("stream timeout"))
		})

		_, err := c.ConvertToBundle(context.Background(), "a.docx", strings.NewReader("x"), convert.Options{})
		if !errors.Is(err, convert.ErrTimeout) {
			t.Errorf("error = %v, want it to wrap ErrTimeout", err)
		}
	})

	t.Run("a cancelled context fails without a sentinel", func(t *testing.T) {
		c, _, _ := newFakeService(t, func(http.ResponseWriter, *http.Request) {})
		ctx, cancel := context.WithCancel(context.Background())
		cancel()

		_, err := c.ConvertToPDF(ctx, "a.docx", strings.NewReader("x"), convert.Options{})
		if !errors.Is(err, context.Canceled) {
			t.Errorf("error = %v, want it to wrap context.Canceled", err)
		}
		if convert.Retryable(err) {
			t.Errorf("a cancelled call must not look retryable")
		}
	})
}

func TestRetryable(t *testing.T) {
	retryable := []error{convert.ErrBusy, convert.ErrUnavailable, convert.ErrInternal}
	final := []error{
		convert.ErrInvalidRequest, convert.ErrUnauthorized, convert.ErrFileTooLarge,
		convert.ErrUnsupportedFormat, convert.ErrPasswordRequired, convert.ErrInvalidPassword,
		convert.ErrLimitExceeded, convert.ErrConversionFailed, convert.ErrTimeout,
	}
	for _, err := range retryable {
		if !convert.Retryable(err) {
			t.Errorf("%v should be retryable", err)
		}
	}
	for _, err := range final {
		if convert.Retryable(err) {
			t.Errorf("%v should not be retryable", err)
		}
	}
}

func TestCapabilitiesAndReady(t *testing.T) {
	ready := true
	mux := http.NewServeMux()
	mux.HandleFunc("GET /v1/capabilities", func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer "+apiKey {
			writeError(w, http.StatusUnauthorized, "UNAUTHORIZED", "missing or invalid API key")
			return
		}
		_ = json.NewEncoder(w).Encode(map[string]any{
			"service": map[string]any{"name": "convert", "version": "1.0.0"},
			"engine":  map[string]any{"service_version": "1.0.0", "libreoffice": "26.8.3.2", "pdfium": "8125", "fingerprint": "sha256:abc"},
			"inputs": []map[string]any{{
				"format": "docx", "family": "document", "engine": "libreoffice", "extensions": []string{"docx"},
				"media_types": []string{"application/vnd.openxmlformats-officedocument.wordprocessingml.document"},
			}},
			"outputs":       []string{"pdf", "images", "bundle"},
			"image_formats": []string{"jpeg", "png"},
			"limits": map[string]any{
				"max_upload_bytes": 125829120, "max_render_pages": 500, "max_dpi": 300, "max_edge_px": 4096,
				"default_timeout_seconds": 120, "max_timeout_seconds": 210,
			},
			"fonts": []string{"Carlito", "Liberation Sans"},
		})
	})
	mux.HandleFunc("GET /readyz", func(w http.ResponseWriter, _ *http.Request) {
		if !ready {
			w.WriteHeader(http.StatusServiceUnavailable)
		}
		_, _ = w.Write([]byte(`{"status":"ready","pools":{"office":{"target":1,"idle":1,"busy":0},"render":{"target":2,"idle":2,"busy":0}}}`))
	})
	server := httptest.NewServer(mux)
	defer server.Close()

	c, err := convert.New(server.URL, apiKey)
	if err != nil {
		t.Fatalf("New: %v", err)
	}

	t.Run("reads capabilities", func(t *testing.T) {
		caps, err := c.Capabilities(context.Background())
		if err != nil {
			t.Fatalf("Capabilities: %v", err)
		}
		if caps.ServiceVersion != "1.0.0" || caps.Engine.LibreOffice != "26.8.3.2" {
			t.Errorf("capabilities = %+v", caps)
		}
		if len(caps.Formats) != 1 || caps.Formats[0].Name != "docx" || caps.Formats[0].Engine != "libreoffice" {
			t.Errorf("formats = %+v", caps.Formats)
		}
		if len(caps.ImageFormats) != 2 || caps.ImageFormats[1] != convert.PNG {
			t.Errorf("image formats = %v", caps.ImageFormats)
		}
		if caps.Limits.MaxUploadBytes != 125829120 || caps.Limits.MaxDPI != 300 || caps.Limits.MaxTimeout != 210*time.Second {
			t.Errorf("limits = %+v", caps.Limits)
		}
	})

	t.Run("a wrong key is unauthorized", func(t *testing.T) {
		wrong, _ := convert.New(server.URL, "wrong-key")
		if _, err := wrong.Capabilities(context.Background()); !errors.Is(err, convert.ErrUnauthorized) {
			t.Errorf("error = %v, want it to wrap ErrUnauthorized", err)
		}
	})

	t.Run("ready and not ready are both answers, not errors", func(t *testing.T) {
		if ok, err := c.Ready(context.Background()); err != nil || !ok {
			t.Errorf("Ready = %v, %v", ok, err)
		}
		ready = false
		if ok, err := c.Ready(context.Background()); err != nil || ok {
			t.Errorf("Ready while draining = %v, %v", ok, err)
		}
	})
}

func TestNew(t *testing.T) {
	if _, err := convert.New("", apiKey); err == nil {
		t.Error("an empty host should be rejected")
	}
	if _, err := convert.New("http://localhost:8080", ""); err == nil {
		t.Error("an empty API key should be rejected")
	}
}

// noiseReader yields bytes that do not compress, so the compressed stream
// is as long as the entry and its count says how much the client pulled.
type noiseReader struct{ state uint64 }

func (r *noiseReader) Read(p []byte) (int, error) {
	for i := range p {
		r.state ^= r.state << 13
		r.state ^= r.state >> 7
		r.state ^= r.state << 17
		p[i] = byte(r.state)
	}
	return len(p), nil
}

// countingWriter counts the bytes written through it.
type countingWriter struct {
	w io.Writer
	n atomic.Int64
}

func (c *countingWriter) Write(p []byte) (int, error) {
	n, err := c.w.Write(p)
	c.n.Add(int64(n))
	return n, err
}

// TestBundleEntryOverItsManifestSize serves a PDF entry of 256 MiB of noise
// that the manifest says has 8 bytes. The noise does not compress, so the
// bytes the server manages to send are about the bytes the client read: it
// must fail on the tar header, before it writes, and leave no file behind.
func TestBundleEntryOverItsManifestSize(t *testing.T) {
	const huge = 256 << 20
	sent := &countingWriter{}
	respond := func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "application/zstd")
		sent.w = w
		zw, err := zstd.NewWriter(sent, zstd.WithEncoderLevel(zstd.SpeedFastest))
		if err != nil {
			t.Errorf("zstd writer: %v", err)
			return
		}
		defer zw.Close()
		tw := tar.NewWriter(zw)
		raw, _ := json.Marshal(testManifest(true))
		_ = tw.WriteHeader(&tar.Header{Name: "manifest.json", Mode: 0o644, Size: int64(len(raw)), Typeflag: tar.TypeReg, Format: tar.FormatUSTAR})
		_, _ = tw.Write(raw)
		_ = tw.WriteHeader(&tar.Header{Name: "document.pdf", Mode: 0o644, Size: huge, Typeflag: tar.TypeReg, Format: tar.FormatUSTAR})
		_, _ = io.Copy(tw, io.LimitReader(&noiseReader{state: 0x9E3779B97F4A7C15}, huge))
	}
	c, _, tempDir := newFakeService(t, respond)
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	_, err := c.ConvertToBundle(ctx, "a.docx", strings.NewReader("x"), convert.Options{})
	if err == nil || !strings.Contains(err.Error(), `entry "document.pdf" has 268435456 bytes, the manifest says 8`) {
		t.Fatalf("error = %v, want the size mismatch", err)
	}
	if files, _ := os.ReadDir(tempDir); len(files) != 0 {
		t.Errorf("temp files left behind: %v", files)
	}
	// What the server sent before the client hung up: the header and the
	// socket buffers, not the entry.
	if n := sent.n.Load(); n > 32<<20 {
		t.Errorf("the server sent %d bytes before the client hung up", n)
	}
}

// TestBundleTotalCap refuses a bundle whose manifest sizes add up to more
// than WithMaxBundleBytes, before anything is written.
func TestBundleTotalCap(t *testing.T) {
	tempDir := t.TempDir()
	server := httptest.NewServer(respondWithBundle(t, true))
	defer server.Close()
	c, err := convert.New(server.URL, apiKey, convert.WithTempDir(tempDir), convert.WithMaxBundleBytes(10))
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	_, err = c.ConvertToBundle(context.Background(), "a.docx", strings.NewReader("x"), convert.Options{})
	if err == nil || !strings.Contains(err.Error(), "14 bytes, more than the 10 that WithMaxBundleBytes allows") {
		t.Fatalf("error = %v, want the total cap", err)
	}
	if files, _ := os.ReadDir(tempDir); len(files) != 0 {
		t.Errorf("temp files left behind: %v", files)
	}
	// A page path given twice, with two sizes, is refused before the
	// sizes are added up.
	dup := testManifest(true)
	dup["pages"] = []map[string]any{
		{"number": 1, "path": "pages/0001.jpg", "width_px": 1, "height_px": 1, "width_pt": 1, "height_pt": 1, "size_bytes": 6, "blank": false},
		{"number": 2, "path": "pages/0001.jpg", "width_px": 1, "height_px": 1, "width_pt": 1, "height_pt": 1, "size_bytes": 1 << 30, "blank": false},
	}
	dupServer := httptest.NewServer(respondWith(tarZst(t, manifestEntry(t, dup), entry{name: "document.pdf", data: []byte("%PDF-1.7")}, entry{name: "pages/0001.jpg", data: []byte("page-1")})))
	defer dupServer.Close()
	c, _ = convert.New(dupServer.URL, apiKey, convert.WithTempDir(tempDir), convert.WithMaxBundleBytes(1<<20))
	_, err = c.ConvertToBundle(context.Background(), "a.docx", strings.NewReader("x"), convert.Options{})
	if err == nil || !strings.Contains(err.Error(), `page 2 repeats the path "pages/0001.jpg"`) {
		t.Fatalf("error = %v, want the repeated path", err)
	}
	if files, _ := os.ReadDir(tempDir); len(files) != 0 {
		t.Errorf("temp files left behind: %v", files)
	}

	// Exactly at the cap, the bundle opens.
	c, _ = convert.New(server.URL, apiKey, convert.WithTempDir(tempDir), convert.WithMaxBundleBytes(14))
	bundle, err := c.ConvertToBundle(context.Background(), "a.docx", strings.NewReader("x"), convert.Options{})
	if err != nil {
		t.Fatalf("at the cap: %v", err)
	}
	_ = bundle.Close()
}

// TestCapabilitiesBodyIsBounded serves a valid capabilities answer of
// 2 MiB: without the limit it would decode, so only the 1 MiB limit makes it
// fail.
func TestCapabilitiesBodyIsBounded(t *testing.T) {
	var answer atomic.Pointer[string]
	long := `{"fonts":["` + strings.Repeat("a", 2<<20) + `"]}`
	answer.Store(&long)
	mux := http.NewServeMux()
	mux.HandleFunc("GET /v1/capabilities", func(w http.ResponseWriter, _ *http.Request) {
		_, _ = io.WriteString(w, *answer.Load())
	})
	server := httptest.NewServer(mux)
	defer server.Close()
	c, err := convert.New(server.URL, apiKey)
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	_, err = c.Capabilities(ctx)
	if err == nil || !strings.Contains(err.Error(), "at most 1048576 bytes are read") {
		t.Fatalf("error = %v, want the limit", err)
	}
	// The same answer under the limit decodes.
	short := `{"fonts":["` + strings.Repeat("a", 1000) + `"]}`
	answer.Store(&short)
	if _, err := c.Capabilities(ctx); err != nil {
		t.Fatalf("a short answer: %v", err)
	}
}

// TestBundleZstdWindowOver8MiB serves a valid bundle in a frame that
// declares a 16 MiB window: the decoder refuses it before it allocates the
// window.
func TestBundleZstdWindowOver8MiB(t *testing.T) {
	// The tar, in one raw block of a frame built by hand: an encoder picks
	// a smaller window for so little data.
	var tarred bytes.Buffer
	tw := tar.NewWriter(&tarred)
	for _, e := range bundleEntries(t) {
		_ = tw.WriteHeader(&tar.Header{Name: e.name, Mode: 0o644, Size: int64(len(e.data)), Typeflag: tar.TypeReg, Format: tar.FormatUSTAR})
		_, _ = tw.Write(e.data)
	}
	_ = tw.Close()
	var buf bytes.Buffer
	buf.Write([]byte{0x28, 0xB5, 0x2F, 0xFD}) // magic
	buf.WriteByte(0x00)                       // no single segment, no content size, no checksum
	buf.WriteByte(14 << 3)                    // window 2^(10+14) = 16 MiB
	size := tarred.Len()
	block := size<<3 | 1 // raw block, the last one
	buf.Write([]byte{byte(block), byte(block >> 8), byte(block >> 16)})
	buf.Write(tarred.Bytes())
	c, _, tempDir := newFakeService(t, respondWith(buf.Bytes()))
	_, err := c.ConvertToBundle(context.Background(), "a.docx", strings.NewReader("x"), convert.Options{})
	if err == nil || !strings.Contains(err.Error(), "window size exceeded") {
		t.Fatalf("error = %v, want the window refused", err)
	}
	if files, _ := os.ReadDir(tempDir); len(files) != 0 {
		t.Errorf("temp files left behind: %v", files)
	}
}

// TestNegativeBundleCapIsRefused: 0 means no cap, a negative cap is a
// mistake, not unlimited.
func TestNegativeBundleCapIsRefused(t *testing.T) {
	if _, err := convert.New("http://x", apiKey, convert.WithMaxBundleBytes(-1)); err == nil {
		t.Fatal("expected an error")
	}
}
