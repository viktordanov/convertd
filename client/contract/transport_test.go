package contract

import (
	"archive/tar"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"mime"
	"net/http"
	"slices"
	"sort"
	"strconv"
	"strings"
	"sync"
	"testing"

	"github.com/getkin/kin-openapi/openapi3"
	"github.com/getkin/kin-openapi/openapi3filter"
	"github.com/getkin/kin-openapi/routers"
	"github.com/getkin/kin-openapi/routers/legacy"
	"github.com/klauspost/compress/zstd"
)

// committedSpec is the spec every response is checked against: the one
// convertd emits, committed by generate.sh.
const committedSpec = "../../api/openapi.json"

func init() {
	// kin-openapi has no decoder for these; the body is a string/binary.
	openapi3filter.RegisterBodyDecoder("application/pdf", openapi3filter.FileBodyDecoder)
	openapi3filter.RegisterBodyDecoder("application/zstd", openapi3filter.FileBodyDecoder)
}

// contractSpec is the loaded spec with a router that matches any host.
type contractSpec struct {
	doc      *openapi3.T
	router   routers.Router
	manifest *openapi3.Schema
}

var (
	specOnce   sync.Once
	loadedSpec *contractSpec
	specErr    error
)

// theSpec loads committedSpec once per test binary.
func theSpec() (*contractSpec, error) {
	specOnce.Do(func() { loadedSpec, specErr = loadContractSpec(committedSpec) })
	return loadedSpec, specErr
}

func loadContractSpec(path string) (*contractSpec, error) {
	doc, err := openapi3.NewLoader().LoadFromFile(path)
	if err != nil {
		return nil, fmt.Errorf("failed to load spec: %w", err)
	}
	// With no servers the router matches on the path alone, so the spec's
	// localhost:8080 does not stop it matching a test server's address.
	doc.Servers = nil
	router, err := legacy.NewRouter(doc)
	if err != nil {
		return nil, fmt.Errorf("failed to build router: %w", err)
	}
	manifest := doc.Components.Schemas["Manifest"]
	if manifest == nil || manifest.Value == nil {
		return nil, errors.New("failed to load spec: components.schemas.Manifest is missing")
	}
	return &contractSpec{doc: doc, router: router, manifest: manifest.Value}, nil
}

// declaredPairs lists every (operationId, status) the spec declares.
func (s *contractSpec) declaredPairs() []observation {
	var pairs []observation
	for _, path := range s.doc.Paths.InMatchingOrder() {
		for _, op := range s.doc.Paths.Value(path).Operations() {
			for status := range op.Responses.Map() {
				pairs = append(pairs, observation{operationID: op.OperationID, status: status})
			}
		}
	}
	sortObservations(pairs)
	return pairs
}

// observation is one (operationId, status) seen by a validating transport.
type observation struct {
	operationID string
	status      string
}

func sortObservations(o []observation) {
	sort.Slice(o, func(i, j int) bool {
		if o[i].operationID != o[j].operationID {
			return o[i].operationID < o[j].operationID
		}
		return o[i].status < o[j].status
	})
}

// coverage collects observations across every test in the binary.
var coverage = struct {
	sync.Mutex
	seen map[observation]bool
}{seen: map[observation]bool{}}

func recordObservation(o observation) {
	coverage.Lock()
	defer coverage.Unlock()
	coverage.seen[o] = true
}

// unobserved returns the declared pairs no transport has seen.
func unobserved(s *contractSpec) []observation {
	coverage.Lock()
	defer coverage.Unlock()
	var out []observation
	for _, p := range s.declaredPairs() {
		if !coverage.seen[p] {
			out = append(out, p)
		}
	}
	return out
}

// reporter receives the violations a transport finds. *testing.T is one; the
// self-tests use a recorder.
type reporter interface {
	Errorf(format string, args ...any)
}

// validatingTransport checks every response against the spec. Requests pass
// through unchanged and are not buffered, so large uploads stream as they do
// in production. Each response is buffered, validated and handed back with
// its body intact. Any violation is reported to the test that made the call.
type validatingTransport struct {
	base   http.RoundTripper
	spec   *contractSpec
	report reporter
	record bool // add observations to the coverage report
}

