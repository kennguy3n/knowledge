package middleware

import (
	"context"
	"net/http"
	"os"
	"strconv"
	"sync"
	"sync/atomic"
	"time"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
)

// Environment variables that tune synthesis fair-share admission. They
// are read directly here (rather than via the config package) so the
// admission controller is self-contained and can be unit-tested in
// isolation.
const (
	// EnvSynthTenantConcurrency caps the number of syntheses a single
	// tenant may run concurrently on the shared llama-server pool.
	EnvSynthTenantConcurrency = "KNOWLEDGE_SYNTHESIS_TENANT_CONCURRENCY"
	// EnvSynthTenantQueue bounds how many additional syntheses a single
	// tenant may have waiting once it is at its concurrency cap. Excess
	// requests are shed with 429.
	EnvSynthTenantQueue = "KNOWLEDGE_SYNTHESIS_TENANT_QUEUE"
	// EnvSynthGlobalConcurrency caps total concurrent syntheses across
	// all tenants, sized to the shared CPU-bound llama-server replica
	// pool so the box is never oversubscribed.
	EnvSynthGlobalConcurrency = "KNOWLEDGE_SYNTHESIS_GLOBAL_CONCURRENCY"
	// EnvSynthQueueWait bounds how long a queued synthesis waits for a
	// free slot before it is shed with 429 + Retry-After.
	EnvSynthQueueWait = "KNOWLEDGE_SYNTHESIS_QUEUE_WAIT"
)

// Fair-share defaults sized for ~5,000 SME tenants sharing a small
// llama-server replica pool. With a per-tenant cap of 2 and a global
// cap of 8, any single tenant can occupy at most 2 of the 8 shared
// synthesis slots, so at least four distinct tenants can always make
// progress concurrently — one tenant can never starve the other 4,999.
const (
	defaultSynthTenantConcurrency = 2
	defaultSynthTenantQueue       = 4
	defaultSynthGlobalConcurrency = 8
	defaultSynthQueueWait         = 5 * time.Second

	// synthGateIdleTTL is how long an idle per-tenant gate is retained
	// before the background sweeper reclaims it, bounding memory under a
	// large, churning tenant population without an eviction scan on the
	// hot admission path.
	synthGateIdleTTL = 10 * time.Minute

	// globalWaiterMultiple bounds the number of goroutines that may
	// block on the global slot relative to its capacity. Each global
	// waiter already holds a per-tenant slot, so the per-tenant caps are
	// the primary admission control; this backstop fast-fails (429)
	// under extreme cluster-wide overload instead of parking thousands
	// of goroutines.
	globalWaiterMultiple = 64

	// serviceTenantKey buckets requests that carry no resolved tenant
	// (e.g. the static service/admin principal) so they are still
	// fair-shared against each other and cannot monopolise the pool.
	serviceTenantKey = "_service"
)

// FairShareConfig tunes the synthesis admission controller.
type FairShareConfig struct {
	// TenantConcurrency is the per-tenant concurrent-synthesis cap.
	TenantConcurrency int
	// TenantQueue is the bounded per-tenant FIFO wait depth once at cap.
	TenantQueue int
	// GlobalConcurrency caps concurrent syntheses across all tenants.
	GlobalConcurrency int
	// QueueWait bounds how long a queued request waits for a slot.
	QueueWait time.Duration
}

// withDefaults returns a copy of cfg with any unset/invalid field
// replaced by its production default. Fail-closed: a non-positive
// concurrency would otherwise disable admission control entirely.
func (c FairShareConfig) withDefaults() FairShareConfig {
	if c.TenantConcurrency < 1 {
		c.TenantConcurrency = defaultSynthTenantConcurrency
	}
	if c.TenantQueue < 0 {
		c.TenantQueue = defaultSynthTenantQueue
	}
	if c.GlobalConcurrency < 1 {
		c.GlobalConcurrency = defaultSynthGlobalConcurrency
	}
	if c.QueueWait <= 0 {
		c.QueueWait = defaultSynthQueueWait
	}
	return c
}

// semaphore is a counting semaphore with a bounded FIFO waiter queue.
// Tokens are modelled as buffered-channel sends/receives; the Go
// runtime services blocked receivers in FIFO order, giving fair,
// first-come-first-served admission once a tenant is at its cap.
type semaphore struct {
	tokens  chan struct{}
	waiting int32 // goroutines currently blocked in acquire (atomic)
	maxWait int32
}

