package main

import (
	"bytes"
	"context"
	"io"
	"os"
	"slices"
	"strings"
	"testing"
)

// minimalDoc is a small, valid OpenAPI 3.1 document with the schemas the
// project checks need (ConvertOptions.pages and Error.code). EXTRA is
// replaced with more component schemas, and PARAMSCHEMA with the schema of a
// header parameter.
const minimalDoc = `{
  "openapi": "3.1.0",
  "info": {
    "title": "t",
    "version": "1.0.0",
    "description": "Errors: ` + "`BAD_REQUEST`" + `, ` + "`UNAUTHORIZED`" + `."
  },
  "paths": {
    "/v1/thing": {
      "get": {
        "operationId": "getThing",
        "parameters": [
          {"name": "X-Request-ID", "in": "header", "required": false, "schema": PARAMSCHEMA}
        ],
        "responses": {
          "200": {
            "description": "ok",
            "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}
          }
        }
      }
    }
  },
  "components": {
    "schemas": {
      "ConvertOptions": {
        "type": "object",
        "properties": {
          "pages": {"type": "string", "pattern": "^[0-9]+(-[0-9]*)?(,[0-9]+(-[0-9]*)?)*$"}
        }
      },
      "Error": {
        "type": "object",
        "required": ["error", "code"],
        "properties": {
          "error": {"type": "string", "description": "never type: null here"},
          "code": {"$ref": "#/components/schemas/ErrorCode"}
        }
      },
      "ErrorCode": {"type": "string", "enum": ["BAD_REQUEST", "UNAUTHORIZED"]}
      EXTRA
    }
  }
}`

func docWith(extra, paramSchema string) []byte {
	if extra != "" {
		extra = "," + extra
	}
	if paramSchema == "" {
		paramSchema = `{"type": "string"}`
	}
	s := strings.Replace(minimalDoc, "EXTRA", extra, 1)
	s = strings.Replace(s, "PARAMSCHEMA", paramSchema, 1)
	return []byte(s)
}

func validateForTest(t *testing.T, data []byte) []string {
	t.Helper()
	findings, err := validateData(context.Background(), data, io.Discard)
	if err != nil {
		t.Fatalf("validate: %v", err)
	}
	return findings
}

func TestValidateCleanDocument(t *testing.T) {
	if findings := validateForTest(t, docWith("", "")); len(findings) != 0 {
		t.Fatalf("clean document has findings:\n%s", strings.Join(findings, "\n"))
	}
}

func TestValidateReferenceSpec(t *testing.T) {
	data, err := os.ReadFile("../../../../api/openapi.yaml")
	if err != nil {
		t.Fatalf("failed to read reference spec: %v", err)
	}
	if findings := validateForTest(t, data); len(findings) != 0 {
		t.Fatalf("reference spec has findings:\n%s", strings.Join(findings, "\n"))
	}
}

