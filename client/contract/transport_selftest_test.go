package contract

import (
	"archive/tar"
	"archive/zip"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"image/png"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"os/signal"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"

	"github.com/klauspost/compress/zstd"

	"github.com/viktordanov/rs-convert/client/convert"
)

// The self-tests prove that the validating transport and the harness catch
// what they must, with canned responses and no convertd. They run in every
// mode, and without the contract build tag.

// recorder collects what a transport reports instead of failing the test.
type recorder struct {
	mu   sync.Mutex
	msgs []string
}

func (r *recorder) Errorf(format string, args ...any) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.msgs = append(r.msgs, fmt.Sprintf(format, args...))
}

func (r *recorder) all() []string {
	r.mu.Lock()
	defer r.mu.Unlock()
	return append([]string(nil), r.msgs...)
}

// canned is one response the test server sends.
type canned struct {
	status  int
	headers map[string]string
	body    []byte
}

func cannedServer(t *testing.T, c canned) *httptest.Server {
	t.Helper()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = io.Copy(io.Discard, r.Body)
		for k, v := range c.headers {
			w.Header().Set(k, v)
		}
		w.WriteHeader(c.status)
		_, _ = w.Write(c.body)
	}))
	t.Cleanup(srv.Close)
	return srv
}

func selfTestTransport(t *testing.T) (*validatingTransport, *recorder) {
	t.Helper()
	s, err := theSpec()
	if err != nil {
		t.Fatalf("failed to load the committed spec: %v", err)
	}
	return selfTestTransportFor(s)
}

func selfTestTransportFor(s *contractSpec) (*validatingTransport, *recorder) {
	rec := &recorder{}
	return &validatingTransport{base: http.DefaultTransport, spec: s, report: rec}, rec
}

func pdfHeaders() map[string]string {
	return map[string]string{
		"Content-Type":               "application/pdf",
		"X-Request-ID":               "req-1",
		"Content-Disposition":        `attachment; filename="doc.pdf"`,
		"Convert-Source-Format":      "pdf",
		"Convert-Page-Count":         "1",
		"Convert-Warning-Count":      "0",
		"Convert-Engine-Fingerprint": "sha256:5b0f6e1c9a2d",
		"Server-Timing":              "recv;dur=0.120, detect;dur=0.010, total;dur=1.500",
	}
}

func bundleHeaders() map[string]string {
	h := pdfHeaders()
	h["Content-Type"] = "application/zstd"
	h["Content-Disposition"] = `attachment; filename="doc.tar.zst"`
	return h
}

func zipNamedHeaders() map[string]string {
	h := bundleHeaders()
	h["Content-Disposition"] = `attachment; filename="doc.zip"`
	return h
}

func mustJSON(t *testing.T, v any) []byte {
	t.Helper()
	b, err := json.Marshal(v)
	if err != nil {
		t.Fatalf("failed to encode JSON: %v", err)
	}
	return b
}

func errorHeaders(extra ...string) map[string]string {
	h := map[string]string{"Content-Type": "application/json", "X-Request-ID": "req-1"}
	for i := 0; i+1 < len(extra); i += 2 {
		h[extra[i]] = extra[i+1]
	}
	return h
}

func errorBody(code string) []byte {
	return []byte(`{"error":"something went wrong","code":"` + code + `"}`)
}

// validManifest is a manifest for a bundle with a PDF and one JPEG page.
func validManifest() map[string]any {
	return map[string]any{
		"manifest_version": 1,
		"request_id":       "req-1",
		"source": map[string]any{
			"filename": "doc.docx", "format": "docx", "family": "document",
			"size_bytes": 1200, "sha256": strings.Repeat("ab", 32), "encrypted": false,
		},
		"page_count": 1,
		"pdf":        map[string]any{"path": "document.pdf", "size_bytes": 400, "origin": "converted"},
		"images":     map[string]any{"format": "jpeg", "dpi": 150, "quality": 85},
		"pages": []any{map[string]any{
			"number": 1, "path": "pages/0001.jpg", "width_px": 1275, "height_px": 1650,
			"width_pt": 612, "height_pt": 792, "size_bytes": 1000, "blank": false,
		}},
		"warnings":   []any{},
		"engine":     map[string]any{"service_version": "0.1.0", "pdfium": "8125", "fingerprint": "sha256:5b0f6e1c9a2d"},
		"timings_ms": map[string]any{"queue": 0, "convert": 10, "render": 5, "total": 16},
	}
}

