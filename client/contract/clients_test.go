package contract

import (
	"context"
	"net/http"
	"testing"

	"github.com/viktordanov/rs-convert/client/convert"
	"github.com/viktordanov/rs-convert/client/internal/api"
)

// facadeFor returns the public client, sending through rt.
func facadeFor(t testing.TB, baseURL, apiKey string, rt http.RoundTripper) *convert.Client {
	t.Helper()
	c, err := convert.New(baseURL, apiKey, convert.WithHTTPClient(&http.Client{Transport: rt}), convert.WithTempDir(t.TempDir()))
	if err != nil {
		t.Fatalf("failed to create facade client: %v", err)
	}
	return c
}

// generatedFor returns the generated client with the bearer key set (none
// when apiKey is empty), sending through rt.
func generatedFor(t testing.TB, baseURL, apiKey string, rt http.RoundTripper) *api.Client {
	t.Helper()
	opts := []api.ClientOption{api.WithHTTPClient(&http.Client{Transport: rt})}
	if apiKey != "" {
		opts = append(opts, api.WithRequestEditorFn(func(_ context.Context, req *http.Request) error {
			req.Header.Set("Authorization", "Bearer "+apiKey)
			return nil
		}))
	}
	c, err := api.NewClient(baseURL, opts...)
	if err != nil {
		t.Fatalf("failed to create generated client: %v", err)
	}
	return c
}

// newFacade returns the public client for the target service, with every
// response checked against the spec.
func newFacade(t *testing.T) *convert.Client {
	t.Helper()
	return facadeFor(t, target.baseURL, target.apiKey, newTransport(t))
}

// newGenerated returns the generated client for the target service, with
// every response checked against the spec.
func newGenerated(t *testing.T) *api.Client {
	t.Helper()
	return generatedFor(t, target.baseURL, target.apiKey, newTransport(t))
}
