package convert

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"

	"github.com/viktordanov/rs-convert/client/internal/api"
)

// Every error returned for a non-200 response wraps exactly one of these, so
// callers branch with errors.Is.
var (
	// Final: the same request will fail the same way.
	ErrInvalidRequest    = errors.New("invalid request")
	ErrUnauthorized      = errors.New("unauthorized")
	ErrFileTooLarge      = errors.New("file too large")
	ErrUnsupportedFormat = errors.New("unsupported format")
	ErrPasswordRequired  = errors.New("password required")
	ErrInvalidPassword   = errors.New("invalid password")
	ErrLimitExceeded     = errors.New("limit exceeded")
	ErrConversionFailed  = errors.New("conversion failed")

	// The job ran out of time. Retry once at most.
	ErrTimeout = errors.New("conversion timed out")

	// Transient: retry later.
	ErrBusy        = errors.New("service busy")
	ErrUnavailable = errors.New("service unavailable")
	ErrInternal    = errors.New("internal server error")
)

// Retryable reports whether err is transient, so that the same request may
// succeed later. A queue handler can return such an error and let its backoff
// retry the job.
func Retryable(err error) bool {
	return errors.Is(err, ErrBusy) || errors.Is(err, ErrUnavailable) || errors.Is(err, ErrInternal)
}

// errorBodyLimit caps how much of an error response is read.
const errorBodyLimit = 64 << 10

// transformErrorResponse turns a non-200 response into an error that wraps
// one sentinel and carries the server's message and the request ID.
func transformErrorResponse(resp *http.Response) error {
	// The body may be missing or not JSON, for example when a proxy answers.
	// The status code alone then decides the sentinel.
	var body api.Error
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, errorBodyLimit))
	_ = json.Unmarshal(raw, &body)

	message := body.Error
	if message == "" {
		message = http.StatusText(resp.StatusCode)
	}

	sentinel := sentinelFor(resp.StatusCode, string(body.Code))
	return fmt.Errorf("%w: %s (status %d, request %s)",
		sentinel, message, resp.StatusCode, resp.Header.Get("X-Request-ID"))
}

// sentinelFor picks by the API's error code first and falls back to the status.
func sentinelFor(status int, code string) error {
	switch code {
	case "BAD_REQUEST", "VALIDATION_ERROR":
		return ErrInvalidRequest
	case "UNAUTHORIZED":
		return ErrUnauthorized
	case "FILE_TOO_LARGE":
		return ErrFileTooLarge
	case "UNSUPPORTED_FORMAT":
		return ErrUnsupportedFormat
	case "PASSWORD_REQUIRED":
		return ErrPasswordRequired
	case "INVALID_PASSWORD":
		return ErrInvalidPassword
	case "LIMIT_EXCEEDED":
		return ErrLimitExceeded
	case "CONVERSION_FAILED":
		return ErrConversionFailed
	case "CONVERSION_TIMEOUT":
		return ErrTimeout
	case "TOO_MANY_REQUESTS":
		return ErrBusy
	case "SERVICE_UNAVAILABLE":
		return ErrUnavailable
	case "INTERNAL_ERROR":
		return ErrInternal
	}

	switch status {
	case http.StatusBadRequest, http.StatusNotFound, http.StatusUnprocessableEntity:
		return ErrInvalidRequest
	case http.StatusUnauthorized, http.StatusForbidden:
		return ErrUnauthorized
	case http.StatusRequestEntityTooLarge:
		return ErrFileTooLarge
	case http.StatusUnsupportedMediaType:
		return ErrUnsupportedFormat
	case http.StatusTooManyRequests:
		return ErrBusy
	case http.StatusBadGateway, http.StatusServiceUnavailable:
		return ErrUnavailable
	case http.StatusGatewayTimeout:
		return ErrTimeout
	default:
		return ErrInternal
	}
}