// newTransport returns a validating transport that reports to t and counts
// towards the coverage report.
func newTransport(t testing.TB) *validatingTransport {
	t.Helper()
	s, err := theSpec()
	if err != nil {
		t.Fatalf("failed to load the committed spec: %v", err)
	}
	return &validatingTransport{base: http.DefaultTransport, spec: s, report: t, record: true}
}

func (v *validatingTransport) RoundTrip(req *http.Request) (*http.Response, error) {
	resp, err := v.base.RoundTrip(req)
	if err != nil {
		return resp, err
	}
	body, err := io.ReadAll(resp.Body)
	_ = resp.Body.Close()
	if err != nil {
		return nil, fmt.Errorf("failed to read response body: %w", err)
	}
	resp.Body = io.NopCloser(bytes.NewReader(body))

	op, problems := v.spec.check(req, resp, body)
	if v.record && op != "" {
		recordObservation(observation{operationID: op, status: strconv.Itoa(resp.StatusCode)})
	}
	for _, p := range problems {
		v.report.Errorf("contract: %s %s -> %s %d: %s", req.Method, req.URL.Path, orUnknown(op), resp.StatusCode, p)
	}
	return resp, nil
}

func orUnknown(op string) string {
	if op == "" {
		return "(no operation)"
	}
	return op
}

// check validates one response. It returns the operationId it matched and
// every problem found.
func (s *contractSpec) check(req *http.Request, resp *http.Response, body []byte) (string, []string) {
	route, pathParams, err := s.router.FindRoute(req)
	if err != nil {
		return "", []string{"no operation in the spec matches the request: " + err.Error()}
	}
	op := route.Operation.OperationID

	var problems []string
	input := &openapi3filter.ResponseValidationInput{
		RequestValidationInput: &openapi3filter.RequestValidationInput{
			Request:    req,
			PathParams: pathParams,
			Route:      route,
		},
		Status:  resp.StatusCode,
		Header:  resp.Header,
		Body:    io.NopCloser(bytes.NewReader(body)),
		Options: &openapi3filter.Options{IncludeResponseStatus: true, MultiError: true},
	}
	if err := openapi3filter.ValidateResponse(context.Background(), input); err != nil {
		problems = append(problems, describe(err)...)
	}

	// kin-openapi reports a missing header only when it is required, and
	// neither the reference nor utoipa sets required on response headers.
	declared := route.Operation.Responses.Status(resp.StatusCode)
	if declared == nil {
		declared = route.Operation.Responses.Default()
	}
	if declared != nil && declared.Value != nil {
		names := make([]string, 0, len(declared.Value.Headers))
		for name := range declared.Value.Headers {
			names = append(names, name)
		}
		sort.Strings(names)
		for _, name := range names {
			if len(resp.Header.Values(name)) == 0 {
				problems = append(problems, fmt.Sprintf("declared header %s is missing", name))
			}
		}
	}

	if mediaType(resp.Header.Get("Content-Type")) == "application/zstd" && resp.StatusCode == http.StatusOK {
		if cd := resp.Header.Get("Content-Disposition"); !strings.Contains(cd, `.tar.zst"`) {
			problems = append(problems, fmt.Sprintf("bundle: Content-Disposition %q does not name a .tar.zst file", cd))
		}
		problems = append(problems, s.checkBundle(body)...)
	}
	return op, problems
}

// archiveEntry is one file of a tar.zst bundle.
type archiveEntry struct {
	name    string
	regular bool
	data    []byte
}

// readArchive reads a whole tar.zst stream.
func readArchive(body []byte) ([]archiveEntry, error) {
	zr, err := zstd.NewReader(bytes.NewReader(body), zstd.WithDecoderConcurrency(1))
	if err != nil {
		return nil, err
	}
	defer zr.Close()
	tr := tar.NewReader(zr)
	var entries []archiveEntry
	for {
		hdr, err := tr.Next()
		if errors.Is(err, io.EOF) {
			return entries, nil
		}
		if err != nil {
			return nil, err
		}
		data, err := io.ReadAll(tr)
		if err != nil {
			return nil, err
		}
		entries = append(entries, archiveEntry{name: hdr.Name, regular: hdr.Typeflag == tar.TypeReg, data: data})
	}
}

