package middleware

import (
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/golang-jwt/jwt/v5"
	"go.uber.org/zap"
)

func ok(w http.ResponseWriter, _ *http.Request) { w.WriteHeader(http.StatusOK) }

func TestInjectRequestID(t *testing.T) {
	t.Parallel()
	var seen string
	h := InjectRequestID(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		seen = RequestID(r.Context())
		w.WriteHeader(http.StatusOK)
	}))

	// Mints when absent.
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/", nil))
	if seen == "" || rec.Header().Get(RequestIDHeader) != seen {
		t.Fatalf("minted id mismatch: ctx=%q hdr=%q", seen, rec.Header().Get(RequestIDHeader))
	}

	// Reuses inbound.
	req := httptest.NewRequest(http.MethodGet, "/", nil)
	req.Header.Set(RequestIDHeader, "abc-123")
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if seen != "abc-123" {
		t.Fatalf("inbound id not reused: %q", seen)
	}
}

func TestRecover(t *testing.T) {
	t.Parallel()
	h := Recover(zap.NewNop())(http.HandlerFunc(func(http.ResponseWriter, *http.Request) {
		panic("boom")
	}))
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/", nil))
	if rec.Code != http.StatusInternalServerError {
		t.Fatalf("code = %d", rec.Code)
	}
}

func TestCORS(t *testing.T) {
	t.Parallel()
	h := CORS([]string{"https://allowed.example"})(http.HandlerFunc(ok))

	req := httptest.NewRequest(http.MethodGet, "/", nil)
	req.Header.Set("Origin", "https://allowed.example")
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if rec.Header().Get("Access-Control-Allow-Origin") != "https://allowed.example" {
		t.Errorf("allowed origin not echoed")
	}

	req = httptest.NewRequest(http.MethodGet, "/", nil)
	req.Header.Set("Origin", "https://evil.example")
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if rec.Header().Get("Access-Control-Allow-Origin") != "" {
		t.Errorf("disallowed origin echoed")
	}

	// Preflight short-circuits.
	req = httptest.NewRequest(http.MethodOptions, "/", nil)
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if rec.Code != http.StatusNoContent {
		t.Errorf("preflight code = %d", rec.Code)
	}
}

func TestAuthenticatorDevMode(t *testing.T) {
	t.Parallel()
	var p Principal
	h := NewAuthenticator("", "").Middleware(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		p, _ = PrincipalFrom(r.Context())
		w.WriteHeader(http.StatusOK)
	}))
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/", nil))
	if rec.Code != http.StatusOK || !p.Service {
		t.Fatalf("dev mode failed: code=%d principal=%+v", rec.Code, p)
	}
}

func TestAuthenticatorAPIKey(t *testing.T) {
	t.Parallel()
	h := NewAuthenticator("secret-key", "").Middleware(http.HandlerFunc(ok))

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/", nil))
	if rec.Code != http.StatusUnauthorized {
		t.Errorf("missing token: code = %d", rec.Code)
	}

	req := httptest.NewRequest(http.MethodGet, "/", nil)
	req.Header.Set("Authorization", "Bearer secret-key")
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Errorf("valid key rejected: code = %d", rec.Code)
	}

	req = httptest.NewRequest(http.MethodGet, "/", nil)
	req.Header.Set("Authorization", "Bearer wrong")
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if rec.Code != http.StatusUnauthorized {
		t.Errorf("wrong key accepted: code = %d", rec.Code)
	}
}

