package middleware

import (
	"context"
	"net/http"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
	"github.com/kennguy3n/knowledge/server/internal/metrics"
	"github.com/kennguy3n/knowledge/server/internal/tenant"
)

// QuotaSource resolves the effective (already-normalized, bounded)
// quota for a tenant. [tenant.QuotaCache] implements it. The bool
// reports whether the tenant is known; enforcement applies the returned
// quota regardless, so an unknown tenant is bounded rather than
// unbounded (fail-closed).
type QuotaSource interface {
	TenantQuota(ctx context.Context, tenantID string) (tenant.Quota, bool)
}

// StorageUsageFunc reports a tenant's current stored bytes. It backs the
// advisory storage soft-cap check. When nil, the storage check is
// skipped. It must be cheap (cached) — it runs on write requests.
type StorageUsageFunc func(ctx context.Context, tenantID string) (int64, bool)

const (
	defaultRequestWindow   = time.Minute
	defaultSynthesisWindow = 24 * time.Hour
	// quotaGateIdleTTL bounds memory: per-tenant counters idle past this
	// are reclaimed by the background sweeper.
	quotaGateIdleTTL = 30 * time.Minute
)

// QuotaConfig tunes the quota enforcer. Zero-valued fields fall back to
// production defaults.
type QuotaConfig struct {
	// RequestWindow is the fixed window for the requests-per-min quota.
	RequestWindow time.Duration
	// SynthesisWindow is the fixed window for the syntheses-per-day quota.
	SynthesisWindow time.Duration
	// SynthesisPathSuffix identifies the synthesis-trigger route whose
	// POSTs count against the daily synthesis quota.
	SynthesisPathSuffix string
	// IngestPathSuffix identifies the ingest route whose writes are
	// checked against the storage soft cap.
	IngestPathSuffix string
	// Usage backs the storage soft-cap check; nil disables it.
	Usage StorageUsageFunc
}

func (c QuotaConfig) withDefaults() QuotaConfig {
	if c.RequestWindow <= 0 {
		c.RequestWindow = defaultRequestWindow
	}
	if c.SynthesisWindow <= 0 {
		c.SynthesisWindow = defaultSynthesisWindow
	}
	if c.SynthesisPathSuffix == "" {
		c.SynthesisPathSuffix = "/synthesis/trigger"
	}
	if c.IngestPathSuffix == "" {
		c.IngestPathSuffix = "/ingest"
	}
	return c
}

// QuotaEnforcer enforces per-tenant volume quotas resolved from a
// [QuotaSource]: a requests-per-minute ceiling on every request and a
// syntheses-per-day ceiling on synthesis triggers, both fixed-window.
// Exceeding a hard quota returns 429 with a structured error and
// Retry-After. The storage soft cap is advisory (a response header +
// metric, never a block). All state is safe for concurrent use.
//
// Wiring note: add [QuotaEnforcer.Middleware] to the /api/v1 chain in
// gateway.go immediately AFTER auth + metrics.TenantMiddleware (so the
// tenant is resolved in context) and before the route handlers. This
// type is kept registration-free so it can be slotted in without this
// package owning gateway.go.
type QuotaEnforcer struct {
	src QuotaSource
	cfg QuotaConfig

	mu       sync.Mutex
	counters map[string]*quotaCounter

	stopOnce sync.Once
	done     chan struct{}
}

// NewQuotaEnforcer builds an enforcer over src with the given config
// (defaults applied for unset fields). A background goroutine reclaims
// idle per-tenant counters; call [QuotaEnforcer.Stop] to shut it down.
func NewQuotaEnforcer(src QuotaSource, cfg QuotaConfig) *QuotaEnforcer {
	e := &QuotaEnforcer{
		src:      src,
		cfg:      cfg.withDefaults(),
		counters: make(map[string]*quotaCounter),
		done:     make(chan struct{}),
	}
	go e.sweepLoop()
	return e
}

// Stop terminates the background sweeper. Safe to call repeatedly.
func (e *QuotaEnforcer) Stop() {
	e.stopOnce.Do(func() { close(e.done) })
}

type quotaCounter struct {
	mu       sync.Mutex
	minStart time.Time
	minCount int
	dayStart time.Time
	dayCount int
	lastSeen int64 // unix-nanos, atomic
}

// quotaDecision is the outcome of an admission check.
type quotaDecision struct {
	ok         bool
	dimension  string
	retryAfter int // seconds
}

