package main

import (
	"encoding/json"
	"fmt"
	"maps"
	"slices"
	"sort"
	"strings"
)

// spec is one parsed document, kept as the raw tree. compare works on the
// tree rather than on kin-openapi's model so that every normalisation is
// written out here, including how a 3.1 $ref with sibling keywords is read.
type spec struct {
	root map[string]any
}

// schemaNode is a schema after the first normalisation step.
//
//   - allOf with one member is unwrapped: the member's keywords and the
//     allOf's siblings become one schema. allOf: [{$ref}] with siblings thus
//     reads as a $ref with siblings, the form utoipa emits for 3.1.
//   - ref is the component name when the schema is a $ref to
//     components.schemas, and own holds the keywords written beside it.
//   - Without a $ref, own holds all keywords.
type schemaNode struct {
	ref string
	own map[string]any
}

const schemaRefPrefix = "#/components/schemas/"

func (s *spec) node(v any) schemaNode {
	m := asMap(v)
	if all, ok := m["allOf"].([]any); ok && len(all) == 1 {
		merged := maps.Clone(asMap(all[0]))
		if merged == nil {
			merged = map[string]any{}
		}
		for k, val := range m {
			if k != "allOf" {
				merged[k] = val
			}
		}
		return s.node(merged)
	}
	if ref, ok := m["$ref"].(string); ok {
		own := maps.Clone(m)
		delete(own, "$ref")
		return schemaNode{ref: strings.TrimPrefix(ref, schemaRefPrefix), own: own}
	}
	return schemaNode{own: m}
}

// full returns every keyword that applies at this place: for a $ref, the
// target's keywords with the siblings laid over them. seen stops alias loops.
func (s *spec) full(n schemaNode) map[string]any {
	return s.fullSeen(n, map[string]bool{})
}

func (s *spec) fullSeen(n schemaNode, seen map[string]bool) map[string]any {
	if n.ref == "" {
		return n.own
	}
	if seen[n.ref] {
		return n.own
	}
	seen[n.ref] = true
	target, ok := s.schemas()[n.ref]
	if !ok {
		return n.own
	}
	out := maps.Clone(s.fullSeen(s.node(target), seen))
	if out == nil {
		out = map[string]any{}
	}
	for k, v := range n.own {
		out[k] = v
	}
	return out
}

func (s *spec) schemas() map[string]any {
	return asMap(asMap(s.root["components"])["schemas"])
}

// resolve follows a local $ref such as #/components/responses/BadRequest, so
// that inline and component forms compare equal. Schemas are not resolved
// here; schemaNode keeps their names.
func (s *spec) resolve(v any) map[string]any {
	m := asMap(v)
	for range 8 {
		ref, ok := m["$ref"].(string)
		if !ok || !strings.HasPrefix(ref, "#/") {
			return m
		}
		var cur any = s.root
		for _, part := range strings.Split(strings.TrimPrefix(ref, "#/"), "/") {
			part = strings.ReplaceAll(strings.ReplaceAll(part, "~1", "/"), "~0", "~")
			cur = asMap(cur)[part]
		}
		m = asMap(cur)
	}
	return m
}

// Keywords that are prose or metadata. They are never compared.
var ignoredKeywords = map[string]bool{
	"description": true, "summary": true, "title": true,
	"example": true, "examples": true, "$comment": true,
	"externalDocs": true, "xml": true, "discriminator": true,
	"nullable": true, // forbidden by validate; null is not compared
}

// Constraint keywords. Each one in the reference must have the same value in
// the emitted spec, and each extra one in the emitted spec is a difference,
// except minimum: 0 on an integer, which utoipa adds for unsigned types.
var constraintKeywords = []string{
	"minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum", "multipleOf",
	"minLength", "maxLength", "pattern",
	"minItems", "maxItems", "uniqueItems",
	"minProperties", "maxProperties",
	"default", "const",
}

// Keywords handled by compareSchema itself.
var structuralKeywords = map[string]bool{
	"type": true, "format": true, "properties": true, "required": true,
	"additionalProperties": true, "enum": true, "items": true,
	"readOnly": true, "writeOnly": true,
	"allOf": true, "oneOf": true, "anyOf": true, "not": true,
}

type differ struct {
	e, r  *spec
	diffs []string
}