// bundleArchive builds a tar.zst holding manifest.json and then the named
// files, in that order.
func bundleArchive(t *testing.T, manifest map[string]any, files ...string) []byte {
	t.Helper()
	raw, err := json.Marshal(manifest)
	if err != nil {
		t.Fatalf("failed to encode manifest: %v", err)
	}
	entries := []archiveEntry{{name: "manifest.json", regular: true, data: raw}}
	for _, f := range files {
		entries = append(entries, archiveEntry{name: f, regular: true, data: []byte("x")})
	}
	return writeArchive(t, entries)
}

// writeArchive builds a tar.zst from entries in order. A non-regular entry
// becomes a symlink.
func writeArchive(t *testing.T, entries []archiveEntry) []byte {
	t.Helper()
	var b bytes.Buffer
	zw, err := zstd.NewWriter(&b, zstd.WithEncoderLevel(zstd.SpeedFastest))
	if err != nil {
		t.Fatalf("failed to start zstd: %v", err)
	}
	tw := tar.NewWriter(zw)
	for _, e := range entries {
		hdr := &tar.Header{Name: e.name, Mode: 0o644, Size: int64(len(e.data)), Typeflag: tar.TypeReg, Format: tar.FormatUSTAR}
		if !e.regular {
			hdr.Typeflag, hdr.Size, hdr.Linkname = tar.TypeSymlink, 0, "/etc/passwd"
		}
		if err := tw.WriteHeader(hdr); err != nil {
			t.Fatalf("failed to write %s: %v", e.name, err)
		}
		if e.regular {
			if _, err := tw.Write(e.data); err != nil {
				t.Fatalf("failed to write %s: %v", e.name, err)
			}
		}
	}
	if err := tw.Close(); err != nil {
		t.Fatalf("failed to finish tar: %v", err)
	}
	if err := zw.Close(); err != nil {
		t.Fatalf("failed to finish zstd: %v", err)
	}
	return b.Bytes()
}

// The calls a case can make through a client.
const (
	callConvertPDF    = "facade ConvertToPDF"
	callConvertBundle = "facade ConvertToBundle"
	callCapabilities  = "generated GetCapabilities"
	callReadiness     = "generated GetReadiness"
	callMetrics       = "generated GetMetrics"
)

func makeCall(t *testing.T, call, baseURL string, rt http.RoundTripper) {
	t.Helper()
	ctx := context.Background()
	switch call {
	case callConvertPDF, callConvertBundle:
		c := facadeFor(t, baseURL, "k", rt)
		// The facade's own result does not matter here; the transport's
		// findings do.
		if call == callConvertPDF {
			if res, err := c.ConvertToPDF(ctx, "doc.pdf", bytes.NewReader(onePagePDF()), convert.Options{}); err == nil {
				_ = res.Body.Close()
			}
		} else if b, err := c.ConvertToBundle(ctx, "doc.docx", bytes.NewReader(docx()), convert.Options{}); err == nil {
			_ = b.Close()
		}
	case callCapabilities, callReadiness, callMetrics:
		g := generatedFor(t, baseURL, "k", rt)
		var resp *http.Response
		var err error
		switch call {
		case callCapabilities:
			resp, err = g.GetCapabilities(ctx, nil)
		case callReadiness:
			resp, err = g.GetReadiness(ctx)
		default:
			resp, err = g.GetMetrics(ctx)
		}
		if err != nil {
			t.Fatalf("%s: %v", call, err)
		}
		_ = resp.Body.Close()
	default:
		t.Fatalf("unknown call %q", call)
	}
}

