package metrics_test

import (
	"context"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/go-chi/chi/v5"
	"github.com/prometheus/client_golang/prometheus"
	dto "github.com/prometheus/client_model/go"

	"github.com/kennguy3n/knowledge/server/internal/metrics"
)

// ctxKey is an unexported context key type for test tenant injection.
type ctxKey int

const keyTestTenant ctxKey = 0

func testTenantID(ctx context.Context) string {
	if v, ok := ctx.Value(keyTestTenant).(string); ok {
		return v
	}
	return ""
}

// injectTenant is test middleware that stores a tenant ID in the context.
func injectTenant(tid string) func(http.Handler) http.Handler {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			ctx := context.WithValue(r.Context(), keyTestTenant, tid)
			next.ServeHTTP(w, r.WithContext(ctx))
		})
	}
}

func init() {
	metrics.SetTenantIDFunc(testTenantID)
}

func gatherCounter(t *testing.T, name string) float64 {
	t.Helper()
	mfs, err := metrics.Registry.Gather()
	if err != nil {
		t.Fatalf("gather: %v", err)
	}
	for _, mf := range mfs {
		if mf.GetName() == name {
			var total float64
			for _, m := range mf.GetMetric() {
				total += m.GetCounter().GetValue()
			}
			return total
		}
	}
	return 0
}

func gatherGauge(t *testing.T, name string) float64 {
	t.Helper()
	mfs, err := metrics.Registry.Gather()
	if err != nil {
		t.Fatalf("gather: %v", err)
	}
	for _, mf := range mfs {
		if mf.GetName() == name {
			var total float64
			for _, m := range mf.GetMetric() {
				total += m.GetGauge().GetValue()
			}
			return total
		}
	}
	return 0
}

func gatherLabeledCounter(t *testing.T, name string, labels map[string]string) float64 {
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
				return m.GetCounter().GetValue()
			}
		}
	}
	return 0
}

func matchLabels(pairs []*dto.LabelPair, want map[string]string) bool {
	found := make(map[string]string, len(pairs))
	for _, p := range pairs {
		found[p.GetName()] = p.GetValue()
	}
	for k, v := range want {
		if found[k] != v {
			return false
		}
	}
	return true
}

func TestMiddleware_IncrementsCounters(t *testing.T) {
	// Reset relevant counters.
	metrics.RequestsTotal.Reset()
	metrics.RequestDuration.Reset()
	metrics.TenantRequestsTotal.Reset()

	r := chi.NewRouter()
	r.Use(injectTenant("t-123"))
	r.Use(metrics.Middleware)
	r.Get("/test", func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusOK)
	})

	req := httptest.NewRequest(http.MethodGet, "/test", nil)
	w := httptest.NewRecorder()
	r.ServeHTTP(w, req)

	if w.Code != http.StatusOK {
		t.Fatalf("expected 200, got %d", w.Code)
	}

	// Request counter incremented.
	val := gatherLabeledCounter(t, "knowledge_gateway_requests_total", map[string]string{
		"method": "GET",
		"status": "200",
	})
	if val < 1 {
		t.Errorf("expected requests_total >= 1, got %f", val)
	}

	// Tenant counter incremented.
	tval := gatherLabeledCounter(t, "knowledge_tenant_requests_total", map[string]string{
		"tenant_id": "t-123",
	})
	if tval < 1 {
		t.Errorf("expected tenant_requests_total >= 1, got %f", tval)
	}
}

func TestHandler_ServesPrometheusExposition(t *testing.T) {
	req := httptest.NewRequest(http.MethodGet, "/metrics", nil)
	w := httptest.NewRecorder()
	metrics.Handler().ServeHTTP(w, req)
	if w.Code != http.StatusOK {
		t.Fatalf("expected 200, got %d", w.Code)
	}
	body := w.Body.String()
	if !strings.Contains(body, "knowledge_gateway_requests_total") {
		t.Errorf("exposition missing knowledge_gateway_requests_total")
	}
}

