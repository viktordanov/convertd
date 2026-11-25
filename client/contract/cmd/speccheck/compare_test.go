package main

import (
	"context"
	"encoding/json"
	"os"
	"slices"
	"strconv"
	"strings"
	"testing"

	"go.yaml.in/yaml/v4"
)

// The compare tests start from a pair that must compare equal: a part of the
// reference (testdata/reference-part.yaml, OpenAPI 3.0.3) and a hand-made
// translation of it in the form utoipa emits (testdata/emitted-part.json,
// OpenAPI 3.1). Each case changes one side and lists the exact differences.

const (
	referencePart = "testdata/reference-part.yaml"
	emittedPart   = "testdata/emitted-part.json"
)

func loadTree(t *testing.T, path string) map[string]any {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("failed to read %s: %v", path, err)
	}
	var tree map[string]any
	if err := yaml.Unmarshal(data, &tree); err != nil {
		t.Fatalf("failed to parse %s: %v", path, err)
	}
	return tree
}

// at walks a tree by map keys and list indexes.
func at(t *testing.T, tree any, path ...string) map[string]any {
	t.Helper()
	cur := tree
	for _, key := range path {
		switch v := cur.(type) {
		case map[string]any:
			cur = v[key]
		case []any:
			i, err := strconv.Atoi(key)
			if err != nil || i >= len(v) {
				t.Fatalf("bad index %q in %v", key, path)
			}
			cur = v[i]
		default:
			t.Fatalf("path %v ends early at %q", path, key)
		}
	}
	m, ok := cur.(map[string]any)
	if !ok {
		t.Fatalf("path %v is not an object", path)
	}
	return m
}

func specOf(t *testing.T, tree map[string]any) *spec {
	t.Helper()
	data, err := json.Marshal(tree)
	if err != nil {
		t.Fatalf("failed to encode tree: %v", err)
	}
	s, err := parseSpec(context.Background(), data)
	if err != nil {
		t.Fatalf("failed to load tree: %v", err)
	}
	return s
}

func schema(t *testing.T, tree map[string]any, name string, prop ...string) map[string]any {
	t.Helper()
	path := []string{"components", "schemas", name}
	for _, p := range prop {
		path = append(path, "properties", p)
	}
	return at(t, tree, path...)
}

func TestCompareSelf(t *testing.T) {
	for _, path := range []string{"../../../../api/openapi.yaml", referencePart, emittedPart} {
		s := specOf(t, loadTree(t, path))
		if diffs, extra := compareSpecs(s, s); len(diffs)+len(extra) != 0 {
			t.Errorf("%s against itself: %v %v", path, diffs, extra)
		}
	}
}

func TestCompareHandTranslationIsEqual(t *testing.T) {
	e, r := specOf(t, loadTree(t, emittedPart)), specOf(t, loadTree(t, referencePart))
	if diffs, extra := compareSpecs(e, r); len(diffs)+len(extra) != 0 {
		t.Fatalf("differences:\n  %s\nextra: %v", strings.Join(diffs, "\n  "), extra)
	}
}

