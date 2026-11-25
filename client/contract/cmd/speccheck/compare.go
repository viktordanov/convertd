package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"slices"
	"sort"
	"strings"

	"github.com/getkin/kin-openapi/openapi3"
	"go.yaml.in/yaml/v4"
)

var httpMethods = []string{"get", "put", "post", "delete", "options", "head", "patch", "trace"}

// compareFiles compares the spec emitted by convertd with the hand-written
// reference. It returns one line per difference. Component schemas that only
// the emitted spec has are printed to out as information.
func compareFiles(ctx context.Context, emittedPath, referencePath string, out io.Writer) ([]string, error) {
	e, err := loadSpec(ctx, emittedPath)
	if err != nil {
		return nil, fmt.Errorf("failed to load emitted spec: %w", err)
	}
	r, err := loadSpec(ctx, referencePath)
	if err != nil {
		return nil, fmt.Errorf("failed to load reference spec: %w", err)
	}
	diffs, extra := compareSpecs(e, r)
	for _, name := range extra {
		fmt.Fprintf(out, "info: schemas.%s is only in the emitted spec\n", name)
	}
	return diffs, nil
}

// loadSpec parses a YAML or JSON document. kin-openapi must be able to load
// it, but the comparison runs on the raw tree.
func loadSpec(ctx context.Context, path string) (*spec, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("failed to read spec: %w", err)
	}
	return parseSpec(ctx, data)
}

func parseSpec(_ context.Context, data []byte) (*spec, error) {
	if _, err := openapi3.NewLoader().LoadFromData(data); err != nil {
		return nil, fmt.Errorf("failed to load spec: %w", err)
	}
	var raw any
	if err := yaml.Unmarshal(data, &raw); err != nil {
		return nil, fmt.Errorf("failed to parse spec: %w", err)
	}
	root, ok := raw.(map[string]any)
	if !ok {
		return nil, errors.New("failed to parse spec: the document is not an object")
	}
	return &spec{root: root}, nil
}

// compareSpecs returns the differences, and the schema names only e has.
func compareSpecs(e, r *spec) ([]string, []string) {
	d := &differ{e: e, r: r}

	ev := asMap(e.root["info"])["version"]
	rv := asMap(r.root["info"])["version"]
	if canon(ev) != canon(rv) {
		d.addf("info.version", "%s in reference, %s in emitted", canon(rv), canon(ev))
	}

	// Schema names: the facade and the generated types depend on them.
	eSchemas, rSchemas := e.schemas(), r.schemas()
	for _, name := range sortedKeys(rSchemas) {
		if _, ok := eSchemas[name]; !ok {
			d.addf("schemas."+name, "missing in emitted")
			continue
		}
		d.compareSchema("schemas."+name, eSchemas[name], rSchemas[name])
	}
	var extra []string
	for _, name := range sortedKeys(eSchemas) {
		if _, ok := rSchemas[name]; !ok {
			extra = append(extra, name)
		}
	}

	eOps, rOps := operations(e), operations(r)
	for _, key := range sortedOpKeys(rOps) {
		eo, ok := eOps[key]
		if !ok {
			d.addf(key, "operation missing in emitted")
			continue
		}
		d.compareOperation(key, eo, rOps[key])
	}
	for _, key := range sortedOpKeys(eOps) {
		if _, ok := rOps[key]; !ok {
			d.addf(key, "operation only in emitted")
		}
	}

	return d.diffs, extra
}

type operation struct {
	op         map[string]any
	pathParams []any
}

// operations indexes every operation as "METHOD /path".
func operations(s *spec) map[string]operation {
	ops := map[string]operation{}
	paths := asMap(s.root["paths"])
	for _, p := range sortedKeys(paths) {
		item := s.resolve(paths[p])
		for _, m := range httpMethods {
			if op, ok := item[m].(map[string]any); ok {
				ops[strings.ToUpper(m)+" "+p] = operation{op: op, pathParams: asList(item["parameters"])}
			}
		}
	}
	return ops
}

func sortedOpKeys(m map[string]operation) []string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	return keys
}

func (d *differ) compareOperation(key string, eo, ro operation) {
	e, r := eo.op, ro.op

	if canon(e["operationId"]) != canon(r["operationId"]) {
		d.addf(key, "operationId %s in reference, %s in emitted", canon(r["operationId"]), canon(e["operationId"]))
	}
	if et, rt := stringSet(e["tags"]), stringSet(r["tags"]); !slices.Equal(et, rt) {
		d.addf(key, "tags %s in reference, %s in emitted", listOrNone(rt), listOrNone(et))
	}
	if es, rs := security(d.e, e), security(d.r, r); es != rs {
		d.addf(key, "security %s in reference, %s in emitted", rs, es)
	}

	d.compareParameters(key, d.e.parameters(eo), d.r.parameters(ro))
	d.compareRequestBody(key, d.e.resolve(e["requestBody"]), d.r.resolve(r["requestBody"]))

	eResp, rResp := asMap(e["responses"]), asMap(r["responses"])
	for _, status := range sortedKeys(rResp) {
		if _, ok := eResp[status]; !ok {
			d.addf(key, "response %s missing in emitted", status)
			continue
		}
		d.compareResponse(key+" "+status, d.e.resolve(eResp[status]), d.r.resolve(rResp[status]))
	}
	for _, status := range sortedKeys(eResp) {
		if _, ok := rResp[status]; !ok {
			d.addf(key, "response %s only in emitted", status)
		}
	}
}

