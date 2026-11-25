// Package convert is the hand-written client for the Convert API.
//
// It wraps the generated low-level client in internal/api and adds what
// callers need: streamed uploads, bundle unpacking, plain Go types and sentinel
// errors that work with errors.Is.
//
//	c, err := convert.New(os.Getenv("CONVERT_HOST"), os.Getenv("CONVERT_API_KEY"))
//	bundle, err := c.ConvertToBundle(ctx, "deck.pptx", file, convert.Options{})
//	defer bundle.Close()
package convert

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"

	"github.com/viktordanov/rs-convert/client/internal/api"
)

// Client talks to one Convert service. It is safe for concurrent use.
type Client struct {
	api     *api.Client
	tempDir string
}

type config struct {
	httpClient *http.Client
	tempDir    string
}

// Option customises a Client.
type Option func(*config)

// WithHTTPClient sets the HTTP client. The default has no timeout of its own,
// because a conversion may take minutes: bound each call with its context.
func WithHTTPClient(httpClient *http.Client) Option {
	return func(c *config) { c.httpClient = httpClient }
}

// WithTempDir sets the parent directory in which each bundle is unpacked
// and kept until Bundle.Close. The default is the system temp directory.
func WithTempDir(dir string) Option {
	return func(c *config) { c.tempDir = dir }
}

// New creates a client for the service at host, for example
// "http://convert.internal:8080", authenticating with apiKey.
func New(host, apiKey string, opts ...Option) (*Client, error) {
	if host == "" {
		return nil, fmt.Errorf("failed to create convert client: host is empty")
	}
	if apiKey == "" {
		return nil, fmt.Errorf("failed to create convert client: API key is empty")
	}

	cfg := config{httpClient: &http.Client{}}
	for _, opt := range opts {
		opt(&cfg)
	}

	authorize := func(_ context.Context, req *http.Request) error {
		req.Header.Set("Authorization", "Bearer "+apiKey)
		return nil
	}
	generated, err := api.NewClient(host,
		api.WithHTTPClient(cfg.httpClient),
		api.WithRequestEditorFn(authorize),
	)
	if err != nil {
		return nil, fmt.Errorf("failed to create convert client: %w", err)
	}

	return &Client{api: generated, tempDir: cfg.tempDir}, nil
}

// Capabilities reports what the service accepts, its limits and its engine
// versions. The answer is fixed for a deployment, so reading it once is enough.
func (c *Client) Capabilities(ctx context.Context) (*Capabilities, error) {
	resp, err := c.api.GetCapabilities(ctx, &api.GetCapabilitiesParams{})
	if err != nil {
		return nil, fmt.Errorf("failed to get capabilities: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("failed to get capabilities: %w", transformErrorResponse(resp))
	}

	var body api.Capabilities
	if err := json.NewDecoder(resp.Body).Decode(&body); err != nil {
		return nil, fmt.Errorf("failed to decode capabilities: %w", err)
	}
	return transformCapabilitiesToResult(body), nil
}

// Ready reports whether the service is ready to take conversions. A service
// that is starting or draining is not ready, and that is not an error.
func (c *Client) Ready(ctx context.Context) (bool, error) {
	resp, err := c.api.GetReadiness(ctx)
	if err != nil {
		return false, fmt.Errorf("failed to check readiness: %w", err)
	}
	defer resp.Body.Close()

	switch resp.StatusCode {
	case http.StatusOK:
		return true, nil
	case http.StatusServiceUnavailable:
		return false, nil
	default:
		return false, fmt.Errorf("failed to check readiness: %w", transformErrorResponse(resp))
	}
}
