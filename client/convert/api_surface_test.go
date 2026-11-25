package convert

import (
	"go/ast"
	"go/parser"
	"go/token"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
)

const generatedImportPath = "github.com/viktordanov/rs-convert/client/internal/api"

// TestAPISurfaceHasNoGeneratedTypes keeps the generated package out of the
// public API: no exported function, method, type, field, variable or constant
// of this package may refer to internal/api. The generated code changes shape
// with every regeneration; the facade must not.
func TestAPISurfaceHasNoGeneratedTypes(t *testing.T) {
	files, err := filepath.Glob("*.go")
	if err != nil {
		t.Fatalf("failed to list package files: %v", err)
	}
	fset := token.NewFileSet()
	checked := 0
	for _, name := range files {
		if strings.HasSuffix(name, "_test.go") {
			continue
		}
		file, err := parser.ParseFile(fset, name, nil, parser.SkipObjectResolution)
		if err != nil {
			t.Fatalf("failed to parse %s: %v", name, err)
		}
		checked++
		for _, leak := range exportedGeneratedRefs(fset, file) {
			t.Errorf("%s exposes a generated type", leak)
		}
	}
	if checked == 0 {
		t.Fatal("no package files found")
	}
}

// TestAPISurfaceCheckerFindsLeaks proves that the checker above reports each
// kind of leak and ignores unexported uses.
func TestAPISurfaceCheckerFindsLeaks(t *testing.T) {
	const src = `package convert

import gen "github.com/viktordanov/rs-convert/client/internal/api"

type Client struct{ api *gen.Client }        // unexported field: fine
type wrapper struct{ M gen.Manifest }          // unexported type: fine
func helper(m gen.Manifest) {}                 // unexported func: fine
func (c *Client) do() gen.Error { return gen.Error{} } // unexported method: fine
func Exported() { _ = gen.Manifest{} }         // body only: fine

func X() gen.Manifest { return gen.Manifest{} }
func (c *Client) Raw(p *gen.ConvertDocumentParams) {}
type Alias = gen.Manifest
type Result struct {
	Public  []gen.PageInfo
	private gen.PageInfo
	Nested  struct{ Inner map[string]gen.Warning }
}
type Embeds struct{ gen.Capabilities }
type Iface interface{ Get() gen.EngineInfo }
var Default gen.ConvertOptions
var Kind = gen.OutputKind("pdf")
const Fmt = gen.ImageFormat("png")
`
	fset := token.NewFileSet()
	file, err := parser.ParseFile(fset, "leaks.go", src, parser.SkipObjectResolution)
	if err != nil {
		t.Fatalf("failed to parse: %v", err)
	}
	got := exportedGeneratedRefs(fset, file)
	want := []string{
		"leaks.go:11: func X uses gen.Manifest",
		"leaks.go:12: method Client.Raw uses gen.ConvertDocumentParams",
		"leaks.go:13: type Alias uses gen.Manifest",
		"leaks.go:14: type Result uses gen.PageInfo",
		"leaks.go:14: type Result uses gen.Warning",
		"leaks.go:19: type Embeds uses gen.Capabilities",
		"leaks.go:20: type Iface uses gen.EngineInfo",
		"leaks.go:21: var Default uses gen.ConvertOptions",
		"leaks.go:22: var Kind uses gen.OutputKind",
		"leaks.go:23: const Fmt uses gen.ImageFormat",
	}
	if strings.Join(got, "\n") != strings.Join(want, "\n") {
		t.Fatalf("got:\n  %s\nwant:\n  %s", strings.Join(got, "\n  "), strings.Join(want, "\n  "))
	}
}