func (d *differ) addf(path, format string, args ...any) {
	d.diffs = append(d.diffs, path+": "+fmt.Sprintf(format, args...))
}

// compareSchema compares one place in both documents. When both sides are a
// $ref, the names must match and only the sibling keywords are compared
// here; the components themselves are compared once, by name. When only one
// side is a $ref, its target is expanded, so a named enum and an inline enum
// compare by their values.
func (d *differ) compareSchema(path string, ev, rv any) {
	en, rn := d.e.node(ev), d.r.node(rv)
	ef, rf := d.e.full(en), d.r.full(rn)
	if en.ref != "" && rn.ref != "" {
		if en.ref != rn.ref {
			d.addf(path, "$ref %s in reference, %s in emitted", rn.ref, en.ref)
			return
		}
		d.compareKeywords(path, en.own, rn.own, ef, rf)
		return
	}
	d.compareKeywords(path, ef, rf, ef, rf)
}

// compareKeywords compares the keywords in e and r. ef and rf are the full
// schemas at this place, which give the types when e and r are only the
// siblings of a $ref.
func (d *differ) compareKeywords(path string, e, r, ef, rf map[string]any) {
	eTypes, rTypes := typeSet(ef), typeSet(rf)

	if et, rt := typeSet(e), typeSet(r); !slices.Equal(et, rt) {
		d.addf(path, "type %s in reference, %s in emitted", listOrNone(rt), listOrNone(et))
	}

	d.compareFormat(path, e, r, eTypes, rTypes)

	// Properties and required.
	eProps, rProps := asMap(e["properties"]), asMap(r["properties"])
	for _, name := range sortedKeys(rProps) {
		if _, ok := eProps[name]; !ok {
			d.addf(path, "property %s missing in emitted", name)
			continue
		}
		d.compareSchema(path+"."+name, eProps[name], rProps[name])
	}
	for _, name := range sortedKeys(eProps) {
		if _, ok := rProps[name]; !ok {
			d.addf(path, "property %s only in emitted", name)
		}
	}
	if er, rr := stringSet(e["required"]), stringSet(r["required"]); !slices.Equal(er, rr) {
		d.addf(path, "required %s in reference, %s in emitted", listOrNone(rr), listOrNone(er))
	}

	d.compareAdditionalProperties(path, e, r)

	if ee, re := enumSet(e["enum"]), enumSet(r["enum"]); !slices.Equal(ee, re) {
		d.addf(path, "enum %s in reference, %s in emitted", listOrNone(re), listOrNone(ee))
	}

	_, eItems := e["items"]
	_, rItems := r["items"]
	switch {
	case eItems && rItems:
		d.compareSchema(path+"[]", e["items"], r["items"])
	case rItems:
		d.addf(path, "items missing in emitted")
	case eItems:
		d.addf(path, "items only in emitted")
	}

	for _, k := range []string{"readOnly", "writeOnly"} {
		if eb, rb := e[k] == true, r[k] == true; eb != rb {
			d.addf(path, "%s %t in reference, %t in emitted", k, rb, eb)
		}
	}

	for _, k := range []string{"allOf", "oneOf", "anyOf"} {
		el, rl := asList(e[k]), asList(r[k])
		if len(el) != len(rl) {
			d.addf(path, "%s with %d member(s) in reference, %d in emitted", k, len(rl), len(el))
			continue
		}
		for i := range rl {
			d.compareSchema(fmt.Sprintf("%s.%s[%d]", path, k, i), el[i], rl[i])
		}
	}
	_, eNot := e["not"]
	_, rNot := r["not"]
	if eNot || rNot {
		if eNot != rNot {
			d.addf(path, "not in %s only", sideName(eNot))
		} else {
			d.compareSchema(path+".not", e["not"], r["not"])
		}
	}

	for _, k := range constraintKeywords {
		ev, eHas := e[k]
		rv, rHas := r[k]
		switch {
		case rHas && !eHas:
			d.addf(path, "%s %s in reference, none in emitted", k, canon(rv))
		case eHas && !rHas:
			if k == "minimum" && slices.Contains(eTypes, "integer") && canon(ev) == "0" {
				continue // utoipa's minimum: 0 for unsigned integers
			}
			d.addf(path, "%s none in reference, %s in emitted", k, canon(ev))
		case eHas && rHas && canon(ev) != canon(rv):
			d.addf(path, "%s %s in reference, %s in emitted", k, canon(rv), canon(ev))
		}
	}

	// Anything else that is not prose or an extension is compared as a value,
	// so that a form no rule knows about shows up instead of passing.
	keys := map[string]bool{}
	for k := range e {
		keys[k] = true
	}
	for k := range r {
		keys[k] = true
	}
	for _, k := range sortedBoolKeys(keys) {
		if ignoredKeywords[k] || structuralKeywords[k] || slices.Contains(constraintKeywords, k) || strings.HasPrefix(k, "x-") {
			continue
		}
		ev, eHas := e[k]
		rv, rHas := r[k]
		if !eHas || !rHas || canon(ev) != canon(rv) {
			d.addf(path, "%s %s in reference, %s in emitted", k, canonOrNone(rv, rHas), canonOrNone(ev, eHas))
		}
	}
}

