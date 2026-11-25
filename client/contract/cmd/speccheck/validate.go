package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"regexp"
	"slices"
	"sort"
	"strconv"
	"strings"

	"github.com/getkin/kin-openapi/openapi3"
	"go.yaml.in/yaml/v4"
)

// pagesCases are the inputs the ConvertOptions.pages pattern must accept
// (true) or reject (false). Ported from the old scripts/check_spec.py.
var pagesCases = []struct {
	text string
	want bool
}{
	{"1", true},
	{"1-5,8,10-", true},
	{"1-", true},
	{"-3", false},
	{"a", false},
	{"1,,2", false},
	{"", false},
}

// errorCodeMention finds codes written as `UPPER_SNAKE` in prose.
var errorCodeMention = regexp.MustCompile("`([A-Z_]{6,})`")

// validateFile runs every check on one document. It returns the problems
// found; err is set only when the file cannot be read or parsed at all.
func validateFile(ctx context.Context, path string, out io.Writer) ([]string, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("failed to read spec: %w", err)
	}
	return validateData(ctx, data, out)
}

func validateData(ctx context.Context, data []byte, out io.Writer) ([]string, error) {
	var raw any
	if err := yaml.Unmarshal(data, &raw); err != nil {
		return nil, fmt.Errorf("failed to parse spec: %w", err)
	}
	root, ok := raw.(map[string]any)
	if !ok {
		return nil, errors.New("failed to parse spec: the document is not an object")
	}

	loader := openapi3.NewLoader()
	doc, err := loader.LoadFromData(data)
	if err != nil {
		return nil, fmt.Errorf("failed to load spec: %w", err)
	}

	fmt.Fprintf(out, "openapi: %s\n", doc.OpenAPI)
	fmt.Fprintf(out, "paths: %s\n", strings.Join(doc.Paths.InMatchingOrder(), ", "))
	fmt.Fprintf(out, "schemas: %d\n", len(doc.Components.Schemas))

	var findings []string
	findings = append(findings, structureFindings(ctx, doc)...)
	findings = append(findings, rawSchemaFindings(root)...)
	findings = append(findings, pagesPatternFindings(doc)...)
	findings = append(findings, errorCodeFindings(doc)...)
	return findings, nil
}

// structureFindings validates the document with kin-openapi. Version rules
// (3.0 or 3.1) follow the openapi field, and examples are validated against
// their schemas.
func structureFindings(ctx context.Context, doc *openapi3.T) []string {
	err := doc.Validate(ctx, openapi3.EnableMultiError())
	if err == nil {
		return nil
	}
	var multi openapi3.MultiError
	if !errors.As(err, &multi) {
		multi = openapi3.MultiError{err}
	}
	findings := make([]string, 0, len(multi))
	for _, e := range multi {
		findings = append(findings, "structure: "+firstLine(e.Error()))
	}
	return findings
}

// firstLine keeps the summary of a kin-openapi error, which names the place
// and the reason, and drops the schema and value dump that follows it.
func firstLine(s string) string {
	line, _, _ := strings.Cut(s, "\n")
	return line
}

// rawSchemaFindings walks the parsed document as a tree, not as text, so prose
// in descriptions cannot trigger it. It visits every schema object: component
// schemas, every "schema" of a parameter, header or media type, and every
// subschema below those.
func rawSchemaFindings(root map[string]any) []string {
	w := &schemaWalker{}
	w.walkDocument("", root)
	sort.Strings(w.findings)
	return w.findings
}

type schemaWalker struct {
	findings []string
}

func (w *schemaWalker) addf(path, format string, args ...any) {
	w.findings = append(w.findings, path+": "+fmt.Sprintf(format, args...))
}

// walkDocument walks the parts of the document that are not schemas, looking
// for the places where schemas start.
func (w *schemaWalker) walkDocument(path string, node any) {
	switch v := node.(type) {
	case map[string]any:
		for _, key := range sortedKeys(v) {
			child := joinPath(path, key)
			switch {
			case key == "example" || key == "examples":
				// Example payloads are data, not schemas.
			case key == "schema":
				w.walkSchema(child, v[key])
			case key == "schemas" && path == "components":
				if schemas, ok := v[key].(map[string]any); ok {
					for _, name := range sortedKeys(schemas) {
						w.walkSchema(joinPath(child, name), schemas[name])
					}
				}
			default:
				w.walkDocument(child, v[key])
			}
		}
	case []any:
		for i, item := range v {
			w.walkDocument(path+"["+strconv.Itoa(i)+"]", item)
		}
	}
}

// Keywords whose value is one subschema, a list of them, or a map of them.
var (
	subschemaKeys     = []string{"items", "not", "additionalProperties", "contains", "if", "then", "else", "propertyNames", "unevaluatedItems", "unevaluatedProperties"}
	subschemaListKeys = []string{"allOf", "oneOf", "anyOf", "prefixItems"}
	subschemaMapKeys  = []string{"properties", "patternProperties", "$defs", "definitions", "dependentSchemas"}
)

