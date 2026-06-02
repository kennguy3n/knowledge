// Package httpx provides shared HTTP plumbing: a hardened HTTP client
// (TLS 1.3 minimum), JSON request/response helpers, and a structured
// API error type used uniformly across the gateway and services.
package httpx

import (
	"crypto/tls"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"net/http"
	"time"
)

// NewClient returns an [*http.Client] hardened for service-to-service
// calls: TLS 1.3 minimum, bounded dial/idle timeouts, and a sane
// overall request timeout. Loopback plaintext (http://127.0.0.1) is
// still permitted — the TLS floor only applies once TLS is used.
func NewClient(timeout time.Duration) *http.Client {
	if timeout <= 0 {
		timeout = 30 * time.Second
	}
	return &http.Client{
		Timeout: timeout,
		Transport: &http.Transport{
			DialContext: (&net.Dialer{
				Timeout:   5 * time.Second,
				KeepAlive: 30 * time.Second,
			}).DialContext,
			TLSClientConfig:       &tls.Config{MinVersion: tls.VersionTLS13},
			MaxIdleConns:          100,
			MaxIdleConnsPerHost:   16,
			IdleConnTimeout:       90 * time.Second,
			TLSHandshakeTimeout:   5 * time.Second,
			ExpectContinueTimeout: time.Second,
			ForceAttemptHTTP2:     true,
		},
	}
}

// Error is a structured API error carrying an HTTP status, a stable
// machine-readable kind, and a human-readable message.
type Error struct {
	// Status is the HTTP status code to emit.
	Status int `json:"-"`
	// Kind is a stable, machine-readable error tag.
	Kind string `json:"kind"`
	// Message is a human-readable description (no PII).
	Message string `json:"message"`
}

// Error implements the error interface.
func (e *Error) Error() string {
	return fmt.Sprintf("%s (%d): %s", e.Kind, e.Status, e.Message)
}

// NewError constructs an [*Error].
func NewError(status int, kind, msg string) *Error {
	return &Error{Status: status, Kind: kind, Message: msg}
}

// Common constructors for frequently used statuses.

// BadRequest builds a 400 error.
func BadRequest(msg string) *Error { return NewError(http.StatusBadRequest, "BadRequest", msg) }

// Unauthorized builds a 401 error.
func Unauthorized(msg string) *Error { return NewError(http.StatusUnauthorized, "Unauthorized", msg) }

// Forbidden builds a 403 error.
func Forbidden(msg string) *Error { return NewError(http.StatusForbidden, "Forbidden", msg) }

// NotFound builds a 404 error.
func NotFound(msg string) *Error { return NewError(http.StatusNotFound, "NotFound", msg) }

// Internal builds a 500 error.
func Internal(msg string) *Error { return NewError(http.StatusInternalServerError, "Internal", msg) }

// WriteJSON serialises v as JSON with the given status code.
func WriteJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(status)
	if v == nil {
		return
	}
	_ = json.NewEncoder(w).Encode(v)
}

// WriteError writes err as a JSON error body. Non-[*Error] values are
// rendered as opaque 500s so internal details never leak to clients.
func WriteError(w http.ResponseWriter, err error) {
	var apiErr *Error
	if !errors.As(err, &apiErr) {
		apiErr = Internal("internal server error")
	}
	WriteJSON(w, apiErr.Status, apiErr)
}

// DecodeJSON reads and strictly decodes a JSON request body into dst,
// rejecting unknown fields and trailing data. The body must already
// be size-limited by upstream middleware.
func DecodeJSON(r *http.Request, dst any) error {
	dec := json.NewDecoder(r.Body)
	dec.DisallowUnknownFields()
	if err := dec.Decode(dst); err != nil {
		return BadRequest("invalid JSON body: " + err.Error())
	}
	if dec.More() {
		return BadRequest("request body must contain a single JSON value")
	}
	return nil
}
