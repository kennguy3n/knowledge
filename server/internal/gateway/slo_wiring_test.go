package gateway

import (
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/golang-jwt/jwt/v5"

	"github.com/kennguy3n/knowledge/server/internal/metrics"
	"github.com/kennguy3n/knowledge/server/internal/middleware"
)

func mintSLOTenantJWT(t *testing.T, secret, tenantID string) string {
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

// sloCounterValue reads a knowledge_tenant_slo_requests_total series.
func sloCounterValue(t *testing.T, labels map[string]string) float64 {
	t.Helper()
	mfs, err := metrics.Registry.Gather()
	if err != nil {
		t.Fatalf("gather: %v", err)
	}
	for _, mf := range mfs {
		if mf.GetName() != "knowledge_tenant_slo_requests_total" {
			continue
		}
		for _, m := range mf.GetMetric() {
			match := true
			for k, v := range labels {
				found := false
				for _, lp := range m.GetLabel() {
					if lp.GetName() == k && lp.GetValue() == v {
						found = true
						break
					}
				}
				if !found {
					match = false
					break
				}
			}
			if match {
				return m.GetCounter().GetValue()
			}
		}
	}
	return 0
}

// TestSLOMiddlewareWiredIntoRouter proves the per-tenant SLO middleware
// is mounted in the real /api/v1 chain: an authenticated query emits a
// knowledge_tenant_slo_requests_total{route="query",outcome="success"}
// sample for the resolved tenant.
func TestSLOMiddlewareWiredIntoRouter(t *testing.T) {
	// Not parallel: asserts on a process-global metric registry.
	metrics.TenantRequestSLO.Reset()
	metrics.TenantRequestDuration.Reset()

	const secret = "jwt-secret"
	const tenantID = "33333333-3333-3333-3333-333333333333"
	h := NewRouter(Deps{Substrate: &fakeSub{}, Auth: middleware.NewAuthenticator("", secret)})

	req := httptest.NewRequest(http.MethodPost, "/api/v1/query",
		strings.NewReader(`{"scope_id":"`+scopeUUID+`","query_text":"hi"}`))
	req.Header.Set("Authorization", "Bearer "+mintSLOTenantJWT(t, secret, tenantID))
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Fatalf("query code = %d body=%s", rec.Code, rec.Body.String())
	}

	if v := sloCounterValue(t, map[string]string{
		"route": "query", "tenant_id": tenantID, "outcome": "success",
	}); v != 1 {
		t.Fatalf("want 1 SLO success sample for the tenant (middleware not wired?), got %v", v)
	}
}
