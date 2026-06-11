package gateway

import (
	"context"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/golang-jwt/jwt/v5"

	"github.com/kennguy3n/knowledge/server/internal/middleware"
	"github.com/kennguy3n/knowledge/server/internal/tenant"
)

// stubQuotaSource returns a fixed quota for every tenant, so a wiring
// test can drive the enforcer past its limit deterministically.
type stubQuotaSource struct{ q tenant.Quota }

func (s stubQuotaSource) TenantQuota(context.Context, string) (tenant.Quota, bool) {
	return s.q, true
}

// mintTenantJWT signs an HS256 token carrying the tenant_id claim that
// Authenticator.parseJWT resolves into the request context.
func mintTenantJWT(t *testing.T, secret, tenantID string) string {
	t.Helper()
	tok := jwt.NewWithClaims(jwt.SigningMethodHS256, jwt.MapClaims{
		"sub":       "user-1",
		"tenant_id": tenantID,
		"exp":       time.Now().Add(time.Hour).Unix(),
	})
	signed, err := tok.SignedString([]byte(secret))
	if err != nil {
		t.Fatalf("sign jwt: %v", err)
	}
	return signed
}

// TestQuotaEnforcerWiredIntoRouter proves the per-tenant quota enforcer
// is actually mounted in the /api/v1 chain (not merely defined): a tenant
// whose requests-per-minute quota is 1 succeeds once and is then shed
// with 429 + Retry-After, while a separate tenant is unaffected.
func TestQuotaEnforcerWiredIntoRouter(t *testing.T) {
	t.Parallel()
	const secret = "jwt-secret"
	enforcer := middleware.NewQuotaEnforcer(
		stubQuotaSource{q: tenant.Quota{RequestsPerMin: 1, SynthesesPerDay: 1, StorageSoftCapBytes: 1 << 40}},
		middleware.QuotaConfig{},
	)
	defer enforcer.Stop()

	h := NewRouter(Deps{
		Substrate: &fakeSub{},
		Auth:      middleware.NewAuthenticator("", secret),
		Quota:     enforcer,
	})

	query := func(tenantID string) *httptest.ResponseRecorder {
		req := httptest.NewRequest(http.MethodPost, "/api/v1/query",
			strings.NewReader(`{"scope_id":"`+scopeUUID+`","query_text":"hi"}`))
		req.Header.Set("Authorization", "Bearer "+mintTenantJWT(t, secret, tenantID))
		rec := httptest.NewRecorder()
		h.ServeHTTP(rec, req)
		return rec
	}

	const tenantA = "11111111-1111-1111-1111-111111111111"
	const tenantB = "22222222-2222-2222-2222-222222222222"

	if rec := query(tenantA); rec.Code != http.StatusOK {
		t.Fatalf("tenant A first request code = %d body=%s", rec.Code, rec.Body.String())
	}
	rec := query(tenantA)
	if rec.Code != http.StatusTooManyRequests {
		t.Fatalf("tenant A second request code = %d, want 429 (quota not enforced — middleware not wired?)", rec.Code)
	}
	if rec.Header().Get("Retry-After") == "" {
		t.Fatalf("429 response missing Retry-After header")
	}

	// Fairness: a different tenant is bucketed independently and is not
	// throttled by tenant A's consumption.
	if rec := query(tenantB); rec.Code != http.StatusOK {
		t.Fatalf("tenant B request code = %d, want 200 (cross-tenant interference)", rec.Code)
	}
}
