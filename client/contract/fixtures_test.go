package contract

import (
	"archive/zip"
	"bytes"
	"fmt"
	"hash/crc32"
	"image"
	"image/color"
	"image/png"
	"io"
	"math/rand/v2"
	"strings"
)

// The fixtures are built in code, so there are no binary files in git.

// onePagePDF returns a valid one-page PDF (US Letter, empty page) with a
// correct cross-reference table, about 400 bytes.
func onePagePDF() []byte {
	objects := []string{
		"<< /Type /Catalog /Pages 2 0 R >>",
		"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
		"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << >> /Contents 4 0 R >>",
		"<< /Length 0 >>\nstream\n\nendstream",
	}
	var b bytes.Buffer
	b.WriteString("%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
	offsets := make([]int, len(objects))
	for i, obj := range objects {
		offsets[i] = b.Len()
		fmt.Fprintf(&b, "%d 0 obj\n%s\nendobj\n", i+1, obj)
	}
	xref := b.Len()
	fmt.Fprintf(&b, "xref\n0 %d\n0000000000 65535 f \n", len(objects)+1)
	for _, off := range offsets {
		fmt.Fprintf(&b, "%010d 00000 n \n", off)
	}
	fmt.Fprintf(&b, "trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n", len(objects)+1, xref)
	return b.Bytes()
}

const (
	docxContentTypes = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>`
	docxRels = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>`
	docxDocumentHead = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>`
	docxDocumentTail = `</w:t></w:r></w:p></w:body></w:document>`
)

// docx returns a minimal Word document with one paragraph.
func docx() []byte {
	return docxWithText("Contract test document.")
}

// docxWithMarker returns a minimal Word document whose only paragraph is
// marker, for example "FAKE:DELAY_MS=3000". word/document.xml is stored
// without compression, so the marker appears verbatim in the upload, where
// the fake engine looks for it. Detection still sees a DOCX, so the request
// takes the office path and its queue.
func docxWithMarker(marker string) []byte {
	return docxWithText(marker)
}

func docxWithText(text string) []byte {
	var b bytes.Buffer
	zw := zip.NewWriter(&b)
	parts := []struct {
		name, body string
		method     uint16
	}{
		{"[Content_Types].xml", docxContentTypes, zip.Deflate},
		{"_rels/.rels", docxRels, zip.Deflate},
		{"word/document.xml", docxDocumentHead + text + docxDocumentTail, zip.Store},
	}
	for _, p := range parts {
		hdr := &zip.FileHeader{Name: p.name, Method: p.method}
		var w io.Writer
		var err error
		if p.method == zip.Store {
			// A stored part gets its sizes and CRC in the local header, with
			// no data descriptor: LibreOffice does not load a DOCX whose
			// stored entry is followed by one, which CreateHeader writes.
			hdr.CRC32 = crc32.ChecksumIEEE([]byte(p.body))
			hdr.CompressedSize64 = uint64(len(p.body))
			hdr.UncompressedSize64 = uint64(len(p.body))
			w, err = zw.CreateRaw(hdr)
		} else {
			w, err = zw.CreateHeader(hdr)
		}
		if err != nil {
			panic(fmt.Sprintf("failed to create docx part: %v", err))
		}
		if _, err := io.WriteString(w, p.body); err != nil {
			panic(fmt.Sprintf("failed to write docx part: %v", err))
		}
	}
	if err := zw.Close(); err != nil {
		panic(fmt.Sprintf("failed to finish docx: %v", err))
	}
	return b.Bytes()
}

// pngImage returns a 2x2 PNG.
func pngImage() []byte {
	img := image.NewRGBA(image.Rect(0, 0, 2, 2))
	img.Set(0, 0, color.RGBA{R: 255, A: 255})
	img.Set(1, 0, color.RGBA{G: 255, A: 255})
	img.Set(0, 1, color.RGBA{B: 255, A: 255})
	img.Set(1, 1, color.RGBA{A: 255})
	var b bytes.Buffer
	if err := png.Encode(&b, img); err != nil {
		panic(fmt.Sprintf("failed to encode png: %v", err))
	}
	return b.Bytes()
}

// htmlPage returns an HTML document, which the service does not accept.
func htmlPage() []byte {
	return []byte("<!DOCTYPE html>\n<html><head><title>t</title></head><body><p>Not a supported format.</p></body></html>\n")
}

// randomReader yields n pseudo-random bytes without holding them in memory,
// for uploads over the size cap.
func randomReader(n int64) io.Reader {
	return io.LimitReader(rand.NewChaCha8([32]byte{4}), n)
}

// containsMarker reports whether marker appears verbatim in b.
func containsMarker(b []byte, marker string) bool {
	return strings.Contains(string(b), marker)
}
