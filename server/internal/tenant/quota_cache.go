package tenant

import (
	"context"
	"errors"
	"sync"
	"time"

	"golang.org/x/sync/singleflight"
)

// DefaultQuotaCacheTTL is the freshness window for cached quotas. Config
// changes take effect within this bound; it trades a small staleness
// window for keeping the per-request quota lookup off the database.
const DefaultQuotaCacheTTL = 30 * time.Second

// defaultStoreReadTimeout bounds a single cache-miss store read. Because
// the read is detached from the caller's context (see TenantQuota) and
// shared across singleflight waiters, an unbounded read against a hung
// store would stall every concurrent quota lookup for that tenant; this
// ceiling makes such a read fail-closed (default quota) instead.
const defaultStoreReadTimeout = 5 * time.Second

// defaultStoreErrorTTL is the (short) cache lifetime for the result of a
// *transient* store error, as opposed to a definitive not-found. A
// transient failure must not pin a fallback quota for a full TTL — that
// would raise an operator-set lower quota (e.g. an abuse throttle) to
// the default for up to DefaultQuotaCacheTTL. The short lifetime bounds
// that window to ~1s while still deduplicating a burst of concurrent
// misses through singleflight; the store is retried promptly.
const defaultStoreErrorTTL = 1 * time.Second

// QuotaCache resolves per-tenant quotas from a [Store] behind a short
// TTL cache so the quota-enforcement middleware never touches the
// database on the hot request path. Concurrent misses for the same
// tenant are collapsed into a single store read (singleflight). Every
// resolved quota is [Quota.Normalized], so callers always receive
// bounded values — including for tenants persisted before quotas
// existed and for unknown tenants (fail-closed).
type QuotaCache struct {
	store            Store
	ttl              time.Duration
	storeReadTimeout time.Duration
	storeErrorTTL    time.Duration
	sf               singleflight.Group

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
		store:            store,
		ttl:              ttl,
		storeReadTimeout: defaultStoreReadTimeout,
		storeErrorTTL:    defaultStoreErrorTTL,
		entries:          make(map[string]quotaEntry),
		done:             make(chan struct{}),
	}
	go c.reapLoop()
	return c
}

// Stop terminates the background reaper. Safe to call repeatedly.
func (c *QuotaCache) Stop() {
	c.stopOnce.Do(func() { close(c.done) })
}

// TenantQuota returns the effective (normalized) quota for tenantID and
// whether the tenant is known. An unknown tenant (definitive not-found)
// yields the safe default quota with found=false for a full TTL. A
// transient store error reuses the last-known quota when available
// (so an operator-set lower restriction is not raised to the default),
// else the default bound, cached only briefly so the store is retried
// promptly. Either way enforcement stays bounded rather than failing
// open.
func (c *QuotaCache) TenantQuota(ctx context.Context, tenantID string) (Quota, bool) {
	now := time.Now()
	c.mu.RLock()
	e, ok := c.entries[tenantID]
	c.mu.RUnlock()
	if ok && now.Before(e.expires) {
		return e.quota, e.found
	}

	v, _, _ := c.sf.Do(tenantID, func() (any, error) {
		// Detach from the caller's cancellation/deadline: singleflight
		// shares one goroutine's ctx across deduplicated waiters, so a
		// single client disconnect must not turn into a cached
		// context.Canceled (which would pin the tenant to default quotas
		// — a temporary quota bypass — for the whole TTL). Re-attach an
		// independent timeout so a hung store fails closed (default
		// quota) instead of blocking every waiter for this tenant.
		sfCtx, cancel := context.WithTimeout(context.WithoutCancel(ctx), c.storeReadTimeout)
		t, err := c.store.GetTenant(sfCtx, tenantID)
		cancel()

		var ent quotaEntry
		switch {
		case err == nil:
			// Resolved: cache the tenant's normalized quota for a full TTL.
			ent = quotaEntry{quota: t.Config.Quota.Normalized(), found: true,
				expires: time.Now().Add(c.ttl)}
		case errors.Is(err, ErrNotFound):
			// Definitive answer: the tenant does not exist. Fail closed to
			// the default quota for a full TTL (also shields the store from
			// repeated lookups of a nonexistent tenant).
			ent = quotaEntry{quota: DefaultQuota(), found: false,
				expires: time.Now().Add(c.ttl)}
		default:
			// Transient store error (timeout, connection failure, …). Do
			// NOT pin a fallback quota for a full TTL: that would *raise* an
			// operator-set lower quota (e.g. an abuse throttle) to the
			// default for up to one TTL — a brief restriction bypass.
			// Prefer the last-known quota (even if expired but not yet
			// reaped) so a lower restriction is retained; fall back to the
			// default bound only when nothing is known. Cache briefly
			// (storeErrorTTL) so the store is retried promptly.
			ent = quotaEntry{quota: DefaultQuota(), found: false}
			c.mu.RLock()
			prev, hadPrev := c.entries[tenantID]
			c.mu.RUnlock()
			if hadPrev {
				ent.quota = prev.quota
				ent.found = prev.found
			}
			ent.expires = time.Now().Add(c.storeErrorTTL)
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
// re-reads the store. Useful immediately after a config change. If a
// singleflight read for this tenant is already in flight it may still
// write its (pre-change) result back, so freshness is guaranteed only
// within one TTL in that narrow overlap — never a stale-unbounded value.
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
