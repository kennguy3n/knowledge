package middleware

import (
	"net/http"
	"sync"
	"time"

	"golang.org/x/time/rate"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
)

// RateLimiter enforces independent per-IP and per-tenant token-bucket
// budgets. Buckets are created lazily and evicted after an idle
// window to bound memory.
type RateLimiter struct {
	ipRPS     rate.Limit
	tenantRPS rate.Limit
	burst     int
	idleTTL   time.Duration

	mu       sync.Mutex
	ipBucket map[string]*bucket
	tnBucket map[string]*bucket
}

type bucket struct {
	lim  *rate.Limiter
	seen time.Time
}

// NewRateLimiter builds a limiter with the given per-IP and per-tenant
// refill rates (requests/second) and a shared burst size.
func NewRateLimiter(ipRPS, tenantRPS float64, burst int) *RateLimiter {
	if burst < 1 {
		burst = 1
	}
	return &RateLimiter{
		ipRPS:     rate.Limit(ipRPS),
		tenantRPS: rate.Limit(tenantRPS),
		burst:     burst,
		idleTTL:   10 * time.Minute,
		ipBucket:  make(map[string]*bucket),
		tnBucket:  make(map[string]*bucket),
	}
}

// PerIPMiddleware enforces the per-IP budget. It is mounted *before*
// authentication so that unauthenticated traffic — credential
// stuffing/brute-force, scanners — is throttled per source IP before it
// reaches (and can hammer) the auth layer. Over-budget requests get 429
// with a Retry-After header.
func (rl *RateLimiter) PerIPMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if !rl.allow(rl.ipBucket, clientIP(r), rl.ipRPS) {
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
	now := time.Now()
	rl.evictLocked(m, now)
	b, ok := m[key]
	if !ok {
		b = &bucket{lim: rate.NewLimiter(limit, rl.burst)}
		m[key] = b
	}
	b.seen = now
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
