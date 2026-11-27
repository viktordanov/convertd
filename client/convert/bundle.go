package convert

import (
	"archive/tar"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"math"
	"os"
	"regexp"
	"sort"
	"strings"
	"time"

	"github.com/klauspost/compress/zstd"

	"github.com/viktordanov/rs-convert/client/internal/api"
)

// The names an archive may hold. manifest.json comes first; document.pdf
// only in a bundle; then one image per rendered page.
const (
	manifestPath = "manifest.json"
	pdfPath      = "document.pdf"
	pagesDir     = "pages"
	// manifestLimit caps the size of manifest.json.
	manifestLimit = 16 << 20
	// tarRecordSize is the default tar record of 20 blocks of 512 bytes. A
	// tar writer pads an archive to whole records, so at most one record of
	// zeros follows the end-of-archive marker.
	tarRecordSize = 20 * 512
	// maxWindow is the largest zstd window the decoder accepts.
	maxWindow = 8 << 20
)

var pagePath = regexp.MustCompile(`^pages/[0-9]{4,}\.(jpg|png)$`)

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
// unpacked archive in a temp directory until Close.
type Bundle struct {
	Manifest Manifest
	Info     Info

	dir  string
	root *os.Root
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
	// Origin is "converted" or "passthrough".
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
	// Hidden is true when the page comes from a slide marked hidden. It is
	// set only with Slides.IncludeHidden, and only when the service could
	// map the slides to the pages exactly.
	Hidden bool
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

// Close releases the bundle and deletes its temp directory. It can be
// called more than once.
func (b *Bundle) Close() error {
	var closeErr error
	if b.root != nil {
		closeErr = b.root.Close()
		b.root = nil
	}
	if err := os.RemoveAll(b.dir); err != nil {
		return fmt.Errorf("failed to remove bundle directory: %w", err)
	}
	if closeErr != nil {
		return fmt.Errorf("failed to close bundle directory: %w", closeErr)
	}
	return nil
}

func (b *Bundle) open(path string) (io.ReadCloser, bool, error) {
	if b.root == nil {
		return nil, false, errors.New("failed to open bundle entry: the bundle is closed")
	}
	entry, err := b.root.Open(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, false, nil
	}
	if err != nil {
		return nil, false, fmt.Errorf("failed to open bundle entry: %w", err)
	}
	return entry, true, nil
}

// readBundle unpacks the tar.zst response into a temp directory and reads its
// manifest. The archive is a stream, so it is read once, front to back; the
// directory gives PDF and Page random access afterwards.
func readBundle(body io.Reader, tempDir string, maxBytes int64) (*Bundle, error) {
	dir, err := os.MkdirTemp(tempDir, "convert-bundle-*")
	if err != nil {
		return nil, fmt.Errorf("failed to create temp directory: %w", err)
	}
	root, err := os.OpenRoot(dir)
	if err != nil {
		_ = os.RemoveAll(dir)
		return nil, fmt.Errorf("failed to open temp directory: %w", err)
	}
	bundle := &Bundle{dir: dir, root: root}

	if err := bundle.load(body, maxBytes); err != nil {
		_ = bundle.Close()
		return nil, err
	}
	return bundle, nil
}

// load reads the archive. It accepts only what the service writes: a
// regular manifest.json first, then exactly the regular files the manifest
// names, each under an expected name, each once and each with the size the
// manifest gives, which its tar header must declare before anything is
// written. With maxBytes above 0, the manifest's sizes may add up to at most
// that. After the tar end it allows only zero padding, then
// requires the end of the zstd stream, so that a frame cut after the tar
// entries, a bad checksum or trailing data fails too.
func (b *Bundle) load(body io.Reader, maxBytes int64) error {
	// The service writes zstd level 1, whose window is far below 8 MiB; a
	// larger window would only let a hostile frame claim more memory.
	zr, err := zstd.NewReader(body, zstd.WithDecoderConcurrency(1), zstd.WithDecoderMaxWindow(maxWindow))
	if err != nil {
		return fmt.Errorf("failed to start zstd decoder: %w", err)
	}
	defer zr.Close()
	tr := tar.NewReader(zr)

	hdr, err := tr.Next()
	if errors.Is(err, io.EOF) {
		return errors.New("failed to read archive: it is empty")
	}
	if err != nil {
		return fmt.Errorf("failed to read archive: %w", err)
	}
	if hdr.Name != manifestPath || hdr.Typeflag != tar.TypeReg {
		return fmt.Errorf("failed to read archive: the first entry is %q, want the file %s", hdr.Name, manifestPath)
	}
	raw, err := io.ReadAll(io.LimitReader(tr, manifestLimit+1))
	if err != nil {
		return fmt.Errorf("failed to read manifest: %w", err)
	}
	if len(raw) > manifestLimit {
		return fmt.Errorf("failed to read manifest: it is larger than %d bytes", manifestLimit)
	}
	var manifest api.Manifest
	if err := json.Unmarshal(raw, &manifest); err != nil {
		return fmt.Errorf("failed to decode manifest: %w", err)
	}
	b.Manifest = transformManifestToResult(manifest)

	expected, err := expectedEntries(b.Manifest)
	if err != nil {
		return err
	}
	if maxBytes > 0 {
		if total, ok := sumSizes(expected); !ok || total > maxBytes {
			size := fmt.Sprintf("%d bytes", total)
			if !ok {
				size = "more bytes than an int64 holds"
			}
			return fmt.Errorf("failed to read archive: the manifest's files take %s, more than the %d that WithMaxBundleBytes allows", size, maxBytes)
		}
	}
	seen := map[string]bool{}
	for {
		hdr, err := tr.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return fmt.Errorf("failed to read archive: %w", err)
		}
		name := hdr.Name
		if err := checkEntryName(name); err != nil {
			return err
		}
		switch {
		case hdr.Typeflag != tar.TypeReg:
			return fmt.Errorf("failed to read archive: entry %q is not a regular file", name)
		case !hasKey(expected, name):
			return fmt.Errorf("failed to read archive: entry %q is not in the manifest", name)
		case seen[name]:
			return fmt.Errorf("failed to read archive: entry %q appears twice", name)
		}
		seen[name] = true
		// The header gives the size before any byte is written: a file
		// larger than the manifest says never reaches the disk.
		if want := expected[name]; hdr.Size != want {
			return fmt.Errorf("failed to read archive: entry %q has %d bytes, the manifest says %d", name, hdr.Size, want)
		}
		n, err := b.write(name, io.LimitReader(tr, expected[name]))
		if err != nil {
			return err
		}
		if want := expected[name]; n != want {
			return fmt.Errorf("failed to read archive: entry %q has %d bytes, the manifest says %d", name, n, want)
		}
	}
	for _, name := range sortedKeys(expected) {
		if !seen[name] {
			return fmt.Errorf("failed to read archive: the manifest names %q, which is not in the archive", name)
		}
	}
	return checkArchiveEnd(zr)
}