// exportedGeneratedRefs lists every exported declaration of file that refers
// to the generated package, as "file:line: kind Name uses pkg.Ident".
func exportedGeneratedRefs(fset *token.FileSet, file *ast.File) []string {
	pkg := generatedImportName(file)
	if pkg == "" {
		return nil
	}

	var leaks []string
	report := func(pos token.Pos, what string, node ast.Node) {
		for _, ref := range selectorsOf(node, pkg) {
			p := fset.Position(pos)
			leaks = append(leaks, p.Filename+":"+strconv.Itoa(p.Line)+": "+what+" uses "+ref)
		}
	}

	for _, decl := range file.Decls {
		switch d := decl.(type) {
		case *ast.FuncDecl:
			if !d.Name.IsExported() {
				continue
			}
			if d.Recv == nil {
				report(d.Pos(), "func "+d.Name.Name, d.Type)
				continue
			}
			recv := baseTypeName(d.Recv.List[0].Type)
			if ast.IsExported(recv) {
				report(d.Pos(), "method "+recv+"."+d.Name.Name, d.Type)
			}
		case *ast.GenDecl:
			for _, spec := range d.Specs {
				switch s := spec.(type) {
				case *ast.TypeSpec:
					if s.Name.IsExported() {
						if s.TypeParams != nil {
							report(s.Pos(), "type "+s.Name.Name, s.TypeParams)
						}
						report(s.Pos(), "type "+s.Name.Name, s.Type)
					}
				case *ast.ValueSpec:
					kind := strings.ToLower(d.Tok.String())
					for i, name := range s.Names {
						if !name.IsExported() {
							continue
						}
						if s.Type != nil {
							report(s.Pos(), kind+" "+name.Name, s.Type)
						}
						if i < len(s.Values) {
							report(s.Pos(), kind+" "+name.Name, s.Values[i])
						}
					}
				}
			}
		}
	}
	return leaks
}

// generatedImportName returns the name the file uses for the generated
// package, or "" when the file does not import it.
func generatedImportName(file *ast.File) string {
	for _, imp := range file.Imports {
		path, err := strconv.Unquote(imp.Path.Value)
		if err != nil || path != generatedImportPath {
			continue
		}
		if imp.Name != nil {
			return imp.Name.Name
		}
		return filepath.Base(path)
	}
	return ""
}

// selectorsOf returns each pkg.Ident below node, in source order. It looks
// only at what is visible outside the package: exported struct fields and
// exported interface methods, not unexported ones.
func selectorsOf(node ast.Node, pkg string) []string {
	var refs []string
	var visit func(ast.Node)
	visit = func(n ast.Node) {
		ast.Inspect(n, func(n ast.Node) bool {
			switch v := n.(type) {
			case *ast.StructType:
				for _, field := range v.Fields.List {
					if fieldIsExported(field) {
						visit(field.Type)
					}
				}
				return false
			case *ast.InterfaceType:
				for _, method := range v.Methods.List {
					if fieldIsExported(method) {
						visit(method.Type)
					}
				}
				return false
			case *ast.SelectorExpr:
				if id, ok := v.X.(*ast.Ident); ok && id.Name == pkg {
					refs = append(refs, pkg+"."+v.Sel.Name)
				}
			}
			return true
		})
	}
	visit(node)
	return refs
}

// fieldIsExported reports whether a struct field or interface element is
// visible outside the package. An embedded field is named after its type.
func fieldIsExported(field *ast.Field) bool {
	if len(field.Names) == 0 {
		return ast.IsExported(baseTypeName(field.Type))
	}
	for _, name := range field.Names {
		if name.IsExported() {
			return true
		}
	}
	return false
}

// baseTypeName returns the type name of T, *T, pkg.T or T[P].
func baseTypeName(expr ast.Expr) string {
	switch e := expr.(type) {
	case *ast.Ident:
		return e.Name
	case *ast.StarExpr:
		return baseTypeName(e.X)
	case *ast.SelectorExpr:
		return e.Sel.Name
	case *ast.IndexExpr:
		return baseTypeName(e.X)
	case *ast.IndexListExpr:
		return baseTypeName(e.X)
	case *ast.ParenExpr:
		return baseTypeName(e.X)
	default:
		return ""
	}
}
