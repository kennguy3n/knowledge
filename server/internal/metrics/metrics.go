// Package metrics provides Prometheus instrumentation for the
// knowledge API gateway. It registers per-endpoint request counters,
// latency histograms, connector sync gauges, per-tenant request
// counters, and synthesis trigger/success/throttle counters on a
// dedicated registry so the /metrics handler exposes only gateway
// metrics (the Rust substrate_server has its own Prometheus surface at
// /internal/metrics).
package metrics

import (
	"context"
	"net/http"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/go-chi/chi/v5"
	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/collectors"
	"github.com/prometheus/client_golang/prometheus/promhttp"
)

// tenantIDFunc holds the resolver set via SetTenantIDFunc. It is read
// concurrently by the middleware; atomic.Value makes that race-free.
var tenantIDFunc atomic.Value // stores func(context.Context) string

// SetTenantIDFunc registers the function that extracts the
// authenticated tenant ID from a request context. It is safe to call
// from multiple goroutines — only the first call takes effect.
func SetTenantIDFunc(fn func(context.Context) string) {
	tenantIDFunc.CompareAndSwap(nil, fn)
}

// getTenantID invokes the registered resolver, if any.
func getTenantID(ctx context.Context) string {
	if fn, ok := tenantIDFunc.Load().(func(context.Context) string); ok && fn != nil {
		return fn(ctx)
	}
	return ""
}

// Registry is the dedicated Prometheus registry for gateway metrics.
// Exposed so tests can assert on metric values without touching the
// global default registry.
var Registry = prometheus.NewRegistry()

// ──────────────────────── per-endpoint counters ─────────────────────

// RequestsTotal counts every HTTP request handled by the gateway,
// partitioned by method, matched route pattern, and response status.
var RequestsTotal = prometheus.NewCounterVec(prometheus.CounterOpts{
	Name: "knowledge_gateway_requests_total",
	Help: "Total HTTP requests handled by the gateway.",
}, []string{"method", "route", "status"})

// RequestDuration observes request latency in seconds, partitioned by
// method and matched route pattern.
var RequestDuration = prometheus.NewHistogramVec(prometheus.HistogramOpts{
	Name:    "knowledge_gateway_request_duration_seconds",
	Help:    "HTTP request latency in seconds.",
	Buckets: prometheus.DefBuckets,
}, []string{"method", "route"})

// ──────────────────────── per-tenant counters ───────────────────────

// TenantRequestsTotal counts requests per resolved tenant ID.
var TenantRequestsTotal = prometheus.NewCounterVec(prometheus.CounterOpts{
	Name: "knowledge_tenant_requests_total",
	Help: "Total HTTP requests per tenant.",
}, []string{"tenant_id"})

// ──────────────────────── connector counters ────────────────────────

// ConnectorSyncSuccess counts successful sync operations per provider.
var ConnectorSyncSuccess = prometheus.NewCounterVec(prometheus.CounterOpts{
	Name: "knowledge_connector_sync_success_total",
	Help: "Successful connector sync operations.",
}, []string{"provider"})

// ConnectorSyncFailure counts failed sync operations per provider.
var ConnectorSyncFailure = prometheus.NewCounterVec(prometheus.CounterOpts{
	Name: "knowledge_connector_sync_failure_total",
	Help: "Failed connector sync operations.",
}, []string{"provider"})

// ──────────────────────── synthesis counters ─────────────────────────

// SynthesisTriggerTotal counts synthesis trigger requests.
var SynthesisTriggerTotal = prometheus.NewCounter(prometheus.CounterOpts{
	Name: "knowledge_synthesis_trigger_total",
	Help: "Total synthesis trigger requests.",
})

// SynthesisSuccessTotal counts completed syntheses.
var SynthesisSuccessTotal = prometheus.NewCounter(prometheus.CounterOpts{
	Name: "knowledge_synthesis_success_total",
	Help: "Total successful synthesis completions.",
})

// SynthesisThrottleTotal counts throttled synthesis requests.
var SynthesisThrottleTotal = prometheus.NewCounter(prometheus.CounterOpts{
	Name: "knowledge_synthesis_throttle_total",
	Help: "Total throttled synthesis requests.",
})

// ──────────────────────── ingest / query ─────────────────────────────

// IngestTotal counts ingest requests.
var IngestTotal = prometheus.NewCounter(prometheus.CounterOpts{
	Name: "knowledge_ingest_total",
	Help: "Total ingest operations.",
})

// QueryTotal counts query requests.
var QueryTotal = prometheus.NewCounter(prometheus.CounterOpts{
	Name: "knowledge_query_total",
	Help: "Total query operations.",
})