func TestSelfTestTransport(t *testing.T) {
	noPageCount := pdfHeaders()
	delete(noPageCount, "Convert-Page-Count")
	zeroPageCount := pdfHeaders()
	zeroPageCount["Convert-Page-Count"] = "0"
	htmlType := pdfHeaders()
	htmlType["Content-Type"] = "text/html"
	noPageCountManifest := validManifest()
	delete(noPageCountManifest, "page_count")
	badPageManifest := validManifest()
	badPageManifest["pages"].([]any)[0].(map[string]any)["width_px"] = 0

	capabilities := `{"service":{"name":"convert","version":"0.1.0"},
		"engine":{"service_version":"0.1.0","pdfium":"unknown","fingerprint":"sha256:0"},
		"inputs":[{"format":"pdf","family":"pdf","extensions":["pdf"],"media_types":["application/pdf"],"engine":"pdfium"}],
		"outputs":["pdf","images","bundle"],"image_formats":["jpeg","png"],
		"limits":{"max_upload_bytes":1048576,"max_render_pages":500,"max_dpi":300,"max_edge_px":4096,
		          "default_timeout_seconds":120,"max_timeout_seconds":210},
		"fonts":[]}`

	cases := []struct {
		name string
		call string
		resp canned
		want []string // each must be in some reported line; none means no line at all
	}{
		// Controls: valid responses must pass, or every failure below is noise.
		{"valid PDF", callConvertPDF, canned{200, pdfHeaders(), onePagePDF()}, nil},
		{"valid bundle", callConvertBundle, canned{200, bundleHeaders(), bundleArchive(t, validManifest(), "document.pdf", "pages/0001.jpg")}, nil},
		{"valid 401", callConvertPDF, canned{401, errorHeaders(), errorBody("UNAUTHORIZED")}, nil},
		{"valid 429", callConvertPDF, canned{429, errorHeaders("Retry-After", "5"), errorBody("TOO_MANY_REQUESTS")}, nil},
		{"valid capabilities", callCapabilities, canned{200, errorHeaders(), []byte(capabilities)}, nil},
		{"valid readiness", callReadiness, canned{200, map[string]string{"Content-Type": "application/json"},
			[]byte(`{"status":"ready","pools":{"office":{"target":1,"idle":1,"busy":0},"render":{"target":1,"idle":1,"busy":0}}}`)}, nil},
		{"valid metrics", callMetrics, canned{200, map[string]string{"Content-Type": "text/plain; version=0.0.4"}, []byte("up 1\n")}, nil},

		// The violations the transport must report.
		{"200 without Convert-Page-Count", callConvertPDF, canned{200, noPageCount, onePagePDF()},
			[]string{"convertDocument 200: declared header Convert-Page-Count is missing"}},
		{"429 without Retry-After", callConvertPDF, canned{429, errorHeaders(), errorBody("TOO_MANY_REQUESTS")},
			[]string{"convertDocument 429: declared header Retry-After is missing"}},
		{"undeclared status 418", callConvertPDF, canned{418, errorHeaders(), errorBody("BAD_REQUEST")},
			[]string{"convertDocument 418: status is not supported"}},
		{"manifest without page_count", callConvertBundle, canned{200, bundleHeaders(), bundleArchive(t, noPageCountManifest, "document.pdf", "pages/0001.jpg")},
			[]string{"convertDocument 200: bundle: manifest.json:", "page_count"}},
		{"error code not in the enum", callConvertPDF, canned{422, errorHeaders(), errorBody("NOT_A_CODE")},
			[]string{"convertDocument 422:", `at "/code"`}},

		// More of what the transport must catch.
		{"header value out of range", callConvertPDF, canned{200, zeroPageCount, onePagePDF()},
			[]string{"convertDocument 200:", "Convert-Page-Count"}},
		{"undeclared content type", callConvertPDF, canned{200, htmlType, []byte("<html></html>")},
			[]string{"convertDocument 200:", "text/html"}},
		{"manifest value out of range", callConvertBundle, canned{200, bundleHeaders(), bundleArchive(t, badPageManifest, "document.pdf", "pages/0001.jpg")},
			[]string{"convertDocument 200: bundle: manifest.json:", `/pages/0/width_px`}},
		{"file in the archive but not in the manifest", callConvertBundle, canned{200, bundleHeaders(), bundleArchive(t, validManifest(), "document.pdf", "pages/0001.jpg", "extra.txt")},
			[]string{"convertDocument 200: bundle: extra.txt is in the archive but not in the manifest"}},
		{"page in the manifest but not in the archive", callConvertBundle, canned{200, bundleHeaders(), bundleArchive(t, validManifest(), "document.pdf")},
			[]string{"convertDocument 200: bundle: manifest names pages/0001.jpg, which is not in the archive"}},
		{"manifest not first", callConvertBundle, canned{200, bundleHeaders(), writeArchive(t, []archiveEntry{
			{name: "document.pdf", regular: true, data: []byte("x")},
			{name: "manifest.json", regular: true, data: mustJSON(t, validManifest())},
			{name: "pages/0001.jpg", regular: true, data: []byte("x")},
		})}, []string{"convertDocument 200: bundle: the first entry is document.pdf, not manifest.json"}},
		{"entries out of order", callConvertBundle, canned{200, bundleHeaders(), bundleArchive(t, validManifest(), "pages/0001.jpg", "document.pdf")},
			[]string{"convertDocument 200: bundle: entries are in the order"}},
		{"a symlink entry", callConvertBundle, canned{200, bundleHeaders(), writeArchive(t, []archiveEntry{
			{name: "manifest.json", regular: true, data: mustJSON(t, validManifest())},
			{name: "document.pdf", regular: true, data: []byte("x")},
			{name: "pages/0001.jpg"},
		})}, []string{"convertDocument 200: bundle: pages/0001.jpg is not a regular file"}},
		{"not a tar.zst", callConvertBundle, canned{200, bundleHeaders(), []byte("PK\x03\x04 a zip")},
			[]string{"convertDocument 200: bundle: not a valid tar.zst"}},
		{"Content-Disposition names a ZIP", callConvertBundle, canned{200, zipNamedHeaders(), bundleArchive(t, validManifest(), "document.pdf", "pages/0001.jpg")},
			[]string{"convertDocument 200: bundle: Content-Disposition"}},
		{"capabilities missing a required field", callCapabilities, canned{200, errorHeaders(), []byte(`{"service":{"name":"convert","version":"0.1.0"}}`)},
			[]string{"getCapabilities 200:", "engine"}},
	}

	// The committed spec is the contract (OpenAPI 3.1, emitted by convertd).
	committed, err := theSpec()
	if err != nil {
		t.Fatalf("failed to load the committed spec: %v", err)
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			runTransportCase(t, committed, tc.call, tc.resp, tc.want)
		})
	}
}

