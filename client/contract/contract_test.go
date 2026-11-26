//go:build contract

package contract

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"mime/multipart"
	"net/http"
	"net/textproto"
	"os"
	"reflect"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/viktordanov/rs-convert/client/convert"
	"github.com/viktordanov/rs-convert/client/internal/api"
)

// The live contract cases (ledger 004 plan, Design section 5). Every response
// passes through the validating transport. Numbers match the plan's table.

func testContext(t *testing.T) context.Context {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	t.Cleanup(cancel)
	return ctx
}

// rawConvert posts a hand-built multipart body through the generated client,
// for requests the facade cannot express.
func rawConvert(t *testing.T, g *api.Client, parts map[string][]byte) *http.Response {
	t.Helper()
	var body bytes.Buffer
	w := multipart.NewWriter(&body)
	for _, name := range []string{"options", "file"} {
		data, ok := parts[name]
		if !ok {
			continue
		}
		h := textproto.MIMEHeader{}
		if name == "options" {
			h.Set("Content-Disposition", `form-data; name="options"`)
			h.Set("Content-Type", "application/json")
		} else {
			h.Set("Content-Disposition", `form-data; name="file"; filename="doc.pdf"`)
			h.Set("Content-Type", "application/octet-stream")
		}
		part, err := w.CreatePart(h)
		if err != nil {
			t.Fatalf("failed to create part: %v", err)
		}
		if _, err := part.Write(data); err != nil {
			t.Fatalf("failed to write part: %v", err)
		}
	}
	if err := w.Close(); err != nil {
		t.Fatalf("failed to close multipart: %v", err)
	}
	resp, err := g.ConvertDocumentWithBody(testContext(t), &api.ConvertDocumentParams{}, w.FormDataContentType(), &body)
	if err != nil {
		t.Fatalf("failed to send request: %v", err)
	}
	t.Cleanup(func() { _ = resp.Body.Close() })
	return resp
}

func errorCode(t *testing.T, resp *http.Response) string {
	t.Helper()
	var e api.Error
	if err := json.NewDecoder(resp.Body).Decode(&e); err != nil {
		t.Fatalf("failed to decode error body: %v", err)
	}
	return string(e.Code)
}

func wantErr(t *testing.T, err, sentinel error) {
	t.Helper()
	if !errors.Is(err, sentinel) {
		t.Fatalf("error = %v, want %v", err, sentinel)
	}
}

// Case 1.
func TestProbes(t *testing.T) {
	g := newGenerated(t)
	ctx := testContext(t)
	for name, call := range map[string]func(context.Context, ...api.RequestEditorFn) (*http.Response, error){
		"healthz": g.GetLiveness,
		"readyz":  g.GetReadiness,
	} {
		resp, err := call(ctx)
		if err != nil {
			t.Fatalf("%s: %v", name, err)
		}
		_ = resp.Body.Close()
		if resp.StatusCode != http.StatusOK {
			t.Errorf("%s: status %d, want 200", name, resp.StatusCode)
		}
	}
	ready, err := newFacade(t).Ready(ctx)
	if err != nil || !ready {
		t.Errorf("facade Ready = %t, %v; want true", ready, err)
	}
}

// Case 2.
func TestMetrics(t *testing.T) {
	resp, err := newGenerated(t).GetMetrics(testContext(t))
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK || mediaType(resp.Header.Get("Content-Type")) != "text/plain" {
		t.Errorf("status %d, content type %q; want 200 text/plain", resp.StatusCode, resp.Header.Get("Content-Type"))
	}
}

// Case 3.
func TestCapabilities(t *testing.T) {
	ctx := testContext(t)
	caps, err := newFacade(t).Capabilities(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if caps.ServiceVersion == "" || caps.Engine.Fingerprint == "" || len(caps.Formats) == 0 || len(caps.ImageFormats) == 0 {
		t.Errorf("capabilities incomplete: %+v", caps)
	}
	if caps.Limits.MaxUploadBytes <= 0 || caps.Limits.DefaultTimeout <= 0 || caps.Limits.MaxTimeout < caps.Limits.DefaultTimeout {
		t.Errorf("limits implausible: %+v", caps.Limits)
	}
	if target.mode == "binary" && caps.Limits.MaxUploadBytes != testMaxUploadBytes {
		t.Errorf("max upload = %d, want the harness's %d", caps.Limits.MaxUploadBytes, testMaxUploadBytes)
	}
	// Versions may be "unknown" and fonts empty in binary mode: there is no
	// build-info.json outside the image.

	resp, err := newGenerated(t).GetCapabilities(ctx, &api.GetCapabilitiesParams{})
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Errorf("generated: status %d, want 200", resp.StatusCode)
	}
}

