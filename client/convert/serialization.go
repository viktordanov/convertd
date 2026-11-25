package convert

import (
	"math"
	"net/http"
	"strconv"
	"time"

	"github.com/viktordanov/rs-convert/client/internal/api"
)

// The three values of the API's output option.
const (
	outputPDF    = "pdf"
	outputImages = "images"
	outputBundle = "bundle"
)

// Capabilities describes one deployment of the service.
type Capabilities struct {
	ServiceVersion string
	Engine         Engine
	Formats        []Format
	ImageFormats   []ImageFormat
	Limits         Limits
	Fonts          []string
}

// Format is one accepted input format.
type Format struct {
	Name       string // for example "docx"
	Family     string // "document", "presentation", "spreadsheet", "pdf" or "image"
	Engine     string // "libreoffice", "pdfium" or "image"
	Extensions []string
	MediaTypes []string
}

// Limits are the caps the service enforces.
type Limits struct {
	MaxUploadBytes int64
	MaxRenderPages int
	MaxDPI         int
	MaxEdgePx      int
	DefaultTimeout time.Duration
	MaxTimeout     time.Duration
}

// transformOptionsToRequest builds the options part. Zero fields are left out
// so that the server applies its defaults.
func transformOptionsToRequest(output string, o Options) api.ConvertOptions {
	kind := api.OutputKind(output)
	req := api.ConvertOptions{Output: &kind}

	if o.Pages != "" {
		req.Pages = &o.Pages
	}
	if o.Password != "" {
		req.Password = &o.Password
	}
	if o.Timeout > 0 {
		setInt(&req.TimeoutSeconds, int(math.Ceil(o.Timeout.Seconds())))
	}

	if o.Images != (ImageOptions{}) {
		images := api.ImageOptions{}
		if o.Images.Format != "" {
			format := api.ImageFormat(o.Images.Format)
			images.Format = &format
		}
		if o.Images.DPI != 0 {
			setInt(&images.Dpi, o.Images.DPI)
		}
		if o.Images.Quality != 0 {
			setInt(&images.Quality, o.Images.Quality)
		}
		if o.Images.MaxEdge != 0 {
			setInt(&images.MaxEdge, o.Images.MaxEdge)
		}
		req.Images = &images
	}

	if o.Slides != (SlideOptions{}) {
		slides := api.SlideOptions{}
		if o.Slides.IncludeHidden {
			slides.IncludeHidden = &o.Slides.IncludeHidden
		}
		if o.Slides.Notes != "" {
			notes := api.SlideNotes(o.Slides.Notes)
			slides.Notes = &notes
		}
		req.Slides = &slides
	}

	if o.Sheets.OnePagePerSheet {
		req.Sheets = &api.SheetOptions{OnePagePerSheet: &o.Sheets.OnePagePerSheet}
	}

	return req
}

func transformHeadersToInfo(h http.Header) Info {
	// A missing or malformed count header leaves the count at zero.
	pageCount, _ := strconv.Atoi(h.Get("Convert-Page-Count"))
	warningCount, _ := strconv.Atoi(h.Get("Convert-Warning-Count"))
	return Info{
		RequestID:    h.Get("X-Request-ID"),
		SourceFormat: h.Get("Convert-Source-Format"),
		PageCount:    pageCount,
		WarningCount: warningCount,
		Fingerprint:  h.Get("Convert-Engine-Fingerprint"),
	}
}

func transformManifestToResult(m api.Manifest) Manifest {
	result := Manifest{
		RequestID: m.RequestId,
		Source: Source{
			Filename:  deref(m.Source.Filename),
			Format:    m.Source.Format,
			Family:    string(m.Source.Family),
			SizeBytes: int64(m.Source.SizeBytes),
			SHA256:    m.Source.Sha256,
			Encrypted: m.Source.Encrypted,
		},
		PageCount: int(m.PageCount),
		Pages:     make([]Page, 0, len(m.Pages)),
		Warnings:  make([]Warning, 0, len(m.Warnings)),
		Engine:    transformEngineToResult(m.Engine),
		Timings: Timings{
			Queue:   milliseconds(m.TimingsMs.Queue),
			Convert: milliseconds(m.TimingsMs.Convert),
			Render:  milliseconds(m.TimingsMs.Render),
			Total:   milliseconds(m.TimingsMs.Total),
		},
	}

	if m.Pdf != nil {
		result.PDF = &PDFFile{
			Path:      m.Pdf.Path,
			SizeBytes: int64(m.Pdf.SizeBytes),
			Origin:    string(m.Pdf.Origin),
		}
	}

	for _, p := range m.Pages {
		result.Pages = append(result.Pages, Page{
			Number:    int(p.Number),
			Path:      p.Path,
			WidthPx:   int(p.WidthPx),
			HeightPx:  int(p.HeightPx),
			WidthPt:   float64(p.WidthPt),
			HeightPt:  float64(p.HeightPt),
			SizeBytes: int64(p.SizeBytes),
			Blank:     p.Blank,
		})
	}

	for _, w := range m.Warnings {
		result.Warnings = append(result.Warnings, Warning{
			Code:    string(w.Code),
			Message: w.Message,
			Page:    int(deref(w.Page)),
			Detail:  deref(w.Detail),
		})
	}

	return result
}

func transformEngineToResult(e api.EngineInfo) Engine {
	return Engine{
		ServiceVersion: e.ServiceVersion,
		LibreOffice:    deref(e.Libreoffice),
		PDFium:         e.Pdfium,
		Fingerprint:    e.Fingerprint,
	}
}

func transformCapabilitiesToResult(c api.Capabilities) *Capabilities {
	result := &Capabilities{
		ServiceVersion: c.Service.Version,
		Engine:         transformEngineToResult(c.Engine),
		Formats:        make([]Format, 0, len(c.Inputs)),
		ImageFormats:   make([]ImageFormat, 0, len(c.ImageFormats)),
		Limits: Limits{
			MaxUploadBytes: int64(c.Limits.MaxUploadBytes),
			MaxRenderPages: int(c.Limits.MaxRenderPages),
			MaxDPI:         int(c.Limits.MaxDpi),
			MaxEdgePx:      int(c.Limits.MaxEdgePx),
			DefaultTimeout: time.Duration(c.Limits.DefaultTimeoutSeconds) * time.Second,
			MaxTimeout:     time.Duration(c.Limits.MaxTimeoutSeconds) * time.Second,
		},
		Fonts: c.Fonts,
	}

	for _, in := range c.Inputs {
		result.Formats = append(result.Formats, Format{
			Name:       in.Format,
			Family:     string(in.Family),
			Engine:     string(in.Engine),
			Extensions: in.Extensions,
			MediaTypes: in.MediaTypes,
		})
	}
	for _, format := range c.ImageFormats {
		result.ImageFormats = append(result.ImageFormats, ImageFormat(format))
	}

	return result
}

func milliseconds[T ~int | ~int32 | ~int64](n T) time.Duration {
	return time.Duration(n) * time.Millisecond
}

// setInt points dst at a copy of v. The generated integer fields are int
// from the hand-written 3.0 spec and int32 or int64 from the emitted 3.1
// spec; T is inferred from dst, so the facade compiles against both.
func setInt[T ~int | ~int32 | ~int64](dst **T, v int) {
	x := T(v)
	*dst = &x
}

func deref[T any](p *T) T {
	if p == nil {
		var zero T
		return zero
	}
	return *p
}