// Middleware returns an http middleware that enforces the tenant's
// quotas. Requests without a resolved tenant (e.g. the static service
// principal) are passed through unmetered.
func (e *QuotaEnforcer) Middleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		ctx := r.Context()
		tid := TenantID(ctx)
		if tid == "" {
			next.ServeHTTP(w, r)
			return
		}
		q, _ := e.src.TenantQuota(ctx, tid)
		isSynthesis := r.Method == http.MethodPost && strings.HasSuffix(r.URL.Path, e.cfg.SynthesisPathSuffix)

		if d := e.admit(tid, q, isSynthesis, time.Now()); !d.ok {
			metrics.ErrorsTotal.WithLabelValues("quota").Inc()
			w.Header().Set("Retry-After", strconv.Itoa(d.retryAfter))
			httpx.WriteError(w, httpx.NewError(http.StatusTooManyRequests, "QuotaExceeded",
				"tenant quota exceeded: "+d.dimension))
			return
		}

		// Advisory storage soft cap: never blocks, only signals.
		e.checkStorage(ctx, w, r, tid, q)
		next.ServeHTTP(w, r)
	})
}

// admit evaluates all hard quotas for one request and, only if every
// limit is satisfied, atomically commits the consumption. Evaluating
// before committing means a request rejected on the synthesis quota
// does not also burn the request-per-minute budget.
func (e *QuotaEnforcer) admit(tid string, q tenant.Quota, isSynthesis bool, now time.Time) quotaDecision {
	c := e.counter(tid)
	c.mu.Lock()
	defer c.mu.Unlock()

	if c.minStart.IsZero() || now.Sub(c.minStart) >= e.cfg.RequestWindow {
		c.minStart = now
		c.minCount = 0
	}
	if isSynthesis && (c.dayStart.IsZero() || now.Sub(c.dayStart) >= e.cfg.SynthesisWindow) {
		c.dayStart = now
		c.dayCount = 0
	}

	if c.minCount >= q.RequestsPerMin {
		return quotaDecision{dimension: "requests_per_min",
			retryAfter: retryAfterUntil(now, c.minStart.Add(e.cfg.RequestWindow))}
	}
	if isSynthesis && c.dayCount >= q.SynthesesPerDay {
		return quotaDecision{dimension: "syntheses_per_day",
			retryAfter: retryAfterUntil(now, c.dayStart.Add(e.cfg.SynthesisWindow))}
	}

	c.minCount++
	if isSynthesis {
		c.dayCount++
	}
	return quotaDecision{ok: true}
}

// checkStorage applies the advisory storage soft cap on ingest writes.
// It is non-blocking: over-cap tenants get an X-Quota-Storage header and
// a metric so operators/clients can react, but the request proceeds.
func (e *QuotaEnforcer) checkStorage(ctx context.Context, w http.ResponseWriter, r *http.Request, tid string, q tenant.Quota) {
	if e.cfg.Usage == nil {
		return
	}
	if !(r.Method == http.MethodPost && strings.HasSuffix(r.URL.Path, e.cfg.IngestPathSuffix)) {
		return
	}
	used, ok := e.cfg.Usage(ctx, tid)
	if ok && used >= q.StorageSoftCapBytes {
		w.Header().Set("X-Quota-Storage", "soft-cap-exceeded")
		metrics.ErrorsTotal.WithLabelValues("storage_soft_cap").Inc()
	}
}

func (e *QuotaEnforcer) counter(tid string) *quotaCounter {
	e.mu.Lock()
	c, ok := e.counters[tid]
	if !ok {
		c = &quotaCounter{}
		e.counters[tid] = c
	}
	e.mu.Unlock()
	atomic.StoreInt64(&c.lastSeen, time.Now().UnixNano())
	return c
}

func (e *QuotaEnforcer) sweepLoop() {
	t := time.NewTicker(quotaGateIdleTTL / 2)
	defer t.Stop()
	for {
		select {
		case <-e.done:
			return
		case <-t.C:
			cutoff := time.Now().Add(-quotaGateIdleTTL).UnixNano()
			e.mu.Lock()
			for k, c := range e.counters {
				if atomic.LoadInt64(&c.lastSeen) < cutoff {
					delete(e.counters, k)
				}
			}
			e.mu.Unlock()
		}
	}
}

// retryAfterUntil returns whole seconds until t (minimum 1), for the
// Retry-After header.
func retryAfterUntil(now, t time.Time) int {
	d := t.Sub(now)
	secs := int((d + time.Second - 1) / time.Second)
	if secs < 1 {
		secs = 1
	}
	return secs
}