func runTransportCase(t *testing.T, s *contractSpec, call string, resp canned, want []string) {
	t.Helper()
	srv := cannedServer(t, resp)
	rt, rec := selfTestTransportFor(s)
	makeCall(t, call, srv.URL, rt)
	got := rec.all()
	if want == nil {
		if len(got) != 0 {
			t.Fatalf("valid response reported:\n  %s", strings.Join(got, "\n  "))
		}
		return
	}
	joined := strings.Join(got, "\n")
	for _, w := range want {
		if !strings.Contains(joined, w) {
			t.Errorf("want a report containing %q, got:\n  %s", w, strings.Join(got, "\n  "))
		}
	}
	for _, line := range got {
		t.Logf("reported: %s", line)
	}
}

func TestSelfTestTransportRecordsCoverage(t *testing.T) {
	srv := cannedServer(t, canned{200, map[string]string{"Content-Type": "text/plain"}, []byte("up 1\n")})
	rt, rec := selfTestTransport(t)
	rt.record = true
	// Leave the live run's coverage report as this test found it.
	metrics := observation{operationID: "getMetrics", status: "200"}
	coverage.Lock()
	seenBefore := coverage.seen[metrics]
	coverage.Unlock()
	t.Cleanup(func() {
		coverage.Lock()
		defer coverage.Unlock()
		if !seenBefore {
			delete(coverage.seen, metrics)
		}
	})
	makeCall(t, callMetrics, srv.URL, rt)
	if got := rec.all(); len(got) != 0 {
		t.Fatalf("unexpected reports: %v", got)
	}
	s, _ := theSpec()
	for _, o := range unobserved(s) {
		if o.operationID == "getMetrics" && o.status == "200" {
			t.Fatal("getMetrics 200 was observed but is still listed as unobserved")
		}
	}
	if len(s.declaredPairs()) < 15 {
		t.Fatalf("only %d declared pairs; the spec has more", len(s.declaredPairs()))
	}
}

func TestSelfTestFixtures(t *testing.T) {
	pdf := onePagePDF()
	if !bytes.HasPrefix(pdf, []byte("%PDF-")) || !bytes.HasSuffix(pdf, []byte("%%EOF\n")) {
		t.Errorf("PDF fixture has no header or trailer")
	}
	if xref := bytes.Index(pdf, []byte("xref\n")); !bytes.Contains(pdf, []byte(fmt.Sprintf("startxref\n%d\n", xref))) {
		t.Errorf("PDF fixture startxref does not point at its xref table (%d)", xref)
	}

	const marker = "FAKE:DELAY_MS=3000"
	doc := docxWithMarker(marker)
	if !containsMarker(doc, marker) {
		t.Errorf("DOCX fixture does not hold the marker verbatim")
	}
	zr, err := zip.NewReader(bytes.NewReader(doc), int64(len(doc)))
	if err != nil {
		t.Fatalf("DOCX fixture is not a ZIP: %v", err)
	}
	var names []string
	for _, f := range zr.File {
		names = append(names, f.Name)
		// LibreOffice does not load a DOCX with a stored entry followed by
		// a data descriptor (flag bit 3).
		if f.Name == "word/document.xml" && (f.Method != zip.Store || f.Flags&0x8 != 0) {
			t.Errorf("word/document.xml: method %d, flags %#x; want stored without a data descriptor", f.Method, f.Flags)
		}
	}
	if strings.Join(names, ",") != "[Content_Types].xml,_rels/.rels,word/document.xml" {
		t.Errorf("DOCX parts = %v", names)
	}

	img, err := png.Decode(bytes.NewReader(pngImage()))
	if err != nil {
		t.Fatalf("PNG fixture does not decode: %v", err)
	}
	if b := img.Bounds(); b.Dx() != 2 || b.Dy() != 2 {
		t.Errorf("PNG fixture is %dx%d, want 2x2", b.Dx(), b.Dy())
	}

	n, err := io.Copy(io.Discard, randomReader(testMaxUploadBytes+1))
	if err != nil || n != testMaxUploadBytes+1 {
		t.Errorf("random reader gave %d bytes, err %v", n, err)
	}
	if !bytes.HasPrefix(htmlPage(), []byte("<!DOCTYPE html>")) {
		t.Errorf("HTML fixture has no doctype")
	}
}