func TestValidateFindsBadSchemas(t *testing.T) {
	cases := []struct {
		name        string
		extra       string
		paramSchema string
		want        string
	}{
		{
			name:  "type array with null",
			extra: `"Page": {"type": "object", "properties": {"label": {"type": ["string", "null"]}}}`,
			want:  `components.schemas.Page.properties.label.type: type includes "null"`,
		},
		{
			name:  "oneOf member of type null",
			extra: `"Page": {"type": "object", "properties": {"pdf": {"oneOf": [{"type": "null"}, {"$ref": "#/components/schemas/Error"}]}}}`,
			want:  `components.schemas.Page.properties.pdf.oneOf[0].type: type includes "null"`,
		},
		{
			name:  "anyOf member of type null",
			extra: `"Page": {"anyOf": [{"type": "string"}, {"type": "null"}]}`,
			want:  `components.schemas.Page.anyOf[1].type: type includes "null"`,
		},
		{
			name:  "nullable true",
			extra: `"Page": {"type": "object", "properties": {"n": {"type": "integer", "nullable": true}}}`,
			want:  `components.schemas.Page.properties.n.nullable: nullable: true is not allowed`,
		},
		{
			name:  "enum with null",
			extra: `"Page": {"type": "string", "enum": ["a", null]}`,
			want:  `components.schemas.Page.enum: enum includes null`,
		},
		{
			name:  "integer array",
			extra: `"Blob": {"type": "object", "properties": {"bytes": {"type": "array", "items": {"type": "integer", "format": "int32", "minimum": 0}}}}`,
			want:  `components.schemas.Blob.properties.bytes.items: array of integers`,
		},
		{
			name:        "null type in a parameter schema",
			paramSchema: `{"type": ["string", "null"]}`,
			want:        `paths./v1/thing.get.parameters[0].schema.type: type includes "null"`,
		},
		{
			name:  "nested in items and additionalProperties",
			extra: `"Map": {"type": "object", "additionalProperties": {"type": "array", "items": {"type": ["integer", "null"]}}}`,
			want:  `components.schemas.Map.additionalProperties.items.type: type includes "null"`,
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			findings := validateForTest(t, docWith(tc.extra, tc.paramSchema))
			if !slices.ContainsFunc(findings, func(f string) bool { return strings.HasPrefix(f, tc.want) }) {
				t.Fatalf("want a finding starting with\n  %s\ngot:\n  %s", tc.want, strings.Join(findings, "\n  "))
			}
		})
	}
}

func TestValidateIgnoresNullInProseAndExamples(t *testing.T) {
	extra := `"Note": {"type": "object", "description": "type: null and nullable: true in prose", "example": {"type": "null", "nullable": true}}`
	if findings := validateForTest(t, docWith(extra, "")); len(findings) != 0 {
		t.Fatalf("unexpected findings:\n%s", strings.Join(findings, "\n"))
	}
}

func TestValidatePagesPattern(t *testing.T) {
	data := bytes.Replace(docWith("", ""), []byte(`"^[0-9]+(-[0-9]*)?(,[0-9]+(-[0-9]*)?)*$"`), []byte(`"[0-9]"`), 1)
	findings := validateForTest(t, data)
	want := []string{
		`components.schemas.ConvertOptions.properties.pages.pattern: "-3" matched=true, want false`,
		`components.schemas.ConvertOptions.properties.pages.pattern: "1,,2" matched=true, want false`,
	}
	if !slices.Equal(findings, want) {
		t.Fatalf("findings:\n  %s\nwant:\n  %s", strings.Join(findings, "\n  "), strings.Join(want, "\n  "))
	}
}

func TestValidateErrorCodesInDescription(t *testing.T) {
	data := bytes.Replace(docWith("", ""), []byte("`UNAUTHORIZED`"), []byte("`UNAUTHORIZED`, `NOT_LOGGED_IN`"), 1)
	findings := validateForTest(t, data)
	want := []string{"info.description: names NOT_LOGGED_IN, which is not in the error code enum"}
	if !slices.Equal(findings, want) {
		t.Fatalf("findings:\n  %s\nwant:\n  %s", strings.Join(findings, "\n  "), strings.Join(want, "\n  "))
	}
}

func TestValidateBadExample(t *testing.T) {
	extra := `"Count": {"type": "integer", "minimum": 1, "example": 0}`
	findings := validateForTest(t, docWith(extra, ""))
	if !slices.ContainsFunc(findings, func(f string) bool {
		return strings.HasPrefix(f, "structure: ") && strings.Contains(f, `schema "Count": invalid example`)
	}) {
		t.Fatalf("bad example not reported:\n%s", strings.Join(findings, "\n"))
	}
}

func TestRunUsage(t *testing.T) {
	var stdout, stderr bytes.Buffer
	if code := run(context.Background(), nil, &stdout, &stderr); code != 2 {
		t.Errorf("no arguments: exit %d, want 2", code)
	}
	if code := run(context.Background(), []string{"validate"}, &stdout, &stderr); code != 2 {
		t.Errorf("validate without a file: exit %d, want 2", code)
	}
	if code := run(context.Background(), []string{"validate", "does-not-exist.yaml"}, &stdout, &stderr); code != 1 {
		t.Errorf("missing file: exit %d, want 1", code)
	}
}
