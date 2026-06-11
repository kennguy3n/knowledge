package tenant

import (
	"context"
	"sync"
	"time"

	"golang.org/x/sync/singleflight"
)

// DefaultQuotaCacheTTL is the freshness window for cached quotas. Config
// changes take effect within this bound; it trades a small staleness
// window for keeping the per-request quota lookup off the database.
const DefaultQuotaCacheTTL = 30 * time.Second

// QuotaCache resolves per-tenant quotas from a [Store] behind a short
// TTL cache so the quota-enforcement middleware never touches the
// database on the hot request path. Concurrent misses for the same
// tenant are collapsed into a single store read (singleflight). Every
// resolved quota is [Quota.Normalized], so callers always receive
// bounded values — including for tenants persisted before quotas
// existed and for unknown tenants (fail-closed).
type QuotaCache struct {
	store Store
	ttl   time.Duration
	sf    singleflight.Group

	mu      sync.RWMutex
	entries map[string]quotaEntry

	stopOnce sync.Once
	done     chan struct{}
}

type quotaEntry struct {
	quota   Quota
	found   bool
	expires time.Time
}

// NewQuotaCache builds a cache over store. A non-positive ttl falls back
// to [DefaultQuotaCacheTTL]. A background goroutine reaps expired
// entries; call [QuotaCache.Stop] to shut it down.
func NewQuotaCache(store Store, ttl time.Duration) *QuotaCache {
	if ttl <= 0 {
		ttl = DefaultQuotaCacheTTL
	}
	c := &QuotaCache{
		store:   store,
		ttl:     ttl,
		entries: make(map[string]quotaEntry),
		done:    make(chan struct{}),
	}
	go c.reapLoop()
	return c
}

// Stop terminates the background reaper. Safe to call repeatedly.
func (c *QuotaCache) Stop() {
	c.stopOnce.Do(func() { close(c.done) })
}

// TenantQuota returns the effective (normalized) quota for tenantID and
// whether the tenant is known. An unknown tenant or a transient store
// error yields the safe default quota with found=false, so enforcement
// stays bounded rather than failing open.
func (c *QuotaCache) TenantQuota(ctx context.Context, tenantID string) (Quota, bool) {
	now := time.Now()
	c.mu.RLock()
	e, ok := c.entries[tenantID]
	c.mu.RUnlock()
	if ok && now.Before(e.expires) {
		return e.quota, e.found
	}

	v, _, _ := c.sf.Do(tenantID, func() (any, error) {
		ent := quotaEntry{expires: time.Now().Add(c.ttl)}
		t, err := c.store.GetTenant(ctx, tenantID)
		if err == nil {
			ent.quota = t.Config.Quota.Normalized()
			ent.found = true
		} else {
			// Unknown tenant or store error: bound it with defaults.
			ent.quota = DefaultQuota()
			ent.found = false
		}
		c.mu.Lock()
		c.entries[tenantID] = ent
		c.mu.Unlock()
		return ent, nil
	})
	ent := v.(quotaEntry)
	return ent.quota, ent.found
}

// Invalidate drops any cached entry for tenantID so the next lookup
// re-reads the store. Useful immediately after a config change.
func (c *QuotaCache) Invalidate(tenantID string) {
	c.mu.Lock()
	delete(c.entries, tenantID)
	c.mu.Unlock()
}

func (c *QuotaCache) reapLoop() {
	t := time.NewTicker(c.ttl)
	defer t.Stop()
	for {
		select {
		case <-c.done:
			return
		case now := <-t.C:
			c.mu.Lock()
			for k, e := range c.entries {
				if now.After(e.expires) {
					delete(c.entries, k)
				}
			}
			c.mu.Unlock()
		}
	}
}
