package metrics_test

import (
	"net/http"
	"net/http/httptest"
	"testing"

	"github.com/go-chi/chi/v5"

	"github.com/kennguy3n/knowledge/server/internal/metrics"
)

// histogramCount returns the observation count of the histogram series
// matching the given labels, and whether such a series exists.
func histogramCount(t *testing.T, name string, labels map[string]string) (uint64, bool) {
	t.Helper()
	mfs, err := metrics.Registry.Gather()
	if err != nil {
		t.Fatalf("gather: %v", err)
	}
	for _, mf := range mfs {
		if mf.GetName() != name {
			continue
		}
		for _, m := range mf.GetMetric() {
			if matchLabels(m.GetLabel(), labels) {
				return m.GetHistogram().GetSampleCount(), true
			}
		}
	}
	return 0, false
}

// sloRouter builds a chi router with the SLO middleware mounted after a
// tenant-injecting middleware (simulating post-auth ordering), serving a
// handler that returns the given status.
func sloRouter(tid string, status int) http.Handler {
	r := chi.NewRouter()
	r.Route("/api/v1", func(sub chi.Router) {
		sub.Use(injectTenant(tid))
		sub.Use(metrics.SLOMiddleware)
		sub.Post("/ingest", func(w http.ResponseWriter, _ *http.Request) { w.WriteHeader(status) })
		sub.Post("/query", func(w http.ResponseWriter, _ *http.Request) { w.WriteHeader(status) })
		sub.Post("/synthesis/trigger", func(w http.ResponseWriter, _ *http.Request) { w.WriteHeader(status) })
		sub.Get("/synthesis/{id}/status", func(w http.ResponseWriter, _ *http.Request) { w.WriteHeader(status) })
		// A sibling route sharing the "ingest" prefix but a different path
		// segment; it must NOT be bucketed into the ingest SLO class.
		sub.Get("/ingest-log", func(w http.ResponseWriter, _ *http.Request) { w.WriteHeader(status) })
		sub.Get("/healthz", func(w http.ResponseWriter, _ *http.Request) { w.WriteHeader(status) })
	})
	return r
}

func serve(h http.Handler, method, path string) {
	req := httptest.NewRequest(method, path, nil)
	h.ServeHTTP(httptest.NewRecorder(), req)
}

func TestSLOMiddleware_EmitsLatencyAndOutcomeLabels(t *testing.T) {
	metrics.TenantRequestDuration.Reset()
	metrics.TenantRequestSLO.Reset()

	h := sloRouter("slo-t1", http.StatusOK)
	serve(h, http.MethodPost, "/api/v1/ingest")
	serve(h, http.MethodPost, "/api/v1/query")
	serve(h, http.MethodPost, "/api/v1/synthesis/trigger")

	for _, class := range []string{"ingest", "query", "synthesis"} {
		cnt, ok := histogramCount(t, "knowledge_tenant_request_duration_seconds", map[string]string{
			"route": class, "tenant_id": "slo-t1",
		})
		if !ok || cnt != 1 {
			t.Fatalf("class %s: want 1 latency observation, ok=%v cnt=%d", class, ok, cnt)
		}
		if v := gatherLabeledCounter(t, "knowledge_tenant_slo_requests_total", map[string]string{
			"route": class, "tenant_id": "slo-t1", "outcome": "success",
		}); v != 1 {
			t.Fatalf("class %s: want 1 success, got %f", class, v)
		}
	}
}

// TestSLOMiddleware_ReusesTenantMiddlewareLabel verifies the production
// ordering (TenantMiddleware then SLOMiddleware): SLOMiddleware reuses
// the cardinality-capped tenant label that TenantMiddleware cached in
// the request context, so both the global per-tenant counter and the
// SLO series are emitted under the same label (the cap is resolved once,
// not re-locked per recorder).
func TestSLOMiddleware_ReusesTenantMiddlewareLabel(t *testing.T) {
	metrics.TenantRequestSLO.Reset()
	metrics.TenantRequestsTotal.Reset()

	r := chi.NewRouter()
	r.Route("/api/v1", func(sub chi.Router) {
		sub.Use(injectTenant("slo-shared"))
		sub.Use(metrics.TenantMiddleware)
		sub.Use(metrics.SLOMiddleware)
		sub.Post("/query", func(w http.ResponseWriter, _ *http.Request) { w.WriteHeader(http.StatusOK) })
	})
	serve(r, http.MethodPost, "/api/v1/query")

	if v := gatherLabeledCounter(t, "knowledge_tenant_requests_total", map[string]string{
		"tenant_id": "slo-shared",
	}); v != 1 {
		t.Fatalf("want 1 tenant request under shared label, got %f", v)
	}
	if v := gatherLabeledCounter(t, "knowledge_tenant_slo_requests_total", map[string]string{
		"route": "query", "tenant_id": "slo-shared", "outcome": "success",
	}); v != 1 {
		t.Fatalf("want 1 SLO success under the same shared label, got %f", v)
	}
}

