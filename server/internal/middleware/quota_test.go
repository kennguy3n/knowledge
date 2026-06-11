package middleware

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strconv"
	"sync"
	"testing"
	"time"

	"github.com/kennguy3n/knowledge/server/internal/tenant"
)

// fakeQuotaSource is an in-memory [QuotaSource] for tests. Unknown
// tenants resolve to the normalized default quota with found=false,
// mirroring the production [tenant.QuotaCache] fail-closed behavior.
type fakeQuotaSource struct {
	mu sync.Mutex
	m  map[string]tenant.Quota
}

func newFakeQuotaSource() *fakeQuotaSource {
	return &fakeQuotaSource{m: make(map[string]tenant.Quota)}
}

func (f *fakeQuotaSource) set(id string, q tenant.Quota) {
	f.mu.Lock()
	f.m[id] = q
	f.mu.Unlock()
}

func (f *fakeQuotaSource) TenantQuota(_ context.Context, id string) (tenant.Quota, bool) {
	f.mu.Lock()
	q, ok := f.m[id]
	f.mu.Unlock()
	if !ok {
		return tenant.DefaultQuota(), false
	}
	return q.Normalized(), true
}

func ctxWithTenant(id string) context.Context {
	return context.WithValue(context.Background(), keyTenant, id)
}

func okHandler() http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusOK)
	})
}

// doReq drives a single request through the enforcer middleware for the
// given tenant + path and returns the recorder.
func doReq(h http.Handler, tid, method, path string) *httptest.ResponseRecorder {
	req := httptest.NewRequest(method, path, nil)
	if tid != "" {
		req = req.WithContext(ctxWithTenant(tid))
	}
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	return rec
}

func TestQuotaRequestsPerMinEnforced(t *testing.T) {
	t.Parallel()
	src := newFakeQuotaSource()
	src.set("noisy", tenant.Quota{RequestsPerMin: 3, SynthesesPerDay: 100, StorageSoftCapBytes: 1 << 40})
	e := NewQuotaEnforcer(src, QuotaConfig{})
	defer e.Stop()
	h := e.Middleware(okHandler())

	for i := 0; i < 3; i++ {
		if rec := doReq(h, "noisy", http.MethodGet, "/api/v1/query"); rec.Code != http.StatusOK {
			t.Fatalf("request %d: want 200, got %d", i+1, rec.Code)
		}
	}
	rec := doReq(h, "noisy", http.MethodGet, "/api/v1/query")
	if rec.Code != http.StatusTooManyRequests {
		t.Fatalf("over-quota request: want 429, got %d", rec.Code)
	}
	if ra := rec.Header().Get("Retry-After"); ra == "" {
		t.Fatal("missing Retry-After header on 429")
	} else if n, err := strconv.Atoi(ra); err != nil || n < 1 {
		t.Fatalf("bad Retry-After %q", ra)
	}
	var body struct {
		Kind    string `json:"kind"`
		Message string `json:"message"`
	}
	if err := json.NewDecoder(rec.Body).Decode(&body); err != nil {
		t.Fatalf("decode error body: %v", err)
	}
	if body.Kind != "QuotaExceeded" {
		t.Fatalf("want kind QuotaExceeded, got %q", body.Kind)
	}
}

// TestQuotaPerTenantOverride proves a low per-tenant override throttles
// one tenant while another (generous) tenant proceeds unaffected.
func TestQuotaPerTenantOverride(t *testing.T) {
	t.Parallel()
	src := newFakeQuotaSource()
	src.set("small", tenant.Quota{RequestsPerMin: 1, SynthesesPerDay: 100, StorageSoftCapBytes: 1 << 40})
	src.set("big", tenant.Quota{RequestsPerMin: 1000, SynthesesPerDay: 100, StorageSoftCapBytes: 1 << 40})
	e := NewQuotaEnforcer(src, QuotaConfig{})
	defer e.Stop()
	h := e.Middleware(okHandler())

	if rec := doReq(h, "small", http.MethodGet, "/api/v1/query"); rec.Code != http.StatusOK {
		t.Fatalf("small first req: want 200, got %d", rec.Code)
	}
	if rec := doReq(h, "small", http.MethodGet, "/api/v1/query"); rec.Code != http.StatusTooManyRequests {
		t.Fatalf("small second req: want 429, got %d", rec.Code)
	}
	// The generous tenant is unaffected by the small tenant's throttling.
	for i := 0; i < 5; i++ {
		if rec := doReq(h, "big", http.MethodGet, "/api/v1/query"); rec.Code != http.StatusOK {
			t.Fatalf("big req %d: want 200, got %d", i+1, rec.Code)
		}
	}
}

func TestQuotaSynthesesPerDayEnforced(t *testing.T) {
	t.Parallel()
	src := newFakeQuotaSource()
	src.set("t", tenant.Quota{RequestsPerMin: 1000, SynthesesPerDay: 2, StorageSoftCapBytes: 1 << 40})
	e := NewQuotaEnforcer(src, QuotaConfig{})
	defer e.Stop()
	h := e.Middleware(okHandler())

	for i := 0; i < 2; i++ {
		if rec := doReq(h, "t", http.MethodPost, "/api/v1/synthesis/trigger"); rec.Code != http.StatusOK {
			t.Fatalf("synthesis %d: want 200, got %d", i+1, rec.Code)
		}
	}
	rec := doReq(h, "t", http.MethodPost, "/api/v1/synthesis/trigger")
	if rec.Code != http.StatusTooManyRequests {
		t.Fatalf("over daily synthesis quota: want 429, got %d", rec.Code)
	}
}

