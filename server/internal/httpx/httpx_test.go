package httpx

import (
	"crypto/tls"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func TestNewClientTLSFloor(t *testing.T) {
	t.Parallel()
	c := NewClient(0)
	tr, ok := c.Transport.(*http.Transport)
	if !ok {
		t.Fatal("transport is not *http.Transport")
	}
	if tr.TLSClientConfig.MinVersion != tls.VersionTLS13 {
		t.Errorf("MinVersion = %x, want TLS 1.3", tr.TLSClientConfig.MinVersion)
	}
}

func TestWriteJSON(t *testing.T) {
	t.Parallel()
	rec := httptest.NewRecorder()
	WriteJSON(rec, http.StatusCreated, map[string]string{"k": "v"})
	if rec.Code != http.StatusCreated {
		t.Errorf("code = %d", rec.Code)
	}
	if !strings.Contains(rec.Body.String(), `"k":"v"`) {
		t.Errorf("body = %q", rec.Body.String())
	}
}

func TestWriteErrorHidesInternal(t *testing.T) {
	t.Parallel()
	rec := httptest.NewRecorder()
	WriteError(rec, errPlain("boom with secret path /etc/passwd"))
	if rec.Code != http.StatusInternalServerError {
		t.Errorf("code = %d", rec.Code)
	}
	if strings.Contains(rec.Body.String(), "secret path") {
		t.Errorf("internal detail leaked: %q", rec.Body.String())
	}
}

func TestWriteErrorPreservesAPIError(t *testing.T) {
	t.Parallel()
	rec := httptest.NewRecorder()
	WriteError(rec, BadRequest("missing scope_id"))
	if rec.Code != http.StatusBadRequest {
		t.Errorf("code = %d", rec.Code)
	}
	if !strings.Contains(rec.Body.String(), "missing scope_id") {
		t.Errorf("message lost: %q", rec.Body.String())
	}
}

func TestErrorConstructors(t *testing.T) {
	t.Parallel()
	cases := []struct {
		err  *Error
		want int
		kind string
	}{
		{BadRequest("x"), http.StatusBadRequest, "BadRequest"},
		{Unauthorized("x"), http.StatusUnauthorized, "Unauthorized"},
		{Forbidden("x"), http.StatusForbidden, "Forbidden"},
		{NotFound("x"), http.StatusNotFound, "NotFound"},
		{TooManyRequests("x"), http.StatusTooManyRequests, "TooManyRequests"},
		{Internal("x"), http.StatusInternalServerError, "Internal"},
	}
	for _, c := range cases {
		if c.err.Status != c.want || c.err.Kind != c.kind {
			t.Errorf("got (%d,%q), want (%d,%q)", c.err.Status, c.err.Kind, c.want, c.kind)
		}
	}
}

func TestDecodeJSONStrict(t *testing.T) {
	t.Parallel()
	type payload struct {
		A int `json:"a"`
	}
	good := httptest.NewRequest(http.MethodPost, "/", strings.NewReader(`{"a":1}`))
	var p payload
	if err := DecodeJSON(good, &p); err != nil {
		t.Fatalf("good body rejected: %v", err)
	}
	unknown := httptest.NewRequest(http.MethodPost, "/", strings.NewReader(`{"a":1,"b":2}`))
	if err := DecodeJSON(unknown, &p); err == nil {
		t.Fatal("unknown field accepted")
	}
	trailing := httptest.NewRequest(http.MethodPost, "/", strings.NewReader(`{"a":1}{"a":2}`))
	if err := DecodeJSON(trailing, &p); err == nil {
		t.Fatal("trailing data accepted")
	}
}

type errPlain string

func (e errPlain) Error() string { return string(e) }