// Case 4.
func TestUnauthorized(t *testing.T) {
	ctx := testContext(t)
	for _, key := range []string{"", "wrong-key"} {
		g := generatedFor(t, target.baseURL, key, newTransport(t))
		resp, err := g.GetCapabilities(ctx, &api.GetCapabilitiesParams{})
		if err != nil {
			t.Fatal(err)
		}
		if resp.StatusCode != http.StatusUnauthorized || errorCode(t, resp) != "UNAUTHORIZED" {
			t.Errorf("capabilities with key %q: status %d, want 401 UNAUTHORIZED", key, resp.StatusCode)
		}
		_ = resp.Body.Close()

		resp = rawConvert(t, g, map[string][]byte{"file": onePagePDF()})
		if resp.StatusCode != http.StatusUnauthorized {
			t.Errorf("convert with key %q: status %d, want 401", key, resp.StatusCode)
		}
	}

	f := facadeFor(t, target.baseURL, "wrong-key", newTransport(t))
	_, err := f.ConvertToPDF(ctx, "doc.pdf", bytes.NewReader(onePagePDF()), convert.Options{})
	wantErr(t, err, convert.ErrUnauthorized)
	_, err = f.Capabilities(ctx)
	wantErr(t, err, convert.ErrUnauthorized)
}

// Case 5.
func TestConvertPDFToPDF(t *testing.T) {
	const requestID = "contract-case-5"
	res, err := newFacade(t).ConvertToPDF(testContext(t), "doc.pdf", bytes.NewReader(onePagePDF()), convert.Options{RequestID: requestID})
	if err != nil {
		t.Fatal(err)
	}
	defer res.Body.Close()
	body, err := io.ReadAll(res.Body)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.HasPrefix(body, []byte("%PDF-")) {
		t.Errorf("body does not start with %%PDF-: %q", body[:min(len(body), 16)])
	}
	if res.Info.RequestID != requestID {
		t.Errorf("request ID = %q, want %q echoed", res.Info.RequestID, requestID)
	}
	if res.Info.SourceFormat != "pdf" || res.Info.PageCount < 1 || res.Info.Fingerprint == "" {
		t.Errorf("info = %+v", res.Info)
	}
}

// Case 6.
func TestConvertDOCXToBundle(t *testing.T) {
	ctx := testContext(t)
	f := newFacade(t)
	b, err := f.ConvertToBundle(ctx, "doc.docx", bytes.NewReader(docx()), convert.Options{})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	if b.Manifest.PDF == nil {
		t.Fatal("bundle manifest has no PDF")
	}
	if b.Info.RequestID != b.Manifest.RequestID {
		t.Errorf("header request ID %q, manifest %q", b.Info.RequestID, b.Manifest.RequestID)
	}
	openAll(t, b)

	t.Run("FAKE:PAGES=3", func(t *testing.T) {
		requireFake(t)
		b3, err := f.ConvertToBundle(ctx, "doc.docx", bytes.NewReader(docxWithMarker("FAKE:PAGES=3")), convert.Options{})
		if err != nil {
			t.Fatal(err)
		}
		defer b3.Close()
		if b3.Manifest.PageCount != 3 || len(b3.Manifest.Pages) != 3 {
			t.Errorf("page count %d, %d pages; want 3", b3.Manifest.PageCount, len(b3.Manifest.Pages))
		}
		openAll(t, b3)
	})
}