// compareFormat compares string formats exactly. Integer and number formats
// are ignored, except that a reference int64 must stay int64.
func (d *differ) compareFormat(path string, e, r map[string]any, eTypes, rTypes []string) {
	ef, _ := e["format"].(string)
	rf, _ := r["format"].(string)
	numeric := slices.Contains(rTypes, "integer") || slices.Contains(rTypes, "number") ||
		slices.Contains(eTypes, "integer") || slices.Contains(eTypes, "number")
	if numeric {
		if rf == "int64" && ef != "int64" {
			d.addf(path, "format int64 in reference, %s in emitted", orNone(ef))
		}
		return
	}
	if ef != rf {
		d.addf(path, "format %s in reference, %s in emitted", orNone(rf), orNone(ef))
	}
}

// compareAdditionalProperties treats absent, true and {} as the same: any
// value is allowed. false must match, and a schema is compared.
func (d *differ) compareAdditionalProperties(path string, e, r map[string]any) {
	ek, ev := additionalKind(e)
	rk, rv := additionalKind(r)
	if ek != rk {
		d.addf(path, "additionalProperties %s in reference, %s in emitted", rk, ek)
		return
	}
	if rk == "schema" {
		d.compareSchema(path+".additionalProperties", ev, rv)
	}
}

func additionalKind(m map[string]any) (string, any) {
	v, ok := m["additionalProperties"]
	if !ok || v == true {
		return "any", nil
	}
	if v == false {
		return "false", nil
	}
	if sm, isMap := v.(map[string]any); isMap && len(sm) == 0 {
		return "any", nil
	}
	return "schema", v
}

// typeSet returns the type keyword as a sorted set without "null": a scalar
// and an array of one element are the same.
func typeSet(m map[string]any) []string {
	var out []string
	for _, t := range schemaTypes(m) {
		if t != "null" && !slices.Contains(out, t) {
			out = append(out, t)
		}
	}
	sort.Strings(out)
	return out
}

func stringSet(v any) []string {
	var out []string
	for _, item := range asList(v) {
		if s, ok := item.(string); ok {
			out = append(out, s)
		}
	}
	sort.Strings(out)
	return slices.Compact(out)
}

func enumSet(v any) []string {
	var out []string
	for _, item := range asList(v) {
		if item != nil {
			out = append(out, canon(item))
		}
	}
	sort.Strings(out)
	return slices.Compact(out)
}

// canon renders a value as canonical JSON, so that 1 from YAML and 1 from
// JSON, or two maps with the same keys, compare equal.
func canon(v any) string {
	b, err := json.Marshal(v)
	if err != nil {
		return fmt.Sprintf("%v", v)
	}
	return string(b)
}

func canonOrNone(v any, has bool) string {
	if !has {
		return "none"
	}
	return canon(v)
}

func listOrNone(l []string) string {
	if len(l) == 0 {
		return "none"
	}
	return "[" + strings.Join(l, ", ") + "]"
}

func orNone(s string) string {
	if s == "" {
		return "none"
	}
	return s
}

func sideName(emitted bool) string {
	if emitted {
		return "emitted"
	}
	return "reference"
}

func asMap(v any) map[string]any {
	m, _ := v.(map[string]any)
	return m
}

func asList(v any) []any {
	l, _ := v.([]any)
	return l
}

func sortedBoolKeys(m map[string]bool) []string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	return keys
}