func (w *schemaWalker) walkSchema(path string, node any) {
	schema, ok := node.(map[string]any)
	if !ok {
		// additionalProperties: true/false and boolean schemas carry no types.
		return
	}

	types := schemaTypes(schema)
	if slices.Contains(types, "null") {
		w.addf(joinPath(path, "type"), `type includes "null"; an Option<T> field needs #[schema(nullable = false)]`)
	}
	if nullable, _ := schema["nullable"].(bool); nullable {
		w.addf(joinPath(path, "nullable"), "nullable: true is not allowed; an Option<T> field needs #[schema(nullable = false)]")
	}
	if enum, ok := schema["enum"].([]any); ok && slices.Contains(enum, nil) {
		w.addf(joinPath(path, "enum"), "enum includes null")
	}
	if slices.Contains(types, "array") {
		if items, ok := schema["items"].(map[string]any); ok {
			if slices.Contains(schemaTypes(items), "integer") {
				w.addf(joinPath(path, "items"), "array of integers; a Vec<u8> body needs #[schema(value_type = String, format = Binary)]")
			}
		}
	}

	for _, key := range subschemaKeys {
		if sub, ok := schema[key]; ok {
			w.walkSchema(joinPath(path, key), sub)
		}
	}
	for _, key := range subschemaListKeys {
		if list, ok := schema[key].([]any); ok {
			for i, sub := range list {
				w.walkSchema(joinPath(path, key)+"["+strconv.Itoa(i)+"]", sub)
			}
		}
	}
	for _, key := range subschemaMapKeys {
		if m, ok := schema[key].(map[string]any); ok {
			for _, name := range sortedKeys(m) {
				w.walkSchema(joinPath(joinPath(path, key), name), m[name])
			}
		}
	}
}

// schemaTypes returns the type keyword as a list: a scalar becomes one item.
func schemaTypes(schema map[string]any) []string {
	switch t := schema["type"].(type) {
	case string:
		return []string{t}
	case nil:
		if _, present := schema["type"]; present {
			// type: null in YAML parses as a nil value.
			return []string{"null"}
		}
		return nil
	case []any:
		types := make([]string, 0, len(t))
		for _, item := range t {
			switch s := item.(type) {
			case string:
				types = append(types, s)
			case nil:
				types = append(types, "null")
			}
		}
		return types
	default:
		return nil
	}
}

// pagesPatternFindings checks that the pages pattern accepts and rejects the
// expected inputs. Go's regexp search has JSON Schema's semantics: the
// pattern is not anchored unless it anchors itself.
func pagesPatternFindings(doc *openapi3.T) []string {
	const path = "components.schemas.ConvertOptions.properties.pages.pattern"
	options := doc.Components.Schemas["ConvertOptions"]
	if options == nil || options.Value == nil {
		return []string{"components.schemas.ConvertOptions: schema is missing"}
	}
	pages := options.Value.Properties["pages"]
	if pages == nil || pages.Value == nil || pages.Value.Pattern == "" {
		return []string{path + ": pattern is missing"}
	}
	re, err := regexp.Compile(pages.Value.Pattern)
	if err != nil {
		return []string{fmt.Sprintf("%s: pattern does not compile: %v", path, err)}
	}
	var findings []string
	for _, c := range pagesCases {
		if got := re.MatchString(c.text); got != c.want {
			findings = append(findings, fmt.Sprintf("%s: %q matched=%t, want %t", path, c.text, got, c.want))
		}
	}
	return findings
}

// errorCodeFindings checks that every code named in info.description is a
// value of the error code enum. The enum is read from Error.code, so it is
// found whether it is a named ErrorCode schema or inline.
func errorCodeFindings(doc *openapi3.T) []string {
	errSchema := doc.Components.Schemas["Error"]
	if errSchema == nil || errSchema.Value == nil {
		return []string{"components.schemas.Error: schema is missing"}
	}
	enum := enumOf(errSchema.Value.Properties["code"])
	if len(enum) == 0 {
		return []string{"components.schemas.Error.properties.code: enum is missing"}
	}
	known := make(map[string]bool, len(enum))
	for _, v := range enum {
		if s, ok := v.(string); ok {
			known[s] = true
		}
	}

	if doc.Info == nil {
		return []string{"info: missing"}
	}
	var missing []string
	for _, m := range errorCodeMention.FindAllStringSubmatch(doc.Info.Description, -1) {
		if !known[m[1]] && !slices.Contains(missing, m[1]) {
			missing = append(missing, m[1])
		}
	}
	sort.Strings(missing)
	findings := make([]string, 0, len(missing))
	for _, m := range missing {
		findings = append(findings, fmt.Sprintf("info.description: names %s, which is not in the error code enum", m))
	}
	return findings
}

// enumOf returns the enum of a schema, looking through a single-member allOf,
// which is how a $ref with sibling keywords is written in OpenAPI 3.0.
func enumOf(ref *openapi3.SchemaRef) []any {
	if ref == nil || ref.Value == nil {
		return nil
	}
	if len(ref.Value.Enum) > 0 {
		return ref.Value.Enum
	}
	if len(ref.Value.AllOf) == 1 {
		return enumOf(ref.Value.AllOf[0])
	}
	return nil
}

func joinPath(path, key string) string {
	if path == "" {
		return key
	}
	return path + "." + key
}

func sortedKeys(m map[string]any) []string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	return keys
}