// security describes the effective security of an operation by scheme type,
// not by scheme name: "http bearer", or "none".
func security(s *spec, op map[string]any) string {
	reqs, ok := op["security"]
	if !ok {
		reqs = s.root["security"]
	}
	schemes := asMap(asMap(s.root["components"])["securitySchemes"])
	var alternatives []string
	for _, req := range asList(reqs) {
		var parts []string
		for _, name := range sortedKeys(asMap(req)) {
			scheme := s.resolve(schemes[name])
			desc := fmt.Sprintf("%v", scheme["type"])
			switch scheme["type"] {
			case "http":
				desc += " " + strings.ToLower(fmt.Sprintf("%v", scheme["scheme"]))
			case "apiKey":
				desc += fmt.Sprintf(" %v %v", scheme["in"], scheme["name"])
			}
			parts = append(parts, desc)
		}
		if len(parts) == 0 {
			alternatives = append(alternatives, "none")
		} else {
			alternatives = append(alternatives, strings.Join(parts, " + "))
		}
	}
	if len(alternatives) == 0 {
		return "none"
	}
	sort.Strings(alternatives)
	return strings.Join(slices.Compact(alternatives), " | ")
}

// parameters returns the operation's parameters, path-level ones included,
// keyed by "in name". Header names are case-insensitive.
func (s *spec) parameters(o operation) map[string]map[string]any {
	out := map[string]map[string]any{}
	for _, list := range [][]any{o.pathParams, asList(o.op["parameters"])} {
		for _, p := range list {
			param := s.resolve(p)
			name := fmt.Sprintf("%v", param["name"])
			if param["in"] == "header" {
				name = strings.ToLower(name)
			}
			out[fmt.Sprintf("%v %s", param["in"], name)] = param
		}
	}
	return out
}

func (d *differ) compareParameters(key string, e, r map[string]map[string]any) {
	for _, name := range sortedMapKeys(r) {
		ep, ok := e[name]
		if !ok {
			d.addf(key, "parameter %s missing in emitted", name)
			continue
		}
		rp := r[name]
		if eb, rb := ep["required"] == true, rp["required"] == true; eb != rb {
			d.addf(key, "parameter %s: required %t in reference, %t in emitted", name, rb, eb)
		}
		d.compareSchema(key+" parameter "+name, ep["schema"], rp["schema"])
	}
	for _, name := range sortedMapKeys(e) {
		if _, ok := r[name]; !ok {
			d.addf(key, "parameter %s only in emitted", name)
		}
	}
}

func (d *differ) compareRequestBody(key string, e, r map[string]any) {
	if e == nil && r == nil {
		return
	}
	if e == nil || r == nil {
		d.addf(key, "request body only in %s", sideName(e != nil))
		return
	}
	if eb, rb := e["required"] == true, r["required"] == true; eb != rb {
		d.addf(key, "request body: required %t in reference, %t in emitted", rb, eb)
	}
	d.compareContent(key+" request", asMap(e["content"]), asMap(r["content"]), true)
}

func (d *differ) compareResponse(key string, e, r map[string]any) {
	d.compareContent(key, asMap(e["content"]), asMap(r["content"]), false)

	eh, rh := headerNames(e), headerNames(r)
	for _, name := range rh {
		if !slices.Contains(eh, name) {
			d.addf(key, "header %s missing in emitted", name)
		}
	}
	for _, name := range eh {
		if !slices.Contains(rh, name) {
			d.addf(key, "header %s only in emitted", name)
		}
	}
}

// headerNames returns the declared response header names, lower case.
func headerNames(resp map[string]any) []string {
	var names []string
	for name := range asMap(resp["headers"]) {
		names = append(names, strings.ToLower(name))
	}
	sort.Strings(names)
	return names
}

func (d *differ) compareContent(key string, e, r map[string]any, withEncoding bool) {
	for _, ct := range sortedKeys(r) {
		if _, ok := e[ct]; !ok {
			d.addf(key, "content type %s missing in emitted", ct)
			continue
		}
		em, rm := asMap(e[ct]), asMap(r[ct])
		d.compareSchema(key+" "+ct, em["schema"], rm["schema"])
		if withEncoding {
			d.compareEncoding(key+" "+ct, asMap(em["encoding"]), asMap(rm["encoding"]))
		}
	}
	for _, ct := range sortedKeys(e) {
		if _, ok := r[ct]; !ok {
			d.addf(key, "content type %s only in emitted", ct)
		}
	}
}

func (d *differ) compareEncoding(key string, e, r map[string]any) {
	parts := map[string]bool{}
	for p := range e {
		parts[p] = true
	}
	for p := range r {
		parts[p] = true
	}
	for _, p := range sortedBoolKeys(parts) {
		ect, _ := asMap(e[p])["contentType"].(string)
		rct, _ := asMap(r[p])["contentType"].(string)
		if ect != rct {
			d.addf(key, "encoding.%s.contentType %s in reference, %s in emitted", p, orNone(rct), orNone(ect))
		}
	}
}

func sortedMapKeys(m map[string]map[string]any) []string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	return keys
}
