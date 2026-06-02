package middleware

import (
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"github.com/golang-jwt/jwt/v5"
	"go.uber.org/zap"
)

func TestAuth_MissingHeader(t *testing.T) {
	handler := Auth("test-key", "", zap.NewNop())(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusOK)
	}))

	req := httptest.NewRequest(http.MethodGet, "/", nil)
	rec := httptest.NewRecorder()
	handler.ServeHTTP(rec, req)

	if rec.Code != http.StatusUnauthorized {
		t.Errorf("status = %d, want %d", rec.Code, http.StatusUnauthorized)
	}
}

func TestAuth_InvalidScheme(t *testing.T) {
	handler := Auth("test-key", "", zap.NewNop())(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusOK)
	}))

	req := httptest.NewRequest(http.MethodGet, "/", nil)
	req.Header.Set("Authorization", "Basic dGVzdDp0ZXN0")
	rec := httptest.NewRecorder()
	handler.ServeHTTP(rec, req)

	if rec.Code != http.StatusUnauthorized {
		t.Errorf("status = %d, want %d", rec.Code, http.StatusUnauthorized)
	}
}

func TestAuth_ValidAPIKey(t *testing.T) {
	var gotTenant, gotActor string
	handler := Auth("test-key", "", zap.NewNop())(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotTenant = TenantID(r.Context())
		gotActor = ActorID(r.Context())
		w.WriteHeader(http.StatusOK)
	}))

	req := httptest.NewRequest(http.MethodGet, "/", nil)
	req.Header.Set("Authorization", "Bearer test-key")
	rec := httptest.NewRecorder()
	handler.ServeHTTP(rec, req)

	if rec.Code != http.StatusOK {
		t.Errorf("status = %d, want %d", rec.Code, http.StatusOK)
	}
	if gotTenant != "system" {
		t.Errorf("tenant = %q, want %q", gotTenant, "system")
	}
	if gotActor != "api-key" {
		t.Errorf("actor = %q, want %q", gotActor, "api-key")
	}
}

func TestAuth_InvalidAPIKey(t *testing.T) {
	handler := Auth("test-key", "", zap.NewNop())(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusOK)
	}))

	req := httptest.NewRequest(http.MethodGet, "/", nil)
	req.Header.Set("Authorization", "Bearer wrong-key")
	rec := httptest.NewRecorder()
	handler.ServeHTTP(rec, req)

	if rec.Code != http.StatusUnauthorized {
		t.Errorf("status = %d, want %d", rec.Code, http.StatusUnauthorized)
	}
}

func TestAuth_ValidJWT(t *testing.T) {
	secret := "jwt-test-secret"
	claims := &TenantClaims{
		RegisteredClaims: jwt.RegisteredClaims{
			Subject:   "user-123",
			ExpiresAt: jwt.NewNumericDate(time.Now().Add(time.Hour)),
		},
		TenantID: "tenant-abc",
	}
	token := jwt.NewWithClaims(jwt.SigningMethodHS256, claims)
	tokenStr, err := token.SignedString([]byte(secret))
	if err != nil {
		t.Fatalf("failed to sign JWT: %v", err)
	}

	var gotTenant, gotActor string
	handler := Auth("api-key", secret, zap.NewNop())(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotTenant = TenantID(r.Context())
		gotActor = ActorID(r.Context())
		w.WriteHeader(http.StatusOK)
	}))

	req := httptest.NewRequest(http.MethodGet, "/", nil)
	req.Header.Set("Authorization", "Bearer "+tokenStr)
	rec := httptest.NewRecorder()
	handler.ServeHTTP(rec, req)

	if rec.Code != http.StatusOK {
		t.Errorf("status = %d, want %d", rec.Code, http.StatusOK)
	}
	if gotTenant != "tenant-abc" {
		t.Errorf("tenant = %q, want %q", gotTenant, "tenant-abc")
	}
	if gotActor != "user-123" {
		t.Errorf("actor = %q, want %q", gotActor, "user-123")
	}
}

func TestAuth_ExpiredJWT(t *testing.T) {
	secret := "jwt-test-secret"
	claims := &TenantClaims{
		RegisteredClaims: jwt.RegisteredClaims{
			Subject:   "user-123",
			ExpiresAt: jwt.NewNumericDate(time.Now().Add(-time.Hour)),
		},
		TenantID: "tenant-abc",
	}
	token := jwt.NewWithClaims(jwt.SigningMethodHS256, claims)
	tokenStr, err := token.SignedString([]byte(secret))
	if err != nil {
		t.Fatalf("failed to sign JWT: %v", err)
	}

	handler := Auth("api-key", secret, zap.NewNop())(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusOK)
	}))

	req := httptest.NewRequest(http.MethodGet, "/", nil)
	req.Header.Set("Authorization", "Bearer "+tokenStr)
	rec := httptest.NewRecorder()
	handler.ServeHTTP(rec, req)

	if rec.Code != http.StatusUnauthorized {
		t.Errorf("status = %d, want %d", rec.Code, http.StatusUnauthorized)
	}
}
