package middleware

import (
	"net/http"
	"sync"
	"time"

	"golang.org/x/time/rate"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
)

// RateLimiter enforces independent per-IP and per-tenant token-bucket
// budgets. Buckets are created lazily and reclaimed by a background
// sweeper after an idle window, bounding memory without paying an
// eviction scan on the hot request path.
type RateLimiter struct {
	ipRPS     rate.Limit
	tenantRPS rate.Limit
	burst     int
	idleTTL   time.Duration
	trust     *ProxyTrust

	mu       sync.Mutex
	ipBucket map[string]*bucket
	tnBucket map[string]*bucket

	stopOnce sync.Once
	done     chan struct{}
}

type bucket struct {
	lim  *rate.Limiter
	seen time.Time
}

// NewRateLimiter builds a limiter with the given per-IP and per-tenant
// refill rates (requests/second) and a shared burst size. trust governs
// how the per-IP key is derived from a request (see [ProxyTrust]); a nil
// trust ignores X-Forwarded-For and keys on the transport peer.
//
// A background goroutine reclaims idle buckets; call [RateLimiter.Stop]
// to shut it down (e.g. on graceful server shutdown).
func NewRateLimiter(ipRPS, tenantRPS float64, burst int, trust *ProxyTrust) *RateLimiter {
	if burst < 1 {
		burst = 1
	}
	rl := &RateLimiter{
		ipRPS:     rate.Limit(ipRPS),
		tenantRPS: rate.Limit(tenantRPS),
		burst:     burst,
		idleTTL:   10 * time.Minute,
		trust:     trust,
		ipBucket:  make(map[string]*bucket),
		tnBucket:  make(map[string]*bucket),
		done:      make(chan struct{}),
	}
	go rl.sweepLoop()
	return rl
}

// Stop terminates the background eviction sweeper. It is safe to call
// more than once.
func (rl *RateLimiter) Stop() {
	rl.stopOnce.Do(func() { close(rl.done) })
}

// sweepLoop periodically reclaims idle buckets from both maps. Running
// eviction on a timer (rather than on every request) keeps allow() O(1)
// regardless of how many distinct keys are tracked.
func (rl *RateLimiter) sweepLoop() {
	t := time.NewTicker(rl.idleTTL / 2)
	defer t.Stop()
	for {
		select {
		case <-rl.done:
			return
		case now := <-t.C:
			rl.mu.Lock()
			rl.evictLocked(rl.ipBucket, now)
			rl.evictLocked(rl.tnBucket, now)
			rl.mu.Unlock()
		}
	}
}

// PerIPMiddleware enforces the per-IP budget. It is mounted *before*
// authentication so that unauthenticated traffic — credential
// stuffing/brute-force, scanners — is throttled per source IP before it
// reaches (and can hammer) the auth layer. Over-budget requests get 429
// with a Retry-After header.
func (rl *RateLimiter) PerIPMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if !rl.allow(rl.ipBucket, rl.trust.ClientIP(r), rl.ipRPS) {
			rl.reject(w)
			return
		}
		next.ServeHTTP(w, r)
	})
}

// PerTenantMiddleware enforces the per-tenant budget. It is mounted
// *after* authentication because it keys on the resolved tenant from the
// request context; requests without a tenant (e.g. raw API-key callers)
// pass through untouched, already having cleared the per-IP gate.
func (rl *RateLimiter) PerTenantMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if tenant := TenantID(r.Context()); tenant != "" {
			if !rl.allow(rl.tnBucket, tenant, rl.tenantRPS) {
				rl.reject(w)
				return
			}
		}
		next.ServeHTTP(w, r)
	})
}

func (rl *RateLimiter) allow(m map[string]*bucket, key string, limit rate.Limit) bool {
	rl.mu.Lock()
	defer rl.mu.Unlock()
	b, ok := m[key]
	if !ok {
		b = &bucket{lim: rate.NewLimiter(limit, rl.burst)}
		m[key] = b
	}
	b.seen = time.Now()
	return b.lim.Allow()
}

// evictLocked removes idle buckets. Callers must hold rl.mu.
func (rl *RateLimiter) evictLocked(m map[string]*bucket, now time.Time) {
	for k, b := range m {
		if now.Sub(b.seen) > rl.idleTTL {
			delete(m, k)
		}
	}
}

func (rl *RateLimiter) reject(w http.ResponseWriter) {
	w.Header().Set("Retry-After", "1")
	httpx.WriteError(w, httpx.NewError(http.StatusTooManyRequests, "RateLimited",
		"rate limit exceeded; retry later"))
}