// checkArchiveEnd reads what follows the tar end-of-archive marker. A tar
// writer may pad the archive with zeros to a whole record, so at most
// tarRecordSize zero bytes are allowed (the service writes none). Then the
// decoder must report io.EOF, which it does only after the end of the frame
// and its checksum. Anything else, including data compressed to a few bytes
// that would expand without bound, fails before it is all decompressed.
func checkArchiveEnd(zr io.Reader) error {
	rest, err := io.ReadAll(io.LimitReader(zr, tarRecordSize+1))
	if err != nil {
		return fmt.Errorf("failed to read the end of the archive: %w", err)
	}
	if len(rest) > tarRecordSize {
		return fmt.Errorf("failed to read the end of the archive: more than %d bytes follow the tar end", tarRecordSize)
	}
	for _, c := range rest {
		if c != 0 {
			return errors.New("failed to read the end of the archive: non-zero data follows the tar end")
		}
	}
	var one [1]byte
	if n, err := zr.Read(one[:]); n != 0 || !errors.Is(err, io.EOF) {
		return fmt.Errorf("failed to read the end of the archive: the stream does not end after the tar end (%d bytes, %v)", n, err)
	}
	return nil
}

// write unpacks one entry and returns its size.
func (b *Bundle) write(name string, r io.Reader) (int64, error) {
	if strings.HasPrefix(name, pagesDir+"/") {
		if err := b.root.Mkdir(pagesDir, 0o700); err != nil && !errors.Is(err, fs.ErrExist) {
			return 0, fmt.Errorf("failed to create pages directory: %w", err)
		}
	}
	f, err := b.root.OpenFile(name, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		return 0, fmt.Errorf("failed to create bundle entry: %w", err)
	}
	n, err := io.Copy(f, r)
	if err != nil {
		_ = f.Close()
		return 0, fmt.Errorf("failed to unpack bundle entry: %w", err)
	}
	if err := f.Close(); err != nil {
		return 0, fmt.Errorf("failed to unpack bundle entry: %w", err)
	}
	return n, nil
}

// checkEntryName rejects a name that could leave the bundle directory or that
// the service never writes.
func checkEntryName(name string) error {
	for _, part := range strings.Split(name, "/") {
		if part == ".." {
			return fmt.Errorf("failed to read archive: entry %q has a path with ..", name)
		}
	}
	if name != pdfPath && !pagePath.MatchString(name) {
		return fmt.Errorf("failed to read archive: unexpected entry %q", name)
	}
	return nil
}

// expectedEntries returns the files the manifest names, with their sizes.
// Each must have a name the service writes, so a manifest cannot point
// outside the bundle.
func expectedEntries(m Manifest) (map[string]int64, error) {
	expected := map[string]int64{}
	if m.PDF != nil {
		if m.PDF.Path != pdfPath {
			return nil, fmt.Errorf("failed to read manifest: the PDF path is %q, want %s", m.PDF.Path, pdfPath)
		}
		expected[pdfPath] = m.PDF.SizeBytes
	}
	for _, p := range m.Pages {
		if err := checkEntryName(p.Path); err != nil || p.Path == pdfPath {
			return nil, fmt.Errorf("failed to read manifest: page %d has the path %q", p.Number, p.Path)
		}
		// Each path once: a second size for the same file would be lost
		// from the total that WithMaxBundleBytes checks.
		if _, dup := expected[p.Path]; dup {
			return nil, fmt.Errorf("failed to read manifest: page %d repeats the path %q", p.Number, p.Path)
		}
		expected[p.Path] = p.SizeBytes
	}
	return expected, nil
}

func hasKey[V any](m map[string]V, key string) bool {
	_, ok := m[key]
	return ok
}

func sortedKeys[V any](m map[string]V) []string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	return keys
}

// sumSizes adds the sizes; false when a size is negative or the sum would
// not fit in an int64.
func sumSizes(sizes map[string]int64) (int64, bool) {
	var total int64
	for _, size := range sizes {
		if size < 0 || total > math.MaxInt64-size {
			return 0, false
		}
		total += size
	}
	return total, true
}
