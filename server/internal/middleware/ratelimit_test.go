package middleware

import (
	"net/http"
	"net/http/httptest"
	"testing"

	"go.uber.org/zap"
)

func TestRateLimiter_AllowsWithinLimit(t *testing.T) {
	rl := NewRateLimiter(10)
	for i := 0; i < 10; i++ {
		if !rl.allow("test-key") {
			t.Errorf("request %d should be allowed", i)
		}
	}
}

func TestRateLimiter_DeniesOverLimit(t *testing.T) {
	rl := NewRateLimiter(1)
	if !rl.allow("test-key") {
		t.Fatal("first request should be allowed")
	}
	if rl.allow("test-key") {
		t.Fatal("second request should be denied (bucket exhausted)")
	}
}

func TestRateLimiter_SeparateKeys(t *testing.T) {
	rl := NewRateLimiter(1)
	if !rl.allow("key-a") {
		t.Fatal("key-a first request should be allowed")
	}
	if !rl.allow("key-b") {
		t.Fatal("key-b first request should be allowed (separate bucket)")
	}
}

func TestRateLimitMiddleware_Returns429(t *testing.T) {
	perIP := NewRateLimiter(1)
	perTenant := NewRateLimiter(1000)

	handler := RateLimit(perIP, perTenant, zap.NewNop())(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusOK)
	}))

	// First request succeeds.
	req := httptest.NewRequest(http.MethodGet, "/", nil)
	req.RemoteAddr = "192.168.1.1:12345"
	rec := httptest.NewRecorder()
	handler.ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Errorf("first request status = %d, want %d", rec.Code, http.StatusOK)
	}

	// Second request from same IP should be rate limited.
	rec2 := httptest.NewRecorder()
	handler.ServeHTTP(rec2, req)
	if rec2.Code != http.StatusTooManyRequests {
		t.Errorf("second request status = %d, want %d", rec2.Code, http.StatusTooManyRequests)
	}
}