func openAll(t *testing.T, b *convert.Bundle) {
	t.Helper()
	if pdf, found, err := b.PDF(); err != nil {
		t.Errorf("PDF: %v", err)
	} else if found {
		_ = pdf.Close()
	}
	for _, p := range b.Manifest.Pages {
		r, found, err := b.Page(p.Number)
		if err != nil || !found {
			t.Errorf("page %d: found %t, %v", p.Number, found, err)
			continue
		}
		_ = r.Close()
	}
}

// Case 7. The input is a PDF: the owner removed image uploads (lane 001
// answers them with 415), so a PNG cannot reach output=images.
func TestConvertPDFToImages(t *testing.T) {
	ctx := testContext(t)
	opts := convert.Options{Images: convert.ImageOptions{Format: convert.PNG, DPI: 96, Quality: 70, MaxEdge: 640}}
	b, err := newFacade(t).ConvertToImages(ctx, "doc.pdf", bytes.NewReader(onePagePDF()), opts)
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	if b.Manifest.PDF != nil {
		t.Error("images bundle has a PDF")
	}
	if len(b.Manifest.Pages) == 0 {
		t.Error("images bundle has no pages")
	}
	openAll(t, b)

	// The facade does not expose the manifest's image settings, so read
	// them from the raw response.
	options, _ := json.Marshal(map[string]any{"output": "images", "images": map[string]any{
		"format": "png", "dpi": 96, "quality": 70, "max_edge": 640,
	}})
	resp := rawConvertFile(t, newGenerated(t), options, "doc.pdf", onePagePDF())
	raw, err := io.ReadAll(resp.Body)
	if err != nil {
		t.Fatal(err)
	}
	entries, err := readArchive(raw)
	if err != nil {
		t.Fatalf("not a tar.zst: %v", err)
	}
	if len(entries) == 0 || entries[0].name != "manifest.json" {
		t.Fatal("the archive does not start with manifest.json")
	}
	var m api.Manifest
	if err := json.Unmarshal(entries[0].data, &m); err != nil {
		t.Fatal(err)
	}
	if m.Images == nil || string(m.Images.Format) != "png" || int(m.Images.Dpi) != 96 {
		t.Errorf("manifest images = %+v, want png at 96 dpi", m.Images)
	}
}

func rawConvertFile(t *testing.T, g *api.Client, options []byte, filename string, file []byte) *http.Response {
	t.Helper()
	var body bytes.Buffer
	w := multipart.NewWriter(&body)
	h := textproto.MIMEHeader{}
	h.Set("Content-Disposition", `form-data; name="options"`)
	h.Set("Content-Type", "application/json")
	part, _ := w.CreatePart(h)
	_, _ = part.Write(options)
	fp, _ := w.CreateFormFile("file", filename)
	_, _ = fp.Write(file)
	_ = w.Close()
	resp, err := g.ConvertDocumentWithBody(testContext(t), &api.ConvertDocumentParams{}, w.FormDataContentType(), &body)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = resp.Body.Close() })
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d, want 200", resp.StatusCode)
	}
	return resp
}

// Case 8.
func TestUnknownOptionProperty(t *testing.T) {
	resp := rawConvert(t, newGenerated(t), map[string][]byte{
		"options": []byte(`{"output":"pdf","bogus":true}`),
		"file":    onePagePDF(),
	})
	if resp.StatusCode != http.StatusUnprocessableEntity || errorCode(t, resp) != "VALIDATION_ERROR" {
		t.Errorf("status %d, want 422 VALIDATION_ERROR", resp.StatusCode)
	}
}

// Case 9.
func TestOptionOutOfRange(t *testing.T) {
	_, err := newFacade(t).ConvertToPDF(testContext(t), "doc.pdf", bytes.NewReader(onePagePDF()),
		convert.Options{Images: convert.ImageOptions{DPI: 10}})
	wantErr(t, err, convert.ErrInvalidRequest)
}

// Case 10.
func TestMissingFilePart(t *testing.T) {
	resp := rawConvert(t, newGenerated(t), map[string][]byte{"options": []byte(`{"output":"pdf"}`)})
	if resp.StatusCode != http.StatusBadRequest || errorCode(t, resp) != "BAD_REQUEST" {
		t.Errorf("status %d, want 400 BAD_REQUEST", resp.StatusCode)
	}
}

