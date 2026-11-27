package contract

import (
	"testing"

	"github.com/viktordanov/convertd/client/internal/api"
)

// TestInternalImport proves that this nested module may import the client's
// internal package: Go's internal rule compares import paths, not modules.
func TestInternalImport(t *testing.T) {
	c, err := api.NewClient("http://127.0.0.1:1")
	if err != nil {
		t.Fatalf("failed to create generated client: %v", err)
	}
	if c.Server != "http://127.0.0.1:1/" {
		t.Errorf("server = %q, want the URL with a trailing slash", c.Server)
	}
}