// ErrorsTotal counts errors by kind.
var ErrorsTotal = prometheus.NewCounterVec(prometheus.CounterOpts{
	Name: "knowledge_errors_total",
	Help: "Total errors by kind.",
}, []string{"kind"})

// SubsystemStatus is a gauge per named subsystem (1 = up, 0 = down).
var SubsystemStatus = prometheus.NewGaugeVec(prometheus.GaugeOpts{
	Name: "knowledge_subsystem_status",
	Help: "Subsystem health (1=up, 0=down).",
}, []string{"name"})

func init() {
	Registry.MustRegister(
		collectors.NewProcessCollector(collectors.ProcessCollectorOpts{}),
		collectors.NewGoCollector(),
		RequestsTotal,
		RequestDuration,
		TenantRequestsTotal,
		ConnectorSyncSuccess,
		ConnectorSyncFailure,
		SynthesisTriggerTotal,
		SynthesisSuccessTotal,
		SynthesisThrottleTotal,
		IngestTotal,
		QueryTotal,
		ErrorsTotal,
		SubsystemStatus,
	)
}

// Handler returns an http.Handler that serves the gateway Prometheus
// exposition from [Registry].
func Handler() http.Handler {
	return promhttp.HandlerFor(Registry, promhttp.HandlerOpts{})
}

// sseRoutePrefix identifies SSE synthesis streaming routes whose
// connections stay open for the entire synthesis run (up to 300 s).
// These are excluded from the latency histogram so they do not inflate
// p99 alerts, but they are still counted in RequestsTotal.
const sseRoutePrefix = "/api/v1/synthesis/"

// isSSERoute reports whether a chi route pattern is an SSE-capable
// synthesis status endpoint (e.g. "/api/v1/synthesis/{id}/status").
func isSSERoute(route string) bool {
	return strings.HasPrefix(route, sseRoutePrefix) && strings.HasSuffix(route, "/status")
}

// Middleware records request count and latency keyed by method and the
// matched chi route pattern (low cardinality, never raw paths). Mount
// this in the global middleware chain (before auth is fine).
// SSE synthesis streaming routes are excluded from the latency
// histogram to prevent long-lived connections from inflating p99.
func Middleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		start := time.Now()
		rec := &statusRecorder{ResponseWriter: w, status: http.StatusOK}
		next.ServeHTTP(rec, r)

		route := chi.RouteContext(r.Context()).RoutePattern()
		if route == "" {
			route = "unmatched"
		}
		statusStr := strconv.Itoa(rec.status)
		RequestsTotal.WithLabelValues(r.Method, route, statusStr).Inc()
		if !isSSERoute(route) {
			RequestDuration.WithLabelValues(r.Method, route).Observe(time.Since(start).Seconds())
		}
	})
}

// maxTenantLabels caps the number of distinct tenant_id label values to
// prevent Prometheus cardinality explosion if the tenant population
// grows unexpectedly large.
const maxTenantLabels = 2000

var (
	knownTenants   = make(map[string]struct{})
	knownTenantsMu sync.Mutex
)

// tenantLabelAllowed returns true if the tenant ID should be tracked.
// Once maxTenantLabels distinct IDs have been seen, new IDs are
// recorded under the synthetic "overflow" label.
func tenantLabelAllowed(tid string) string {
	knownTenantsMu.Lock()
	defer knownTenantsMu.Unlock()
	if _, ok := knownTenants[tid]; ok {
		return tid
	}
	if len(knownTenants) >= maxTenantLabels {
		return "overflow"
	}
	knownTenants[tid] = struct{}{}
	return tid
}

// TenantMiddleware increments per-tenant request counters using the
// authenticated tenant ID from the request context. Mount this AFTER
// the auth middleware so the context already contains the resolved
// tenant identity. Cardinality is capped at maxTenantLabels distinct
// tenant IDs; excess tenants are bucketed under "overflow".
func TenantMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		next.ServeHTTP(w, r)
		if tid := getTenantID(r.Context()); tid != "" {
			TenantRequestsTotal.WithLabelValues(tenantLabelAllowed(tid)).Inc()
		}
	})
}

type statusRecorder struct {
	http.ResponseWriter
	status int
}

func (s *statusRecorder) WriteHeader(code int) {
	s.status = code
	s.ResponseWriter.WriteHeader(code)
}

func (s *statusRecorder) Flush() {
	if f, ok := s.ResponseWriter.(http.Flusher); ok {
		f.Flush()
	}
}

func (s *statusRecorder) Unwrap() http.ResponseWriter {
	return s.ResponseWriter
}