// Case 11.
func TestUnsupportedFormat(t *testing.T) {
	_, err := newFacade(t).ConvertToPDF(testContext(t), "page.html", bytes.NewReader(htmlPage()), convert.Options{})
	wantErr(t, err, convert.ErrUnsupportedFormat)
}

// Image uploads left v1 (owner decision): a PNG gets 415, and capabilities
// lists no image input.
func TestImageUploadUnsupported(t *testing.T) {
	ctx := testContext(t)
	f := newFacade(t)
	_, err := f.ConvertToImages(ctx, "image.png", bytes.NewReader(pngImage()), convert.Options{})
	wantErr(t, err, convert.ErrUnsupportedFormat)
	if convert.Retryable(err) {
		t.Error("ErrUnsupportedFormat is retryable")
	}

	caps, err := f.Capabilities(ctx)
	if err != nil {
		t.Fatal(err)
	}
	for _, format := range caps.Formats {
		if format.Family == "image" || format.Engine == "image" {
			t.Errorf("capabilities lists the image input %+v", format)
		}
	}
}

// uploadLimit is the server's cap: the harness's in binary mode, the
// advertised one in image mode.
func uploadLimit(t *testing.T) int64 {
	t.Helper()
	if target.mode == "binary" {
		return testMaxUploadBytes
	}
	caps, err := newFacade(t).Capabilities(testContext(t))
	if err != nil {
		t.Fatal(err)
	}
	return caps.Limits.MaxUploadBytes
}

// Case 12.
func TestFileTooLarge(t *testing.T) {
	_, err := newFacade(t).ConvertToPDF(testContext(t), "big.pdf", randomReader(uploadLimit(t)+1), convert.Options{})
	wantErr(t, err, convert.ErrFileTooLarge)
}

// Case 13.
func TestFakeUnprocessableCodes(t *testing.T) {
	requireFake(t)
	f := newFacade(t)
	for code, sentinel := range map[string]error{
		"PASSWORD_REQUIRED": convert.ErrPasswordRequired,
		"INVALID_PASSWORD":  convert.ErrInvalidPassword,
		"LIMIT_EXCEEDED":    convert.ErrLimitExceeded,
		"CONVERSION_FAILED": convert.ErrConversionFailed,
	} {
		t.Run(code, func(t *testing.T) {
			_, err := f.ConvertToPDF(testContext(t), "doc.docx", bytes.NewReader(docxWithMarker("FAKE:"+code)), convert.Options{})
			wantErr(t, err, sentinel)
			if convert.Retryable(err) {
				t.Errorf("%s is retryable", code)
			}
		})
	}
}

// Case 14 runs at level B: see levelb_test.go.

// Case 15, level A: the 429 from the concurrency cap and from the marker.
func TestFakeBusy(t *testing.T) {
	requireFake(t)
	t.Parallel()

	t.Run("marker", func(t *testing.T) {
		_, err := newFacade(t).ConvertToPDF(testContext(t), "doc.docx", bytes.NewReader(docxWithMarker("FAKE:TOO_MANY_REQUESTS")), convert.Options{})
		wantErr(t, err, convert.ErrBusy)
		if !convert.Retryable(err) {
			t.Error("ErrBusy is not retryable")
		}
	})

	t.Run("MAX_CONCURRENT_REQUESTS=1", func(t *testing.T) {
		s := startTestServer(t, "MAX_CONCURRENT_REQUESTS=1")
		f := facadeFor(t, s.baseURL, testAPIKey, newTransport(t))
		ctx := testContext(t)

		var wg sync.WaitGroup
		var errA error
		wg.Add(1)
		go func() {
			defer wg.Done()
			res, err := f.ConvertToPDF(ctx, "a.docx", bytes.NewReader(docxWithMarker("FAKE:DELAY_MS=3000")), convert.Options{})
			if err == nil {
				_ = res.Body.Close()
			}
			errA = err
		}()
		time.Sleep(300 * time.Millisecond)

		_, errB := f.ConvertToPDF(ctx, "b.pdf", bytes.NewReader(onePagePDF()), convert.Options{})
		wantErr(t, errB, convert.ErrBusy)
		if !convert.Retryable(errB) {
			t.Error("ErrBusy is not retryable")
		}
		wg.Wait()
		if errA != nil {
			t.Errorf("request A: %v, want success", errA)
		}
	})
}

