// Command speccheck checks the Convert API's OpenAPI documents.
//
//	speccheck validate FILE
//	    Validates one document (OpenAPI 3.0 or 3.1, YAML or JSON): structure
//	    and examples through kin-openapi, plus the rules this project adds
//	    (no null types, no integer arrays, the pages pattern, the error codes
//	    named in info.description).
//
// It exits 1 when it finds a problem and 2 on a usage error.
package main

import (
	"context"
	"fmt"
	"io"
	"os"
)

func main() {
	os.Exit(run(context.Background(), os.Args[1:], os.Stdout, os.Stderr))
}

func run(ctx context.Context, args []string, stdout, stderr io.Writer) int {
	if len(args) == 0 {
		usage(stderr)
		return 2
	}

	switch args[0] {
	case "validate":
		if len(args) != 2 {
			usage(stderr)
			return 2
		}
		findings, err := validateFile(ctx, args[1], stdout)
		return report(stdout, stderr, findings, err)
	default:
		usage(stderr)
		return 2
	}
}

func report(stdout, stderr io.Writer, findings []string, err error) int {
	if err != nil {
		fmt.Fprintln(stderr, "speccheck:", err)
		return 1
	}
	for _, f := range findings {
		fmt.Fprintln(stdout, f)
	}
	if len(findings) > 0 {
		fmt.Fprintf(stdout, "FAIL: %d problem(s)\n", len(findings))
		return 1
	}
	fmt.Fprintln(stdout, "ok")
	return 0
}

func usage(w io.Writer) {
	fmt.Fprintln(w, "usage: speccheck validate FILE")
}
