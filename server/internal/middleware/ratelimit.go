// Package middleware — rate-limiting middleware.
package middleware

import (
	"net/http"
	"sync"
	"time"

	"go.uber.org/zap"
)

// bucket is a simple token-bucket rate limiter.
type bucket struct {
	tokens     float64
	max        float64
	refillRate float64 // tokens per second
	lastRefill time.Time
	mu         sync.Mutex
}

func newBucket(maxTokens int) *bucket {
	return &bucket{
		tokens:     float64(maxTokens),
		max:        float64(maxTokens),
		refillRate: float64(maxTokens), // refill to max per second
		lastRefill: time.Now(),
	}
}

func (b *bucket) allow() bool {
	b.mu.Lock()
	defer b.mu.Unlock()

	now := time.Now()
	elapsed := now.Sub(b.lastRefill).Seconds()
	b.tokens += elapsed * b.refillRate
	if b.tokens > b.max {
		b.tokens = b.max
	}
	b.lastRefill = now

	if b.tokens >= 1.0 {
		b.tokens--
		return true
	}
	return false
}

// RateLimiter holds per-key token buckets.
type RateLimiter struct {
	buckets sync.Map
	max     int
}

// NewRateLimiter creates a rate limiter with the given per-key max tokens/sec.
func NewRateLimiter(maxPerSec int) *RateLimiter {
	return &RateLimiter{max: maxPerSec}
}

func (rl *RateLimiter) allow(key string) bool {
	v, _ := rl.buckets.LoadOrStore(key, newBucket(rl.max))
	return v.(*bucket).allow()
}

// RateLimit returns middleware that applies per-IP and per-tenant rate limiting.
func RateLimit(perIP, perTenant *RateLimiter, logger *zap.Logger) func(http.Handler) http.Handler {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			ip := r.RemoteAddr

			if !perIP.allow(ip) {
				logger.Warn("rate limit exceeded", zap.String("ip", ip))
				http.Error(w, `{"error":"rate limit exceeded"}`, http.StatusTooManyRequests)
				return
			}

			tenantID := TenantID(r.Context())
			if tenantID != "" && !perTenant.allow(tenantID) {
				logger.Warn("tenant rate limit exceeded", zap.String("tenant_id", tenantID))
				http.Error(w, `{"error":"tenant rate limit exceeded"}`, http.StatusTooManyRequests)
				return
			}

			next.ServeHTTP(w, r)
		})
	}
}