// TestCompareNormalisations covers each tolerated difference. Each case
// rewrites the emitted side into another accepted form, so the pair must
// still compare equal.
func TestCompareNormalisations(t *testing.T) {
	cases := []struct {
		name   string
		mutate func(t *testing.T, e, r map[string]any)
	}{
		{"openapi version is ignored", func(t *testing.T, e, r map[string]any) {
			e["openapi"] = "3.1.1"
		}},
		{"allOf with one $ref and siblings equals $ref with siblings", func(t *testing.T, e, r map[string]any) {
			schema(t, e, "ConvertOptions")["properties"].(map[string]any)["output"] = map[string]any{
				"allOf": []any{map[string]any{"$ref": "#/components/schemas/OutputKind"}}, "default": "pdf",
			}
		}},
		{"allOf with one $ref and no siblings equals the $ref", func(t *testing.T, e, r map[string]any) {
			schema(t, e, "ConvertOptions")["properties"].(map[string]any)["images"] = map[string]any{
				"allOf": []any{map[string]any{"$ref": "#/components/schemas/ImageOptions"}},
			}
		}},
		{"component responses, parameters and headers equal inline ones", func(t *testing.T, e, r map[string]any) {
			// The emitted side is inline; make the reference inline too and
			// the emitted side use components, the reverse of the fixtures.
			comps := at(t, e, "components")
			comps["responses"] = map[string]any{"Unauthorized": at(t, e, "paths", "/v1/capabilities", "get", "responses", "401")}
			comps["parameters"] = map[string]any{"RequestId": at(t, e, "paths", "/v1/capabilities", "get", "parameters", "0")}
			comps["headers"] = map[string]any{"Retry-After": at(t, e, "paths", "/v1/convert", "post", "responses", "429", "headers", "Retry-After")}
			at(t, e, "paths", "/v1/capabilities", "get", "responses")["401"] = map[string]any{"$ref": "#/components/responses/Unauthorized"}
			at(t, e, "paths", "/v1/capabilities", "get")["parameters"] = []any{map[string]any{"$ref": "#/components/parameters/RequestId"}}
			at(t, e, "paths", "/v1/convert", "post", "responses", "429", "headers")["Retry-After"] = map[string]any{"$ref": "#/components/headers/Retry-After"}
		}},
		{"a named enum equals an inline enum with the same values", func(t *testing.T, e, r map[string]any) {
			schema(t, e, "Capabilities", "outputs")["items"] = map[string]any{"type": "string", "enum": []any{"bundle", "pdf", "images"}}
		}},
		{"integer format is ignored", func(t *testing.T, e, r map[string]any) {
			schema(t, e, "ImageOptions", "dpi")["format"] = "int64"
			delete(schema(t, e, "ImageOptions", "quality"), "format")
		}},
		{"number format is ignored", func(t *testing.T, e, r map[string]any) {
			schema(t, e, "PageInfo", "width_pt")["format"] = "double"
		}},
		{"minimum 0 on an emitted integer is ignored when the reference has none", func(t *testing.T, e, r map[string]any) {
			if _, has := schema(t, r, "Limits", "max_dpi")["minimum"]; has {
				t.Fatal("fixture changed: reference Limits.max_dpi has a minimum")
			}
			schema(t, e, "Limits", "max_dpi")["minimum"] = 0
		}},
		{"a type array of one equals the scalar, and null is ignored", func(t *testing.T, e, r map[string]any) {
			schema(t, e, "ServiceInfo", "name")["type"] = []any{"string"}
			schema(t, e, "ServiceInfo", "commit")["type"] = []any{"string", "null"}
		}},
		{"header names compare case-insensitively", func(t *testing.T, e, r map[string]any) {
			h := at(t, e, "paths", "/v1/convert", "post", "responses", "200", "headers")
			h["server-timing"] = h["Server-Timing"]
			delete(h, "Server-Timing")
		}},
		{"security compares by scheme type, not name", func(t *testing.T, e, r map[string]any) {
			at(t, e, "components")["securitySchemes"] = map[string]any{"token": map[string]any{"type": "http", "scheme": "Bearer"}}
			e["security"] = []any{map[string]any{"token": []any{}}}
		}},
		{"descriptions, examples and extensions are not compared", func(t *testing.T, e, r map[string]any) {
			s := schema(t, e, "PageInfo", "number")
			s["description"] = "changed"
			s["example"] = 7
			s["x-rust-type"] = "u32"
			at(t, e, "info")["description"] = "changed"
		}},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			e, r := loadTree(t, emittedPart), loadTree(t, referencePart)
			tc.mutate(t, e, r)
			if diffs, _ := compareSpecs(specOf(t, e), specOf(t, r)); len(diffs) != 0 {
				t.Fatalf("differences:\n  %s", strings.Join(diffs, "\n  "))
			}
		})
	}
}