func newSemaphore(capacity, maxWaiters int) *semaphore {
	if capacity < 1 {
		capacity = 1
	}
	if maxWaiters < 0 {
		maxWaiters = 0
	}
	s := &semaphore{
		tokens:  make(chan struct{}, capacity),
		maxWait: int32(maxWaiters),
	}
	for i := 0; i < capacity; i++ {
		s.tokens <- struct{}{}
	}
	return s
}

// tryAcquire takes a token without blocking.
func (s *semaphore) tryAcquire() bool {
	select {
	case <-s.tokens:
		return true
	default:
		return false
	}
}

// acquire takes a token, blocking in a bounded FIFO queue until one is
// free or ctx is done (its deadline is the queue-wait budget). It
// returns false (fail-closed) when the waiter slots are exhausted or
// ctx is cancelled/expired.
func (s *semaphore) acquire(ctx context.Context) bool {
	if s.tryAcquire() {
		return true
	}
	// Reserve a bounded waiter slot before parking. AddInt32 returns the
	// post-increment value, so a value above the bound means the queue
	// is already full and we must shed.
	if atomic.AddInt32(&s.waiting, 1) > s.maxWait {
		atomic.AddInt32(&s.waiting, -1)
		return false
	}
	defer atomic.AddInt32(&s.waiting, -1)

	select {
	case <-s.tokens:
		return true
	case <-ctx.Done():
		return false
	}
}

// release returns a token. The default arm guards against a release
// without a matching acquire (which would otherwise panic on a full
// channel); it is unreachable in correct use.
func (s *semaphore) release() {
	select {
	case s.tokens <- struct{}{}:
	default:
	}
}

// idle reports whether every token is held by the semaphore (nothing
// in flight) and no goroutine is waiting — the precondition for safely
// reclaiming a per-tenant gate.
func (s *semaphore) idle() bool {
	return len(s.tokens) == cap(s.tokens) && atomic.LoadInt32(&s.waiting) == 0
}

type tenantGate struct {
	sem      *semaphore
	lastSeen int64 // unix-nanos, atomic
}

// SynthesisFairShare admits synthesis requests under a two-level cap:
// a per-tenant concurrency cap with a bounded FIFO queue (fairness — no
// single tenant can monopolise the pool) layered over a global
// concurrency cap (protection — the shared CPU-bound llama-server is
// never oversubscribed). All operations are safe for concurrent use.
type SynthesisFairShare struct {
	cfg    FairShareConfig
	global *semaphore

	mu    sync.Mutex
	gates map[string]*tenantGate

	stopOnce sync.Once
	done     chan struct{}
}

// NewSynthesisFairShare builds an admission controller from cfg,
// applying production defaults for any unset field. A background
// goroutine reclaims idle per-tenant gates; call [SynthesisFairShare.Stop]
// to shut it down.
func NewSynthesisFairShare(cfg FairShareConfig) *SynthesisFairShare {
	cfg = cfg.withDefaults()
	f := &SynthesisFairShare{
		cfg:    cfg,
		global: newSemaphore(cfg.GlobalConcurrency, cfg.GlobalConcurrency*globalWaiterMultiple),
		gates:  make(map[string]*tenantGate),
		done:   make(chan struct{}),
	}
	go f.sweepLoop()
	return f
}

// NewSynthesisFairShareFromEnv builds an admission controller from the
// KNOWLEDGE_SYNTHESIS_* environment variables, falling back to the
// production defaults when they are unset or invalid.
func NewSynthesisFairShareFromEnv() *SynthesisFairShare {
	return NewSynthesisFairShare(FairShareConfig{
		TenantConcurrency: envInt(EnvSynthTenantConcurrency, defaultSynthTenantConcurrency),
		TenantQueue:       envInt(EnvSynthTenantQueue, defaultSynthTenantQueue),
		GlobalConcurrency: envInt(EnvSynthGlobalConcurrency, defaultSynthGlobalConcurrency),
		QueueWait:         envDuration(EnvSynthQueueWait, defaultSynthQueueWait),
	})
}

// Stop terminates the background gate sweeper. Safe to call repeatedly.
func (f *SynthesisFairShare) Stop() {
	f.stopOnce.Do(func() { close(f.done) })
}

