package convert

import (
	"archive/zip"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"time"

	"github.com/viktordanov/rs-convert/client/internal/api"
)

const manifestPath = "manifest.json"

// Info is the metadata the service sends in response headers.
type Info struct {
	RequestID    string
	SourceFormat string
	PageCount    int
	WarningCount int
	// Fingerprint identifies the exact engine build. Use it in cache keys.
	Fingerprint string
}

// PDF is the result of ConvertToPDF.
type PDF struct {
	// Body is the PDF, streamed from the service. The caller must close it.
	Body io.ReadCloser
	Info Info
}

// Bundle is the result of ConvertToBundle and ConvertToImages. It holds the
// downloaded ZIP in a temp file until Close.
type Bundle struct {
	Manifest Manifest
	Info     Info

	file *os.File
	zip  *zip.Reader
}

// Manifest describes a bundle: the source, the PDF, each rendered page and
// anything the service noticed during conversion.
type Manifest struct {
	RequestID string
	Source    Source
	// PageCount is the number of pages in the PDF, rendered or not.
	PageCount int
	// PDF is nil when the bundle holds images only.
	PDF      *PDFFile
	Pages    []Page
	Warnings []Warning
	Engine   Engine
	Timings  Timings
}

// Source describes the uploaded file.
type Source struct {
	Filename  string
	Format    string
	Family    string
	SizeBytes int64
	SHA256    string
	Encrypted bool
}

// PDFFile describes the PDF inside a bundle.
type PDFFile struct {
	Path      string
	SizeBytes int64
	// Origin is "converted", "passthrough" or "assembled".
	Origin string
}

// Page describes one rendered page.
type Page struct {
	Number    int // 1-based page number in the PDF
	Path      string
	WidthPx   int
	HeightPx  int
	WidthPt   float64
	HeightPt  float64
	SizeBytes int64
	Blank     bool
}

// Warning is a quality signal. Warnings never fail a conversion.
type Warning struct {
	// Code is one of the warning codes in the API spec, such as "FONT_SUBSTITUTED".
	Code    string
	Message string
	// Page is the page the warning is about, or 0 when it concerns the whole document.
	Page   int
	Detail map[string]any
}

// Engine identifies the software that produced a result.
type Engine struct {
	ServiceVersion string
	LibreOffice    string // empty when LibreOffice was not used
	PDFium         string
	Fingerprint    string
}

// Timings says where the time went on the server.
type Timings struct {
	Queue   time.Duration
	Convert time.Duration
	Render  time.Duration
	Total   time.Duration
}

// PDF opens the PDF in the bundle. The bool is false when the bundle holds
// images only, which is not an error. The caller must close the reader.
func (b *Bundle) PDF() (io.ReadCloser, bool, error) {
	if b.Manifest.PDF == nil {
		return nil, false, nil
	}
	return b.open(b.Manifest.PDF.Path)
}

// Page opens the image of one page by its 1-based number. The bool is false
// when that page was not rendered, which is not an error. The caller must
// close the reader.
func (b *Bundle) Page(number int) (io.ReadCloser, bool, error) {
	for _, page := range b.Manifest.Pages {
		if page.Number == number {
			return b.open(page.Path)
		}
	}
	return nil, false, nil
}

// Close releases the bundle and deletes its temp file.
func (b *Bundle) Close() error {
	name := b.file.Name()
	closeErr := b.file.Close()
	if err := os.Remove(name); err != nil && !errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("failed to remove bundle file: %w", err)
	}
	if closeErr != nil && !errors.Is(closeErr, os.ErrClosed) {
		return fmt.Errorf("failed to close bundle file: %w", closeErr)
	}
	return nil
}

func (b *Bundle) open(path string) (io.ReadCloser, bool, error) {
	entry, err := b.zip.Open(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, false, nil
	}
	if err != nil {
		return nil, false, fmt.Errorf("failed to open bundle entry: %w", err)
	}
	return entry, true, nil
}

// readBundle downloads the ZIP into a temp file and reads its manifest. ZIP
// needs random access, so the response cannot be read as a stream.
func readBundle(body io.Reader, tempDir string) (*Bundle, error) {
	file, err := os.CreateTemp(tempDir, "convert-bundle-*.zip")
	if err != nil {
		return nil, fmt.Errorf("failed to create temp file: %w", err)
	}
	bundle := &Bundle{file: file}

	if err := bundle.load(body); err != nil {
		_ = bundle.Close()
		return nil, err
	}
	return bundle, nil
}

func (b *Bundle) load(body io.Reader) error {
	size, err := io.Copy(b.file, body)
	if err != nil {
		return fmt.Errorf("failed to download bundle: %w", err)
	}

	b.zip, err = zip.NewReader(b.file, size)
	if err != nil {
		return fmt.Errorf("failed to open bundle: %w", err)
	}

	entry, err := b.zip.Open(manifestPath)
	if err != nil {
		return fmt.Errorf("failed to open manifest: %w", err)
	}
	defer entry.Close()

	var manifest api.Manifest
	if err := json.NewDecoder(entry).Decode(&manifest); err != nil {
		return fmt.Errorf("failed to decode manifest: %w", err)
	}
	b.Manifest = transformManifestToResult(manifest)
	return nil
}