// TestCompareRules covers each must-match rule with one change and the exact
// lines it must produce.
func TestCompareRules(t *testing.T) {
	cases := []struct {
		name   string
		mutate func(t *testing.T, e, r map[string]any)
		want   []string
	}{
		{"info.version", func(t *testing.T, e, r map[string]any) {
			at(t, e, "info")["version"] = "1.0.0"
		}, []string{`info.version: "1.0.0-draft.1" in reference, "1.0.0" in emitted`}},
		{"operation missing", func(t *testing.T, e, r map[string]any) {
			delete(at(t, e, "paths"), "/healthz")
		}, []string{"GET /healthz: operation missing in emitted"}},
		{"operation only in emitted", func(t *testing.T, e, r map[string]any) {
			at(t, e, "paths")["/swagger/doc.json"] = map[string]any{"get": map[string]any{
				"operationId": "spec", "tags": []any{"operations"}, "security": []any{},
				"responses": map[string]any{"200": map[string]any{"description": "ok"}},
			}}
		}, []string{"GET /swagger/doc.json: operation only in emitted"}},
		{"operationId", func(t *testing.T, e, r map[string]any) {
			at(t, e, "paths", "/healthz", "get")["operationId"] = "healthz"
		}, []string{`GET /healthz: operationId "getLiveness" in reference, "healthz" in emitted`}},
		{"tags", func(t *testing.T, e, r map[string]any) {
			at(t, e, "paths", "/healthz", "get")["tags"] = []any{"probes"}
		}, []string{"GET /healthz: tags [operations] in reference, [probes] in emitted"}},
		{"security", func(t *testing.T, e, r map[string]any) {
			delete(at(t, e, "paths", "/healthz", "get"), "security")
		}, []string{"GET /healthz: security none in reference, http bearer in emitted"}},
		{"parameter missing", func(t *testing.T, e, r map[string]any) {
			delete(at(t, e, "paths", "/v1/capabilities", "get"), "parameters")
		}, []string{"GET /v1/capabilities: parameter header x-request-id missing in emitted"}},
		{"parameter required", func(t *testing.T, e, r map[string]any) {
			at(t, e, "paths", "/v1/capabilities", "get", "parameters", "0")["required"] = true
		}, []string{"GET /v1/capabilities: parameter header x-request-id: required false in reference, true in emitted"}},
		{"parameter schema", func(t *testing.T, e, r map[string]any) {
			delete(at(t, e, "paths", "/v1/capabilities", "get", "parameters", "0", "schema"), "maxLength")
		}, []string{"GET /v1/capabilities parameter header x-request-id: maxLength 128 in reference, none in emitted"}},
		{"request body required", func(t *testing.T, e, r map[string]any) {
			at(t, e, "paths", "/v1/convert", "post", "requestBody")["required"] = false
		}, []string{"POST /v1/convert: request body: required true in reference, false in emitted"}},
		{"request content type", func(t *testing.T, e, r map[string]any) {
			c := at(t, e, "paths", "/v1/convert", "post", "requestBody", "content")
			c["application/json"] = c["multipart/form-data"]
			delete(c, "multipart/form-data")
		}, []string{
			"POST /v1/convert request: content type multipart/form-data missing in emitted",
			"POST /v1/convert request: content type application/json only in emitted",
		}},
		{"request part encoding", func(t *testing.T, e, r map[string]any) {
			at(t, e, "paths", "/v1/convert", "post", "requestBody", "content", "multipart/form-data", "encoding", "options")["contentType"] = "text/plain"
		}, []string{"POST /v1/convert request multipart/form-data: encoding.options.contentType application/json in reference, text/plain in emitted"}},
		{"response status missing", func(t *testing.T, e, r map[string]any) {
			delete(at(t, e, "paths", "/v1/convert", "post", "responses"), "504")
		}, []string{"POST /v1/convert: response 504 missing in emitted"}},
		{"response content type missing", func(t *testing.T, e, r map[string]any) {
			delete(at(t, e, "paths", "/v1/convert", "post", "responses", "200", "content"), "application/zstd")
		}, []string{"POST /v1/convert 200: content type application/zstd missing in emitted"}},
		{"response schema", func(t *testing.T, e, r map[string]any) {
			at(t, e, "paths", "/readyz", "get", "responses", "503", "content", "application/json")["schema"] = map[string]any{"$ref": "#/components/schemas/Health"}
		}, []string{"GET /readyz 503 application/json: $ref Readiness in reference, Health in emitted"}},
		{"response header missing", func(t *testing.T, e, r map[string]any) {
			delete(at(t, e, "paths", "/v1/convert", "post", "responses", "200", "headers"), "Server-Timing")
		}, []string{"POST /v1/convert 200: header server-timing missing in emitted"}},
		{"schema renamed", func(t *testing.T, e, r map[string]any) {
			schemas := at(t, e, "components", "schemas")
			schemas["ErrorBody"] = schemas["Error"]
			delete(schemas, "Error")
			renameRefs(e, "#/components/schemas/Error", "#/components/schemas/ErrorBody")
		}, append([]string{"schemas.Error: missing in emitted"}, errorUses("$ref Error in reference, ErrorBody in emitted")...)},
		{"property missing", func(t *testing.T, e, r map[string]any) {
			delete(schema(t, e, "ServiceInfo")["properties"].(map[string]any), "commit")
		}, []string{"schemas.ServiceInfo: property commit missing in emitted"}},
		{"property only in emitted", func(t *testing.T, e, r map[string]any) {
			schema(t, e, "ServiceInfo")["properties"].(map[string]any)["extra"] = map[string]any{"type": "string"}
		}, []string{"schemas.ServiceInfo: property extra only in emitted"}},
		{"required set", func(t *testing.T, e, r map[string]any) {
			schema(t, e, "ServiceInfo")["required"] = []any{"name"}
		}, []string{"schemas.ServiceInfo: required [name, version] in reference, [name] in emitted"}},
		{"additionalProperties false missing", func(t *testing.T, e, r map[string]any) {
			delete(schema(t, e, "ImageOptions"), "additionalProperties")
		}, []string{"schemas.ImageOptions: additionalProperties false in reference, any in emitted"}},
		{"additionalProperties false only in emitted", func(t *testing.T, e, r map[string]any) {
			schema(t, e, "ServiceInfo")["additionalProperties"] = false
		}, []string{"schemas.ServiceInfo: additionalProperties any in reference, false in emitted"}},
		{"enum values", func(t *testing.T, e, r map[string]any) {
			schema(t, e, "ImageFormat")["enum"] = []any{"jpeg", "png"}
		}, []string{`schemas.ImageFormat: enum ["jpeg", "png", "webp"] in reference, ["jpeg", "png"] in emitted`}},
		{"inline enum values", func(t *testing.T, e, r map[string]any) {
			schema(t, e, "InputFormat", "family")["enum"] = []any{"document"}
		}, []string{`schemas.InputFormat.family: enum ["document", "image", "pdf", "presentation", "spreadsheet"] in reference, ["document"] in emitted`}},
		{"items", func(t *testing.T, e, r map[string]any) {
			schema(t, e, "Capabilities", "fonts")["items"] = map[string]any{"type": "integer"}
		}, []string{"schemas.Capabilities.fonts[]: type [string] in reference, [integer] in emitted"}},
		{"writeOnly", func(t *testing.T, e, r map[string]any) {
			delete(schema(t, e, "ConvertOptions", "password"), "writeOnly")
		}, []string{"schemas.ConvertOptions.password: writeOnly true in reference, false in emitted"}},
		{"string format", func(t *testing.T, e, r map[string]any) {
			delete(schema(t, e, "ConvertRequest", "file"), "format")
		}, []string{"schemas.ConvertRequest.file: format binary in reference, none in emitted"}},
		{"int64 must stay int64", func(t *testing.T, e, r map[string]any) {
			schema(t, e, "Limits", "max_upload_bytes")["format"] = "int32"
		}, []string{"schemas.Limits.max_upload_bytes: format int64 in reference, int32 in emitted"}},
		{"$ref target", func(t *testing.T, e, r map[string]any) {
			schema(t, e, "Error", "code")["$ref"] = "#/components/schemas/OutputKind"
		}, []string{"schemas.Error.code: $ref ErrorCode in reference, OutputKind in emitted"}},
		{"minimum missing", func(t *testing.T, e, r map[string]any) {
			delete(schema(t, e, "PageInfo", "number"), "minimum")
		}, []string{"schemas.PageInfo.number: minimum 1 in reference, none in emitted"}},
		{"minimum 0 where the reference has 1", func(t *testing.T, e, r map[string]any) {
			schema(t, e, "PageInfo", "number")["minimum"] = 0
		}, []string{"schemas.PageInfo.number: minimum 1 in reference, 0 in emitted"}},
		{"maximum differs", func(t *testing.T, e, r map[string]any) {
			schema(t, e, "ImageOptions", "dpi")["maximum"] = 1200
		}, []string{"schemas.ImageOptions.dpi: maximum 600 in reference, 1200 in emitted"}},
		{"maxLength missing", func(t *testing.T, e, r map[string]any) {
			delete(schema(t, e, "ConvertOptions", "pages"), "maxLength")
		}, []string{"schemas.ConvertOptions.pages: maxLength 256 in reference, none in emitted"}},
		{"pattern differs", func(t *testing.T, e, r map[string]any) {
			schema(t, e, "ConvertOptions", "pages")["pattern"] = "^[0-9,-]+$"
		}, []string{`schemas.ConvertOptions.pages: pattern "^[0-9]+(-[0-9]*)?(,[0-9]+(-[0-9]*)?)*$" in reference, "^[0-9,-]+$" in emitted`}},
		{"default beside a $ref", func(t *testing.T, e, r map[string]any) {
			schema(t, e, "ConvertOptions", "output")["default"] = "bundle"
		}, []string{`schemas.ConvertOptions.output: default "pdf" in reference, "bundle" in emitted`}},
		{"default missing", func(t *testing.T, e, r map[string]any) {
			delete(schema(t, e, "ImageOptions", "quality"), "default")
		}, []string{"schemas.ImageOptions.quality: default 85 in reference, none in emitted"}},
		{"extra constraint in emitted", func(t *testing.T, e, r map[string]any) {
			schema(t, e, "ConvertOptions", "pages")["minLength"] = 1
		}, []string{"schemas.ConvertOptions.pages: minLength none in reference, 1 in emitted"}},
		{"a keyword no rule knows is compared as a value", func(t *testing.T, e, r map[string]any) {
			schema(t, e, "ConvertRequest", "file")["contentMediaType"] = "application/octet-stream"
		}, []string{`schemas.ConvertRequest.file: contentMediaType none in reference, "application/octet-stream" in emitted`}},
		{"oneOf where the reference has a plain schema", func(t *testing.T, e, r map[string]any) {
			schema(t, e, "Readiness")["properties"].(map[string]any)["status"] = map[string]any{
				"oneOf": []any{
					map[string]any{"type": "string", "enum": []any{"ready", "starting"}},
					map[string]any{"type": "string", "enum": []any{"draining", "degraded"}},
				},
			}
		}, []string{
			"schemas.Readiness.status: type [string] in reference, none in emitted",
			`schemas.Readiness.status: enum ["degraded", "draining", "ready", "starting"] in reference, none in emitted`,
			"schemas.Readiness.status: oneOf with 0 member(s) in reference, 2 in emitted",
		}},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			e, r := loadTree(t, emittedPart), loadTree(t, referencePart)
			tc.mutate(t, e, r)
			diffs, _ := compareSpecs(specOf(t, e), specOf(t, r))
			if !slices.Equal(diffs, tc.want) {
				t.Fatalf("differences:\n  %s\nwant:\n  %s", strings.Join(diffs, "\n  "), strings.Join(tc.want, "\n  "))
			}
		})
	}
}

