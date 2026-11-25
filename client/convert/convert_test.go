package convert_test

import (
	"archive/zip"
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
	"testing"
	"time"

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

// bundleZIP builds a bundle as the service would. withPDF false gives the
// shape of output=images.
func bundleZIP(t *testing.T, withPDF bool) []byte {
	t.Helper()

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
				"width_pt": 960, "height_pt": 540, "size_bytes": 6, "blank": false},
		},
		"warnings": []map[string]any{
			{"code": "FONT_SUBSTITUTED", "message": "Aptos was replaced by Carlito",
				"detail": map[string]any{"requested": "Aptos", "substitute": "Carlito"}},
			{"code": "BLANK_PAGE", "message": "page 2 is blank", "page": 2},
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

	var buf bytes.Buffer
	zw := zip.NewWriter(&buf)
	add := func(name string, data []byte) {
		w, err := zw.Create(name)
		if err != nil {
			t.Fatalf("zip create %s: %v", name, err)
		}
		if _, err := w.Write(data); err != nil {
			t.Fatalf("zip write %s: %v", name, err)
		}
	}
	manifestJSON, _ := json.Marshal(manifest)
	add("manifest.json", manifestJSON)
	if withPDF {
		add("document.pdf", []byte("%PDF-1.7"))
	}
	add("pages/0001.jpg", []byte("page-1"))
	if err := zw.Close(); err != nil {
		t.Fatalf("zip close: %v", err)
	}
	return buf.Bytes()
}

func respondWithBundle(t *testing.T, withPDF bool) http.HandlerFunc {
	return func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "application/zip")
		w.Header().Set("X-Request-ID", "req-1")
		w.Header().Set("Convert-Page-Count", "2")
		w.Header().Set("Convert-Warning-Count", "2")
		_, _ = w.Write(bundleZIP(t, withPDF))
	}
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
		if len(m.Pages) != 1 || m.Pages[0].Number != 1 || m.Pages[0].WidthPx != 2000 || m.Pages[0].WidthPt != 960 {
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
		if len(w) != 2 {
			t.Fatalf("warnings = %+v", w)
		}
		if w[0].Code != "FONT_SUBSTITUTED" || w[0].Page != 0 || w[0].Detail["requested"] != "Aptos" {
			t.Errorf("warning 0 = %+v", w[0])
		}
		if w[1].Code != "BLANK_PAGE" || w[1].Page != 2 || w[1].Detail != nil {
			t.Errorf("warning 1 = %+v", w[1])
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

	t.Run("Close deletes the temp file and can be called twice", func(t *testing.T) {
		if files, _ := filepath.Glob(filepath.Join(tempDir, "convert-bundle-*.zip")); len(files) != 1 {
			t.Fatalf("temp files before Close = %v", files)
		}
		if err := bundle.Close(); err != nil {
			t.Fatalf("Close: %v", err)
		}
		if err := bundle.Close(); err != nil {
			t.Fatalf("second Close: %v", err)
		}
		if files, _ := filepath.Glob(filepath.Join(tempDir, "convert-bundle-*.zip")); len(files) != 0 {
			t.Errorf("temp files after Close = %v", files)
		}
	})
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
	t.Run("a response that is not a ZIP fails and leaves no temp file", func(t *testing.T) {
		c, _, tempDir := newFakeService(t, func(w http.ResponseWriter, _ *http.Request) {
			_, _ = w.Write([]byte("not a zip"))
		})

		_, err := c.ConvertToBundle(context.Background(), "a.docx", strings.NewReader("x"), convert.Options{})
		if err == nil {
			t.Fatal("expected an error")
		}
		if files, _ := os.ReadDir(tempDir); len(files) != 0 {
			t.Errorf("temp files left behind: %v", files)
		}
	})
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
		Images:    convert.ImageOptions{Format: convert.WebP, DPI: 200, Quality: 70, MaxEdge: 2576},
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
			"images":          map[string]any{"format": "webp", "dpi": float64(200), "quality": float64(70), "max_edge": float64(2576)},
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
			"image_formats": []string{"jpeg", "png", "webp"},
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
		if len(caps.ImageFormats) != 3 || caps.ImageFormats[2] != convert.WebP {
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
