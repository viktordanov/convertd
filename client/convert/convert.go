package convert

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"mime/multipart"
	"net/http"
	"net/textproto"
	"time"

	"github.com/viktordanov/rs-convert/client/internal/api"
)

// ImageFormat is the encoding of rendered pages.
type ImageFormat string

const (
	JPEG ImageFormat = "jpeg"
	PNG  ImageFormat = "png"
	WebP ImageFormat = "webp"
)

// SlideNotes says what to do with speaker notes.
type SlideNotes string

const (
	NotesNone   SlideNotes = "none"
	NotesAppend SlideNotes = "append"
	NotesOnly   SlideNotes = "only"
)

// Options controls one conversion. The zero value asks for the server defaults.
type Options struct {
	// Pages selects which pages are rendered as images, for example "1-5,8,10-".
	// Empty means all pages. The PDF always holds every page.
	Pages string
	// Password opens an encrypted input.
	Password string
	// Timeout is the whole budget for the call on the server. Zero uses the
	// server default. It is rounded up to whole seconds.
	Timeout time.Duration
	// RequestID is sent as X-Request-ID. Empty lets the server mint one.
	RequestID string

	Images ImageOptions
	Slides SlideOptions
	Sheets SheetOptions
}

// ImageOptions controls how pages are rendered. Zero fields use the server defaults.
type ImageOptions struct {
	Format  ImageFormat // default JPEG
	DPI     int         // default 150
	Quality int         // default 85; JPEG and WebP only
	MaxEdge int         // longest edge in pixels; 0 means no clamp
}

// SlideOptions only affects presentations.
type SlideOptions struct {
	IncludeHidden bool
	Notes         SlideNotes // default NotesNone
}

// SheetOptions only affects spreadsheets.
type SheetOptions struct {
	OnePagePerSheet bool
}

// ConvertToPDF converts file to a PDF and streams it back.
//
// The caller must close PDF.Body. The body is streamed, so it can be copied
// straight to blob storage without holding the PDF in memory.
func (c *Client) ConvertToPDF(ctx context.Context, filename string, file io.Reader, opts Options) (*PDF, error) {
	resp, err := c.convert(ctx, outputPDF, filename, file, opts)
	if err != nil {
		return nil, fmt.Errorf("failed to convert document: %w", err)
	}
	return &PDF{Body: resp.Body, Info: transformHeadersToInfo(resp.Header)}, nil
}

// ConvertToBundle converts file and returns the PDF, the page images and the
// manifest. The caller must call Bundle.Close.
func (c *Client) ConvertToBundle(ctx context.Context, filename string, file io.Reader, opts Options) (*Bundle, error) {
	return c.convertToBundle(ctx, outputBundle, filename, file, opts)
}

// ConvertToImages converts file and returns the page images and the manifest,
// without the PDF. The caller must call Bundle.Close.
func (c *Client) ConvertToImages(ctx context.Context, filename string, file io.Reader, opts Options) (*Bundle, error) {
	return c.convertToBundle(ctx, outputImages, filename, file, opts)
}

func (c *Client) convertToBundle(ctx context.Context, output, filename string, file io.Reader, opts Options) (*Bundle, error) {
	resp, err := c.convert(ctx, output, filename, file, opts)
	if err != nil {
		return nil, fmt.Errorf("failed to convert document: %w", err)
	}
	defer resp.Body.Close()

	bundle, err := readBundle(resp.Body, c.tempDir)
	if err != nil {
		return nil, fmt.Errorf("failed to read bundle: %w", err)
	}
	bundle.Info = transformHeadersToInfo(resp.Header)
	return bundle, nil
}

// convert sends the request and returns the response of a successful
// conversion. Any other status becomes an error wrapping a sentinel.
func (c *Client) convert(ctx context.Context, output, filename string, file io.Reader, opts Options) (*http.Response, error) {
	optionsJSON, err := json.Marshal(transformOptionsToRequest(output, opts))
	if err != nil {
		return nil, fmt.Errorf("failed to encode options: %w", err)
	}

	// The body is written through a pipe so that the file is streamed to the
	// service and never held in memory.
	pipeReader, pipeWriter := io.Pipe()
	form := multipart.NewWriter(pipeWriter)
	go func() {
		pipeWriter.CloseWithError(writeForm(form, filename, file, optionsJSON))
	}()
	// Unblocks the writer goroutine if the request ends before the body is read.
	defer pipeReader.Close()

	params := &api.ConvertDocumentParams{}
	if opts.RequestID != "" {
		// Assigned as a plain string: api.RequestId is an alias of string
		// while the parameter is a component, and gone when it is inline.
		requestID := opts.RequestID
		params.XRequestID = &requestID
	}

	resp, err := c.api.ConvertDocumentWithBody(ctx, params, form.FormDataContentType(), pipeReader)
	if err != nil {
		return nil, fmt.Errorf("failed to send request: %w", err)
	}
	if resp.StatusCode != http.StatusOK {
		defer resp.Body.Close()
		return nil, transformErrorResponse(resp)
	}
	return resp, nil
}

// writeForm writes the two multipart parts. Options go first, so the service
// can reject a bad request before it has received the whole file.
func writeForm(form *multipart.Writer, filename string, file io.Reader, optionsJSON []byte) error {
	header := textproto.MIMEHeader{}
	header.Set("Content-Disposition", `form-data; name="options"`)
	header.Set("Content-Type", "application/json")
	optionsPart, err := form.CreatePart(header)
	if err != nil {
		return fmt.Errorf("failed to create options part: %w", err)
	}
	if _, err := optionsPart.Write(optionsJSON); err != nil {
		return fmt.Errorf("failed to write options part: %w", err)
	}

	filePart, err := form.CreateFormFile("file", filename)
	if err != nil {
		return fmt.Errorf("failed to create file part: %w", err)
	}
	if _, err := io.Copy(filePart, file); err != nil {
		return fmt.Errorf("failed to write file part: %w", err)
	}

	if err := form.Close(); err != nil {
		return fmt.Errorf("failed to close form: %w", err)
	}
	return nil
}