// renameRefs rewrites every $ref equal to from.
func renameRefs(node any, from, to string) {
	switch v := node.(type) {
	case map[string]any:
		if v["$ref"] == from {
			v["$ref"] = to
		}
		for _, child := range v {
			renameRefs(child, from, to)
		}
	case []any:
		for _, child := range v {
			renameRefs(child, from, to)
		}
	}
}

// errorUses lists, in report order, the responses whose body is an Error.
func errorUses(msg string) []string {
	var lines []string
	for _, status := range []string{"401", "500"} {
		lines = append(lines, "GET /v1/capabilities "+status+" application/json: "+msg)
	}
	for _, status := range []string{"400", "401", "413", "415", "422", "429", "500", "503", "504"} {
		lines = append(lines, "POST /v1/convert "+status+" application/json: "+msg)
	}
	return lines
}

func TestCompareListsExtraSchemasWithoutFailing(t *testing.T) {
	e, r := loadTree(t, emittedPart), loadTree(t, referencePart)
	at(t, e, "components", "schemas")["Extra"] = map[string]any{"type": "string"}
	diffs, extra := compareSpecs(specOf(t, e), specOf(t, r))
	if len(diffs) != 0 {
		t.Errorf("differences: %v", diffs)
	}
	if !slices.Equal(extra, []string{"Extra"}) {
		t.Errorf("extra = %v, want [Extra]", extra)
	}
}

func TestCompareCommand(t *testing.T) {
	var out, errOut strings.Builder
	if code := run(context.Background(), []string{"compare", emittedPart, referencePart}, &out, &errOut); code != 0 {
		t.Fatalf("exit %d: %s %s", code, out.String(), errOut.String())
	}
	if code := run(context.Background(), []string{"compare", emittedPart}, &out, &errOut); code != 2 {
		t.Errorf("compare with one file: exit %d, want 2", code)
	}
	if code := run(context.Background(), []string{"compare", emittedPart, "../../../../api/openapi.yaml"}, &out, &errOut); code != 1 {
		t.Errorf("part against the whole reference: exit %d, want 1", code)
	}
}