// Acquire reserves a synthesis slot for tenant, blocking up to the
// configured QueueWait. On success it returns a release function (which
// is idempotent and must be called exactly once, e.g. via defer) and a
// nil error. When the tenant is at its cap with a full queue, the
// global pool is saturated, or the wait times out, it returns a 429
// [*httpx.Error] together with a Retry-After hint in seconds.
//
// Slots are taken tenant-first, then global, so a goroutine that blocks
// on the global cap already holds a per-tenant slot; this bounds the
// global slots any single tenant can contend for to its per-tenant cap.
func (f *SynthesisFairShare) Acquire(ctx context.Context, tenant string) (release func(), retryAfter int, err error) {
	if tenant == "" {
		tenant = serviceTenantKey
	}
	// A single shared deadline bounds the *total* queue wait to QueueWait
	// across both the tenant and global gates (so a request can't wait
	// QueueWait twice), and keeps the Retry-After hint accurate.
	wctx, cancel := context.WithTimeout(ctx, f.cfg.QueueWait)
	defer cancel()
	g := f.gate(tenant)
	if !g.sem.acquire(wctx) {
		return nil, f.retryAfterSeconds(), f.throttle("tenant synthesis concurrency limit reached; retry later")
	}
	if !f.global.acquire(wctx) {
		g.sem.release()
		return nil, f.retryAfterSeconds(), f.throttle("synthesis pool saturated; retry later")
	}
	var once sync.Once
	return func() {
		once.Do(func() {
			f.global.release()
			g.sem.release()
		})
	}, 0, nil
}

func (f *SynthesisFairShare) throttle(msg string) error {
	return httpx.NewError(http.StatusTooManyRequests, "SynthesisThrottled", msg)
}

// retryAfterSeconds rounds the queue-wait window up to whole seconds
// (minimum 1) for the Retry-After header.
func (f *SynthesisFairShare) retryAfterSeconds() int {
	secs := int((f.cfg.QueueWait + time.Second - 1) / time.Second)
	if secs < 1 {
		secs = 1
	}
	return secs
}

// gate returns the (lazily created) per-tenant gate, refreshing its
// last-seen stamp so the sweeper does not reclaim it while in use.
func (f *SynthesisFairShare) gate(tenant string) *tenantGate {
	now := time.Now().UnixNano()
	f.mu.Lock()
	g, ok := f.gates[tenant]
	if !ok {
		// Stamp lastSeen under the lock at creation so reap() (which also
		// holds the lock) can never observe a zero timestamp and evict a
		// brand-new gate before its first acquire.
		g = &tenantGate{
			sem:      newSemaphore(f.cfg.TenantConcurrency, f.cfg.TenantQueue),
			lastSeen: now,
		}
		f.gates[tenant] = g
	}
	f.mu.Unlock()
	atomic.StoreInt64(&g.lastSeen, now)
	return g
}

func (f *SynthesisFairShare) sweepLoop() {
	t := time.NewTicker(synthGateIdleTTL / 2)
	defer t.Stop()
	for {
		select {
		case <-f.done:
			return
		case <-t.C:
			f.reap()
		}
	}
}

// reap reclaims per-tenant gates that have been idle past the TTL. A
// gate is only removed when its semaphore is fully idle, so a request
// holding (or queued for) a slot is never dropped; gate() refreshes
// lastSeen immediately before acquiring, so an in-use gate cannot be
// older than the TTL.
func (f *SynthesisFairShare) reap() {
	cutoff := time.Now().Add(-synthGateIdleTTL).UnixNano()
	f.mu.Lock()
	defer f.mu.Unlock()
	for k, g := range f.gates {
		if atomic.LoadInt64(&g.lastSeen) > cutoff {
			continue
		}
		if g.sem.idle() {
			delete(f.gates, k)
		}
	}
}

// envInt parses an integer env var, returning def when unset or
// unparseable. Range validation (e.g. rejecting non-positive values) is
// left to [FairShareConfig.withDefaults].
func envInt(name string, def int) int {
	v := os.Getenv(name)
	if v == "" {
		return def
	}
	n, err := strconv.Atoi(v)
	if err != nil {
		return def
	}
	return n
}

// envDuration reads a Go duration env var, returning def when unset or
// unparseable.
func envDuration(name string, def time.Duration) time.Duration {
	v := os.Getenv(name)
	if v == "" {
		return def
	}
	d, err := time.ParseDuration(v)
	if err != nil {
		return def
	}
	return d
}