// TestQuotaRejectedSynthesisDoesNotBurnRequestBudget verifies admit()
// commits consumption only when every limit passes: a synthesis shed on
// the daily quota must not also consume the per-minute request budget.
func TestQuotaRejectedSynthesisDoesNotBurnRequestBudget(t *testing.T) {
	t.Parallel()
	src := newFakeQuotaSource()
	e := NewQuotaEnforcer(src, QuotaConfig{})
	defer e.Stop()
	q := tenant.Quota{RequestsPerMin: 5, SynthesesPerDay: 1, StorageSoftCapBytes: 1 << 40}
	now := time.Now()

	if d := e.admit("t", q, true, now); !d.ok {
		t.Fatal("first synthesis should be admitted")
	}
	// Second synthesis is shed on the daily quota...
	if d := e.admit("t", q, true, now); d.ok || d.dimension != "syntheses_per_day" {
		t.Fatalf("second synthesis should be shed on syntheses_per_day, got %+v", d)
	}
	// ...and must not have consumed the request budget: 4 more non-
	// synthesis requests (total 5 with the first) should still pass.
	for i := 0; i < 4; i++ {
		if d := e.admit("t", q, false, now); !d.ok {
			t.Fatalf("request %d after shed synthesis should pass, got %+v", i+1, d)
		}
	}
	if d := e.admit("t", q, false, now); d.ok || d.dimension != "requests_per_min" {
		t.Fatalf("6th request should be shed on requests_per_min, got %+v", d)
	}
}

// TestQuotaWindowResets confirms the fixed request window rolls over.
func TestQuotaWindowResets(t *testing.T) {
	t.Parallel()
	src := newFakeQuotaSource()
	e := NewQuotaEnforcer(src, QuotaConfig{RequestWindow: time.Minute})
	defer e.Stop()
	q := tenant.Quota{RequestsPerMin: 1, SynthesesPerDay: 10, StorageSoftCapBytes: 1 << 40}
	t0 := time.Now()

	if d := e.admit("t", q, false, t0); !d.ok {
		t.Fatal("first request should pass")
	}
	if d := e.admit("t", q, false, t0); d.ok {
		t.Fatal("second request in same window should be shed")
	}
	// Advance past the window: the counter resets.
	if d := e.admit("t", q, false, t0.Add(time.Minute+time.Second)); !d.ok {
		t.Fatal("request in next window should pass")
	}
}

// TestQuotaNoTenantPassthrough verifies unauthenticated/service-principal
// requests (no tenant in context) are not metered.
func TestQuotaNoTenantPassthrough(t *testing.T) {
	t.Parallel()
	src := newFakeQuotaSource()
	e := NewQuotaEnforcer(src, QuotaConfig{})
	defer e.Stop()
	h := e.Middleware(okHandler())
	for i := 0; i < 100; i++ {
		if rec := doReq(h, "", http.MethodGet, "/api/v1/query"); rec.Code != http.StatusOK {
			t.Fatalf("service request %d should pass unmetered, got %d", i+1, rec.Code)
		}
	}
}

// TestQuotaStorageSoftCapAdvisory verifies the storage soft cap is
// advisory: an over-cap tenant gets a signal header but the write still
// succeeds (never blocks).
func TestQuotaStorageSoftCapAdvisory(t *testing.T) {
	t.Parallel()
	src := newFakeQuotaSource()
	src.set("t", tenant.Quota{RequestsPerMin: 1000, SynthesesPerDay: 100, StorageSoftCapBytes: 1024})
	e := NewQuotaEnforcer(src, QuotaConfig{
		Usage: func(_ context.Context, _ string) (int64, bool) { return 4096, true },
	})
	defer e.Stop()
	h := e.Middleware(okHandler())

	rec := doReq(h, "t", http.MethodPost, "/api/v1/ingest")
	if rec.Code != http.StatusOK {
		t.Fatalf("over-soft-cap ingest must still succeed, got %d", rec.Code)
	}
	if got := rec.Header().Get("X-Quota-Storage"); got != "soft-cap-exceeded" {
		t.Fatalf("want X-Quota-Storage advisory header, got %q", got)
	}
}

// TestQuotaUnknownTenantBounded confirms an unknown tenant is still
// bounded by the default quota (fail-closed), not unbounded.
func TestQuotaUnknownTenantBounded(t *testing.T) {
	t.Parallel()
	src := newFakeQuotaSource() // empty: every tenant is "unknown"
	e := NewQuotaEnforcer(src, QuotaConfig{})
	defer e.Stop()
	q, found := src.TenantQuota(context.Background(), "ghost")
	if found {
		t.Fatal("ghost tenant should be unknown")
	}
	if q.RequestsPerMin != tenant.DefaultQuota().RequestsPerMin {
		t.Fatalf("unknown tenant should inherit default quota, got %d", q.RequestsPerMin)
	}
}