// Case 16.
func TestServedSpecIsCommittedSpec(t *testing.T) {
	req, err := http.NewRequestWithContext(testContext(t), http.MethodGet, target.baseURL+"/swagger/doc.json", nil)
	if err != nil {
		t.Fatal(err)
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d, want 200", resp.StatusCode)
	}
	var served, committed any
	if err := json.NewDecoder(resp.Body).Decode(&served); err != nil {
		t.Fatal(err)
	}
	data, err := os.ReadFile(committedSpec)
	if err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(data, &committed); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(served, committed) {
		t.Error("/swagger/doc.json differs from the committed api/openapi.json; run ./generate.sh")
	}
}

// Case 18.
func TestFakeTransientCodes(t *testing.T) {
	requireFake(t)
	f := newFacade(t)
	for code, sentinel := range map[string]error{
		"SERVICE_UNAVAILABLE": convert.ErrUnavailable,
		"INTERNAL_ERROR":      convert.ErrInternal,
	} {
		t.Run(code, func(t *testing.T) {
			_, err := f.ConvertToPDF(testContext(t), "doc.docx", bytes.NewReader(docxWithMarker("FAKE:"+code)), convert.Options{})
			wantErr(t, err, sentinel)
			if !convert.Retryable(err) {
				t.Errorf("%s is not retryable", code)
			}
		})
	}
}

// Case 19: the server answers while the client is still uploading. The body
// is far past socket buffer sizes, so the answer comes before the upload
// ends; the client must surface the answer, not a write error. The server
// drains the rest of an unread body after it answers (lane 001's lingering
// close); before that, about half the runs ended in "write: broken pipe".
func TestEarlyAnswerDuringUpload(t *testing.T) {
	t.Parallel()
	const size = 64 << 20
	ctx := testContext(t)

	t.Run("wrong key", func(t *testing.T) {
		f := facadeFor(t, target.baseURL, "wrong-key", newTransport(t))
		_, err := f.ConvertToPDF(ctx, "big.pdf", randomReader(size), convert.Options{})
		wantErr(t, err, convert.ErrUnauthorized)
	})
	t.Run("over the cap", func(t *testing.T) {
		// In image mode the cap is the deployed one (120 MiB by default), so
		// the upload must be larger than both.
		over := max(int64(size), uploadLimit(t)+1)
		_, err := newFacade(t).ConvertToPDF(ctx, "big.pdf", pdfPrefixed(randomReader(over)), convert.Options{})
		wantErr(t, err, convert.ErrFileTooLarge)
	})
}

// pdfPrefixed makes a large upload that detection would take for a PDF.
func pdfPrefixed(r io.Reader) io.Reader {
	return io.MultiReader(strings.NewReader("%PDF-1.4\n"), r)
}

// stallingReader sends head, then blocks for stall before it ends.
type stallingReader struct {
	head  []byte
	stall time.Duration
	sent  bool
}

func (s *stallingReader) Read(p []byte) (int, error) {
	if !s.sent {
		s.sent = true
		return copy(p, s.head), nil
	}
	time.Sleep(s.stall)
	return 0, io.EOF
}

// Case 20.
func TestStalledUpload(t *testing.T) {
	t.Parallel()
	s := startTestServer(t, "IO_IDLE_TIMEOUT_SECONDS=2")
	f := facadeFor(t, s.baseURL, testAPIKey, newTransport(t))
	head := append([]byte("%PDF-1.4\n"), bytes.Repeat([]byte("x"), 1024)...)

	start := time.Now()
	_, err := f.ConvertToPDF(testContext(t), "stall.pdf", &stallingReader{head: head, stall: 4 * time.Second}, convert.Options{})
	elapsed := time.Since(start)
	wantErr(t, err, convert.ErrInvalidRequest)
	if !strings.Contains(err.Error(), "stalled") {
		t.Errorf("error does not say the upload stalled: %v", err)
	}
	if elapsed > 3500*time.Millisecond {
		t.Errorf("answered after %s, want about 2-3 s: the client waited for the reader", elapsed)
	}
}
