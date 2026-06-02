package middleware

import (
	"net/http"
	"net/http/httptest"
	"testing"
)

func TestInjectRequestID_GeneratesNew(t *testing.T) {
	var gotID string
	handler := InjectRequestID(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotID = RequestID(r.Context())
		w.WriteHeader(http.StatusOK)
	}))

	req := httptest.NewRequest(http.MethodGet, "/", nil)
	rec := httptest.NewRecorder()
	handler.ServeHTTP(rec, req)

	if gotID == "" {
		t.Error("expected non-empty request ID")
	}
	if rec.Header().Get("X-Request-Id") != gotID {
		t.Errorf("response header = %q, context = %q", rec.Header().Get("X-Request-Id"), gotID)
	}
}

func TestInjectRequestID_PropagatesExisting(t *testing.T) {
	var gotID string
	handler := InjectRequestID(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotID = RequestID(r.Context())
		w.WriteHeader(http.StatusOK)
	}))

	req := httptest.NewRequest(http.MethodGet, "/", nil)
	req.Header.Set("X-Request-Id", "existing-id-123")
	rec := httptest.NewRecorder()
	handler.ServeHTTP(rec, req)

	if gotID != "existing-id-123" {
		t.Errorf("request ID = %q, want %q", gotID, "existing-id-123")
	}
}
