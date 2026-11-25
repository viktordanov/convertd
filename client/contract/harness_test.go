//go:build contract

package contract

import (
	"fmt"
	"io"
	"os"
	"testing"
)

// TestMain picks the mode from the environment:
//
//   - binary: CONVERTD_BIN is a convertd built with --features test-hooks.
//     One shared server is started with the fake engine (TEST_ENGINE=fake).
//   - image: CONVERT_HOST and CONVERT_API_KEY name a running service.
//
// With neither, it exits 1 rather than skipping, so that a CI job can never
// pass by running nothing. After the tests it prints the declared
// (operationId, status) pairs that no test observed.
func TestMain(m *testing.M) {
	os.Exit(runMain(m, os.Getenv, os.Stdout, os.Stderr))
}

func runMain(m *testing.M, getenv func(string) string, stdout, stderr io.Writer) int {
	bin, host := getenv("CONVERTD_BIN"), getenv("CONVERT_HOST")
	switch {
	case bin != "" && host != "":
		fmt.Fprintln(stderr, "contract: set CONVERTD_BIN or CONVERT_HOST, not both")
		return 1
	case bin != "":
		s, err := startServer(bin)
		if err != nil {
			fmt.Fprintln(stderr, "contract:", err)
			return 1
		}
		target.mode, target.baseURL, target.apiKey, target.bin = "binary", s.baseURL, testAPIKey, bin
		code := m.Run()
		if err := s.stop(); err != nil {
			fmt.Fprintln(stderr, "contract:", err)
			code = 1
		}
		if code != 0 {
			fmt.Fprintf(stderr, "--- shared server log ---\n%s", s.logs())
		}
		s.remove()
		printCoverage(stdout)
		return code
	case host != "":
		key := getenv("CONVERT_API_KEY")
		if key == "" {
			fmt.Fprintln(stderr, "contract: CONVERT_HOST is set but CONVERT_API_KEY is not")
			return 1
		}
		if err := waitReady(host, readyTimeout, nil); err != nil {
			fmt.Fprintln(stderr, "contract:", err)
			return 1
		}
		target.mode, target.baseURL, target.apiKey = "image", host, key
		code := m.Run()
		printCoverage(stdout)
		return code
	default:
		fmt.Fprintln(stderr, "contract: no server to test. Set CONVERTD_BIN to a convertd built with --features test-hooks,")
		fmt.Fprintln(stderr, "or CONVERT_HOST and CONVERT_API_KEY for a running service. scripts/contract_test.sh does this.")
		return 1
	}
}

// printCoverage lists the declared (operationId, status) pairs no test saw.
// It never fails the run: the cases assert each status themselves.
func printCoverage(w io.Writer) {
	s, err := theSpec()
	if err != nil {
		fmt.Fprintln(w, "coverage: failed to load the spec:", err)
		return
	}
	missing := unobserved(s)
	if len(missing) == 0 {
		fmt.Fprintln(w, "coverage: every declared (operationId, status) was observed")
		return
	}
	fmt.Fprintf(w, "coverage: %d declared (operationId, status) pair(s) not observed:\n", len(missing))
	for _, o := range missing {
		fmt.Fprintf(w, "  %s %s\n", o.operationID, o.status)
	}
}
