package middleware

import (
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"go.uber.org/zap"
)

func TestMaxBodySize_RejectsLargeBody(t *testing.T) {
	handler := MaxBodySize(zap.NewNop())(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusOK)
	}))

	body := strings.NewReader(strings.Repeat("x", 11*1024*1024))
	req := httptest.NewRequest(http.MethodPost, "/", body)
	req.ContentLength = 11 * 1024 * 1024
	rec := httptest.NewRecorder()
	handler.ServeHTTP(rec, req)

	if rec.Code != http.StatusRequestEntityTooLarge {
		t.Errorf("status = %d, want %d", rec.Code, http.StatusRequestEntityTooLarge)
	}
}

func TestMaxBodySize_AllowsSmallBody(t *testing.T) {
	handler := MaxBodySize(zap.NewNop())(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusOK)
	}))

	req := httptest.NewRequest(http.MethodPost, "/", strings.NewReader("hello"))
	req.ContentLength = 5
	rec := httptest.NewRecorder()
	handler.ServeHTTP(rec, req)

	if rec.Code != http.StatusOK {
		t.Errorf("status = %d, want %d", rec.Code, http.StatusOK)
	}
}

func TestValidateUTF8Body(t *testing.T) {
	tests := []struct {
		name  string
		input string
		valid bool
	}{
		{"valid ascii", "hello world", true},
		{"valid unicode", "こんにちは", true},
		{"valid emoji", "🎉", true},
		{"empty", "", true},
		{"invalid bytes", string([]byte{0xff, 0xfe}), false},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := ValidateUTF8Body(tt.input)
			if got != tt.valid {
				t.Errorf("ValidateUTF8Body(%q) = %v, want %v", tt.input, got, tt.valid)
			}
		})
	}
}