func TestSLOMiddleware_5xxCountedAsError(t *testing.T) {
	metrics.TenantRequestSLO.Reset()

	h := sloRouter("slo-err", http.StatusInternalServerError)
	serve(h, http.MethodPost, "/api/v1/query")

	if v := gatherLabeledCounter(t, "knowledge_tenant_slo_requests_total", map[string]string{
		"route": "query", "tenant_id": "slo-err", "outcome": "error",
	}); v != 1 {
		t.Fatalf("want 1 error outcome, got %f", v)
	}
	if v := gatherLabeledCounter(t, "knowledge_tenant_slo_requests_total", map[string]string{
		"route": "query", "tenant_id": "slo-err", "outcome": "success",
	}); v != 0 {
		t.Fatalf("5xx must not count as success, got %f", v)
	}
}

func TestSLOMiddleware_4xxIsSuccessForAvailability(t *testing.T) {
	metrics.TenantRequestSLO.Reset()

	h := sloRouter("slo-4xx", http.StatusTooManyRequests)
	serve(h, http.MethodPost, "/api/v1/ingest")

	// Availability SLO counts only 5xx as failures; client errors (4xx)
	// are not charged against the service error budget.
	if v := gatherLabeledCounter(t, "knowledge_tenant_slo_requests_total", map[string]string{
		"route": "ingest", "tenant_id": "slo-4xx", "outcome": "success",
	}); v != 1 {
		t.Fatalf("4xx should be counted as success, got %f", v)
	}
}

func TestSLOMiddleware_SSEExcludedFromLatencyButCounted(t *testing.T) {
	metrics.TenantRequestDuration.Reset()
	metrics.TenantRequestSLO.Reset()

	h := sloRouter("slo-sse", http.StatusOK)
	serve(h, http.MethodGet, "/api/v1/synthesis/abc/status")

	// Counted for error-rate...
	if v := gatherLabeledCounter(t, "knowledge_tenant_slo_requests_total", map[string]string{
		"route": "synthesis", "tenant_id": "slo-sse", "outcome": "success",
	}); v != 1 {
		t.Fatalf("SSE status should be counted, got %f", v)
	}
	// ...but excluded from the latency histogram (would inflate p99).
	if cnt, ok := histogramCount(t, "knowledge_tenant_request_duration_seconds", map[string]string{
		"route": "synthesis", "tenant_id": "slo-sse",
	}); ok && cnt != 0 {
		t.Fatalf("SSE status must not be observed in latency histogram, got %d", cnt)
	}
}

func TestSLOMiddleware_NonSLORouteIgnored(t *testing.T) {
	metrics.TenantRequestSLO.Reset()

	h := sloRouter("slo-skip", http.StatusOK)
	serve(h, http.MethodGet, "/api/v1/healthz")

	for _, class := range []string{"ingest", "query", "synthesis"} {
		if v := gatherLabeledCounter(t, "knowledge_tenant_slo_requests_total", map[string]string{
			"route": class, "tenant_id": "slo-skip", "outcome": "success",
		}); v != 0 {
			t.Fatalf("non-SLO route must not emit SLO metric for class %s, got %f", class, v)
		}
	}
}

func TestSLOMiddleware_SiblingPrefixNotMisbucketed(t *testing.T) {
	metrics.TenantRequestSLO.Reset()

	h := sloRouter("slo-sib", http.StatusOK)
	serve(h, http.MethodGet, "/api/v1/ingest-log")

	// "/api/v1/ingest-log" shares the "/api/v1/ingest" prefix but is a
	// distinct route, so segment-aware classification must skip it
	// rather than charge it to the ingest SLO class.
	if v := gatherLabeledCounter(t, "knowledge_tenant_slo_requests_total", map[string]string{
		"route": "ingest", "tenant_id": "slo-sib", "outcome": "success",
	}); v != 0 {
		t.Fatalf("sibling prefix route must not be bucketed as ingest, got %f", v)
	}
}

func TestSLOMiddleware_NoTenantSkipped(t *testing.T) {
	metrics.TenantRequestSLO.Reset()

	h := sloRouter("", http.StatusOK) // empty tenant => service principal
	serve(h, http.MethodPost, "/api/v1/query")

	if v := gatherLabeledCounter(t, "knowledge_tenant_slo_requests_total", map[string]string{
		"route": "query", "outcome": "success",
	}); v != 0 {
		t.Fatalf("request without resolved tenant must be skipped, got %f", v)
	}
}