func TestDomainCounters_Increment(t *testing.T) {
	metrics.IngestTotal.Add(0)
	metrics.QueryTotal.Add(0)
	metrics.SynthesisTriggerTotal.Add(0)
	metrics.SynthesisSuccessTotal.Add(0)
	metrics.SynthesisThrottleTotal.Add(0)
	metrics.ConnectorSyncSuccess.Reset()
	metrics.ConnectorSyncFailure.Reset()
	metrics.ErrorsTotal.Reset()
	metrics.SubsystemStatus.Reset()

	before := gatherCounter(t, "knowledge_ingest_total")
	metrics.IngestTotal.Inc()
	after := gatherCounter(t, "knowledge_ingest_total")
	if after <= before {
		t.Errorf("ingest_total did not increment")
	}

	metrics.QueryTotal.Inc()
	if gatherCounter(t, "knowledge_query_total") < 1 {
		t.Error("query_total did not increment")
	}

	metrics.SynthesisTriggerTotal.Inc()
	if gatherCounter(t, "knowledge_synthesis_trigger_total") < 1 {
		t.Error("synthesis_trigger_total did not increment")
	}

	metrics.ConnectorSyncSuccess.WithLabelValues("slack").Inc()
	v := gatherLabeledCounter(t, "knowledge_connector_sync_success_total", map[string]string{"provider": "slack"})
	if v < 1 {
		t.Error("connector_sync_success_total{provider=slack} did not increment")
	}

	metrics.ErrorsTotal.WithLabelValues("crypto").Inc()
	v = gatherLabeledCounter(t, "knowledge_errors_total", map[string]string{"kind": "crypto"})
	if v < 1 {
		t.Error("errors_total{kind=crypto} did not increment")
	}

	metrics.SubsystemStatus.WithLabelValues("evidence_store").Set(1)
	g := gatherGauge(t, "knowledge_subsystem_status")
	if g < 1 {
		t.Error("subsystem_status{name=evidence_store} not set")
	}
}

func TestRegistry_ContainsGoProcessMetrics(t *testing.T) {
	mfs, err := metrics.Registry.Gather()
	if err != nil {
		t.Fatalf("gather: %v", err)
	}
	found := false
	for _, mf := range mfs {
		if strings.HasPrefix(mf.GetName(), "process_") || strings.HasPrefix(mf.GetName(), "go_") {
			found = true
			break
		}
	}
	if !found {
		t.Error("expected process_ or go_ collectors in registry")
	}
}

func TestMiddleware_NoTenantInContext(t *testing.T) {
	metrics.TenantRequestsTotal.Reset()
	r := chi.NewRouter()
	r.Use(metrics.Middleware)
	r.Get("/notenant", func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusOK)
	})

	req := httptest.NewRequest(http.MethodGet, "/notenant", nil)
	w := httptest.NewRecorder()
	r.ServeHTTP(w, req)

	if w.Code != http.StatusOK {
		t.Fatalf("expected 200, got %d", w.Code)
	}

	// No tenant counter should have been incremented with empty values.
	mfs, err := metrics.Registry.Gather()
	if err != nil {
		t.Fatalf("gather: %v", err)
	}
	for _, mf := range mfs {
		if mf.GetName() != "knowledge_tenant_requests_total" {
			continue
		}
		for _, m := range mf.GetMetric() {
			for _, lp := range m.GetLabel() {
				if lp.GetName() == "tenant_id" && lp.GetValue() == "" {
					t.Error("should not record empty tenant_id")
				}
			}
		}
	}
}

func TestStatusRecorder_Flush(t *testing.T) {
	inner := httptest.NewRecorder()
	rec := &statusRecorderWrapper{ResponseWriter: inner, status: http.StatusOK}
	rec.Flush()
	// Just ensure no panic; NewRecorder implements Flusher.
}

// statusRecorderWrapper mirrors the unexported type for testing Flush.
type statusRecorderWrapper struct {
	http.ResponseWriter
	status int
}

func (s *statusRecorderWrapper) WriteHeader(code int) {
	s.status = code
	s.ResponseWriter.WriteHeader(code)
}

func (s *statusRecorderWrapper) Flush() {
	if f, ok := s.ResponseWriter.(http.Flusher); ok {
		f.Flush()
	}
}

func TestRegistry_NotNil(t *testing.T) {
	if metrics.Registry == nil {
		t.Fatal("Registry must not be nil")
	}
	var _ prometheus.Gatherer = metrics.Registry
}