// The harness self-tests run this test binary as a stand-in for convertd:
// with fakeServerEnv set, init serves /readyz on HOST:PORT instead of running
// tests, which checks startServer's environment, readiness wait and stop.
const fakeServerEnv = "CONTRACT_SELFTEST_FAKE_SERVER"

func init() {
	switch os.Getenv(fakeServerEnv) {
	case "":
		return
	case "exit":
		fmt.Println("fake convertd: exiting at once")
		os.Exit(3)
	case "serve":
		os.Exit(runFakeServer())
	}
}

func runFakeServer() int {
	fmt.Printf("fake convertd: args=%v TEST_ENGINE=%s API_KEY=%s TMP_DIR=%t MAX_UPLOAD_BYTES=%s STRAY=%q\n",
		os.Args[1:], os.Getenv("TEST_ENGINE"), os.Getenv("API_KEY"), os.Getenv("TMP_DIR") != "",
		os.Getenv("MAX_UPLOAD_BYTES"), os.Getenv("CONTRACT_SELFTEST_STRAY"))
	mux := http.NewServeMux()
	mux.HandleFunc("GET /readyz", func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_, _ = io.WriteString(w, `{"status":"ready","pools":{"office":{"target":1,"idle":1,"busy":0},"render":{"target":1,"idle":1,"busy":0}}}`)
	})
	srv := &http.Server{Addr: os.Getenv("HOST") + ":" + os.Getenv("PORT"), Handler: mux, ReadHeaderTimeout: time.Second}
	stop := make(chan os.Signal, 1)
	signal.Notify(stop, syscall.SIGTERM)
	go func() {
		<-stop
		fmt.Println("fake convertd: SIGTERM")
		_ = srv.Close()
	}()
	if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
		fmt.Println("fake convertd:", err)
		return 1
	}
	return 0
}

func TestSelfTestHarnessStartsAndStops(t *testing.T) {
	t.Setenv("CONTRACT_SELFTEST_STRAY", "must-not-leak")
	s, err := startServer(os.Args[0], fakeServerEnv+"=serve")
	if err != nil {
		t.Fatal(err)
	}
	defer s.remove()

	rt, rec := selfTestTransport(t)
	makeCall(t, callReadiness, s.baseURL, rt)
	if got := rec.all(); len(got) != 0 {
		t.Errorf("readiness reported: %v", got)
	}

	if err := s.stop(); err != nil {
		t.Fatalf("stop: %v", err)
	}
	logs := s.logs()
	for _, want := range []string{
		"args=[serve]", "TEST_ENGINE=fake", "API_KEY=" + testAPIKey, "TMP_DIR=true",
		fmt.Sprintf("MAX_UPLOAD_BYTES=%d", testMaxUploadBytes), `STRAY=""`, "SIGTERM",
	} {
		if !strings.Contains(logs, want) {
			t.Errorf("server log lacks %q:\n%s", want, logs)
		}
	}
}

func TestSelfTestHarnessReportsEarlyExit(t *testing.T) {
	start := time.Now()
	_, err := startServer(os.Args[0], fakeServerEnv+"=exit")
	if err == nil {
		t.Fatal("startServer succeeded for a server that exits at once")
	}
	if !strings.Contains(err.Error(), "exited before it was ready") || !strings.Contains(err.Error(), "exiting at once") {
		t.Errorf("error does not explain the exit or carry the log: %v", err)
	}
	if time.Since(start) > 5*time.Second {
		t.Errorf("early exit took %s to notice", time.Since(start))
	}
}

func TestSelfTestServerEnvLastValueWins(t *testing.T) {
	env := serverEnv(1234, "/tmp/x", []string{"TEST_ENGINE=worker", "MAX_CONCURRENT_REQUESTS=1"})
	if env[len(env)-2] != "TEST_ENGINE=worker" || env[len(env)-1] != "MAX_CONCURRENT_REQUESTS=1" {
		t.Errorf("extra environment is not last: %v", env)
	}
}