// checkBundle validates manifest.json against components.schemas.Manifest and
// checks the archive layout the spec describes: regular files only,
// manifest.json first, then document.pdf when the manifest names it, then
// each page in manifest order, and nothing else. An archive entry is not an
// HTTP body, so ValidateResponse cannot see it.
func (s *contractSpec) checkBundle(body []byte) []string {
	entries, err := readArchive(body)
	if err != nil {
		return []string{"bundle: not a valid tar.zst: " + err.Error()}
	}
	if len(entries) == 0 {
		return []string{"bundle: the archive is empty"}
	}
	var problems []string
	if entries[0].name != "manifest.json" {
		problems = append(problems, fmt.Sprintf("bundle: the first entry is %s, not manifest.json", entries[0].name))
	}
	var raw []byte
	got := make([]string, 0, len(entries))
	for _, e := range entries {
		if !e.regular {
			problems = append(problems, fmt.Sprintf("bundle: %s is not a regular file", e.name))
		}
		if e.name == "manifest.json" && raw == nil {
			raw = e.data
		}
		got = append(got, e.name)
	}
	if raw == nil {
		return append(problems, "bundle: manifest.json is missing")
	}
	var manifest any
	if err := json.Unmarshal(raw, &manifest); err != nil {
		return append(problems, "bundle: manifest.json is not JSON: "+err.Error())
	}

	opts := []openapi3.SchemaValidationOption{openapi3.MultiErrors()}
	if s.doc.IsOpenAPI31OrLater() {
		opts = append(opts, openapi3.EnableJSONSchema2020())
	}
	if err := s.doc.ValidateSchemaJSON(s.manifest, manifest, opts...); err != nil {
		for _, p := range describe(err) {
			problems = append(problems, "bundle: manifest.json: "+p)
		}
	}

	want := []string{"manifest.json"}
	m, _ := manifest.(map[string]any)
	if pdf, ok := m["pdf"].(map[string]any); ok {
		if p, ok := pdf["path"].(string); ok {
			want = append(want, p)
		}
	}
	pages, _ := m["pages"].([]any)
	for _, page := range pages {
		if pm, ok := page.(map[string]any); ok {
			if p, ok := pm["path"].(string); ok {
				want = append(want, p)
			}
		}
	}
	inArchive := map[string]bool{}
	for _, name := range got {
		inArchive[name] = true
	}
	named := map[string]bool{}
	for _, name := range want {
		named[name] = true
		if !inArchive[name] {
			problems = append(problems, fmt.Sprintf("bundle: manifest names %s, which is not in the archive", name))
		}
	}
	for _, name := range got {
		if !named[name] {
			problems = append(problems, fmt.Sprintf("bundle: %s is in the archive but not in the manifest", name))
		}
	}
	if len(problems) == 0 && !slices.Equal(got, want) {
		problems = append(problems, fmt.Sprintf("bundle: entries are in the order %v, want %v", got, want))
	}
	return problems
}

// describe turns a kin-openapi error into one line per problem, each naming
// the JSON path of the value where there is one.
func describe(err error) []string {
	switch e := err.(type) {
	case openapi3.MultiError:
		var out []string
		for _, sub := range e {
			out = append(out, describe(sub)...)
		}
		return out
	case *openapi3filter.ResponseError:
		if e.Err == nil {
			return []string{e.Reason}
		}
		subs := describe(e.Err)
		if e.Reason == "" {
			return subs
		}
		for i, sub := range subs {
			subs[i] = e.Reason + ": " + sub
		}
		return subs
	case *openapi3.SchemaError:
		return []string{fmt.Sprintf("at %q: %s", "/"+strings.Join(e.JSONPointer(), "/"), oneLine(e.Reason))}
	default:
		return []string{firstLine(err.Error())}
	}
}

// oneLine joins a multi-line reason, as the JSON Schema 2020-12 validator
// writes them, into one line.
func oneLine(s string) string {
	var parts []string
	for _, line := range strings.Split(s, "\n") {
		if line = strings.TrimSpace(line); line != "" {
			parts = append(parts, line)
		}
	}
	return strings.Join(parts, "; ")
}

func firstLine(s string) string {
	line, _, _ := strings.Cut(s, "\n")
	return line
}

func mediaType(contentType string) string {
	mt, _, err := mime.ParseMediaType(contentType)
	if err != nil {
		return contentType
	}
	return mt
}