func TestAuthenticatorJWT(t *testing.T) {
	t.Parallel()
	secret := "jwt-signing-secret"
	var gotTenant string
	h := NewAuthenticator("", secret).Middleware(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotTenant = TenantID(r.Context())
		w.WriteHeader(http.StatusOK)
	}))

	tok := jwt.NewWithClaims(jwt.SigningMethodHS256, jwt.MapClaims{
		"sub": "user-1", "tenant_id": "tenant-9",
	})
	signed, err := tok.SignedString([]byte(secret))
	if err != nil {
		t.Fatal(err)
	}
	req := httptest.NewRequest(http.MethodGet, "/", nil)
	req.Header.Set("Authorization", "Bearer "+signed)
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if rec.Code != http.StatusOK || gotTenant != "tenant-9" {
		t.Fatalf("valid jwt failed: code=%d tenant=%q", rec.Code, gotTenant)
	}

	// Wrong signature.
	bad := jwt.NewWithClaims(jwt.SigningMethodHS256, jwt.MapClaims{"sub": "user-1"})
	badSigned, _ := bad.SignedString([]byte("other-secret"))
	req = httptest.NewRequest(http.MethodGet, "/", nil)
	req.Header.Set("Authorization", "Bearer "+badSigned)
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if rec.Code != http.StatusUnauthorized {
		t.Errorf("bad signature accepted: code = %d", rec.Code)
	}
}

func TestBodyLimit(t *testing.T) {
	t.Parallel()
	h := BodyLimit(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, err := io.ReadAll(r.Body)
		if err != nil {
			http.Error(w, "too big", http.StatusRequestEntityTooLarge)
			return
		}
		w.WriteHeader(http.StatusOK)
	}))
	big := strings.NewReader(strings.Repeat("a", int(11<<20)))
	req := httptest.NewRequest(http.MethodPost, "/", big)
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if rec.Code != http.StatusRequestEntityTooLarge {
		t.Fatalf("oversize body not rejected: code = %d", rec.Code)
	}
}

func TestRateLimiterPerIP(t *testing.T) {
	t.Parallel()
	rl := NewRateLimiter(1, 1, 1) // burst 1
	h := rl.PerIPMiddleware(http.HandlerFunc(ok))

	req := httptest.NewRequest(http.MethodGet, "/", nil)
	req.RemoteAddr = "10.0.0.1:1234"
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Fatalf("first request blocked: %d", rec.Code)
	}
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if rec.Code != http.StatusTooManyRequests {
		t.Fatalf("burst exceeded not blocked: %d", rec.Code)
	}

	// A different source IP has its own bucket and is not affected.
	other := httptest.NewRequest(http.MethodGet, "/", nil)
	other.RemoteAddr = "10.0.0.2:1234"
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, other)
	if rec.Code != http.StatusOK {
		t.Fatalf("second IP blocked by first IP's bucket: %d", rec.Code)
	}
}

func TestRateLimiterPerTenant(t *testing.T) {
	t.Parallel()
	rl := NewRateLimiter(1, 1, 1) // burst 1
	h := rl.PerTenantMiddleware(http.HandlerFunc(ok))

	// Requests carrying a tenant share that tenant's bucket regardless
	// of source IP.
	withTenant := func(ip string) *http.Request {
		req := httptest.NewRequest(http.MethodGet, "/", nil)
		req.RemoteAddr = ip
		ctx := withPrincipal(req.Context(), Principal{Subject: "u", TenantID: "tenant-a"})
		return req.WithContext(ctx)
	}
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, withTenant("10.0.0.1:1"))
	if rec.Code != http.StatusOK {
		t.Fatalf("first tenant request blocked: %d", rec.Code)
	}
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, withTenant("10.0.0.2:1"))
	if rec.Code != http.StatusTooManyRequests {
		t.Fatalf("tenant burst exceeded not blocked: %d", rec.Code)
	}

	// A request with no tenant in context bypasses the per-tenant gate.
	rec = httptest.NewRecorder()
	noTenant := httptest.NewRequest(http.MethodGet, "/", nil)
	noTenant.RemoteAddr = "10.0.0.3:1"
	h.ServeHTTP(rec, noTenant)
	if rec.Code != http.StatusOK {
		t.Fatalf("untenanted request blocked by per-tenant limiter: %d", rec.Code)
	}
}
