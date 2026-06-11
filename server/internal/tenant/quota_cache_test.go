package tenant

import (
	"context"
	"errors"
	"testing"
	"time"
)

// ctxAwareStore wraps MemoryStore but honours context cancellation in
// GetTenant, letting tests exercise how QuotaCache treats a cancelled
// caller context.
type ctxAwareStore struct {
	*MemoryStore
	calls int
}

func (s *ctxAwareStore) GetTenant(ctx context.Context, id string) (Tenant, error) {
	s.calls++
	if err := ctx.Err(); err != nil {
		return Tenant{}, err
	}
	return s.MemoryStore.GetTenant(ctx, id)
}

// blockingStore simulates an unreachable/hung database: GetTenant blocks
// until the (detached) context's timeout fires.
type blockingStore struct{ *MemoryStore }

func (s *blockingStore) GetTenant(ctx context.Context, _ string) (Tenant, error) {
	<-ctx.Done()
	return Tenant{}, ctx.Err()
}

// flakyStore wraps MemoryStore and, while failing is set, returns a
// transient (non-ErrNotFound) error from GetTenant — simulating a DB
// connection blip distinct from a definitive "tenant not found".
type flakyStore struct {
	*MemoryStore
	failing bool
}

func (s *flakyStore) GetTenant(ctx context.Context, id string) (Tenant, error) {
	if s.failing {
		return Tenant{}, errors.New("dial tcp: connection refused")
	}
	return s.MemoryStore.GetTenant(ctx, id)
}

// TestQuotaCacheTransientErrorKeepsLastKnownQuota guards against a
// transient store error raising an operator-set lower quota to the
// default. A tenant throttled below default is resolved once, then the
// store starts failing transiently; within that window the cache must
// keep serving the throttled quota (last-known), never the higher
// default — and the failure must be cached only briefly so the store is
// retried promptly once it recovers.
func TestQuotaCacheTransientErrorKeepsLastKnownQuota(t *testing.T) {
	t.Parallel()
	store := &flakyStore{MemoryStore: NewMemoryStore()}
	throttled := Quota{RequestsPerMin: 10, SynthesesPerDay: 2, StorageSoftCapBytes: 1 << 20}
	if err := store.CreateTenant(context.Background(), Tenant{ID: "t1", Config: Config{Quota: throttled}}); err != nil {
		t.Fatal(err)
	}

	c := NewQuotaCache(store, time.Minute)
	c.storeErrorTTL = 20 * time.Millisecond
	defer c.Stop()

	// Resolve the real (low) quota, then expire the cached entry and make
	// the store fail transiently.
	if q, found := c.TenantQuota(context.Background(), "t1"); !found || q != throttled {
		t.Fatalf("initial resolve = %+v found=%v, want %+v", q, found, throttled)
	}
	c.mu.Lock()
	e := c.entries["t1"]
	e.expires = time.Now().Add(-time.Second) // force a re-read on next lookup
	c.entries["t1"] = e
	c.mu.Unlock()
	store.failing = true

	q, found := c.TenantQuota(context.Background(), "t1")
	if !found || q != throttled {
		t.Fatalf("during transient error quota = %+v found=%v, want last-known %+v (default would bypass the throttle)", q, found, throttled)
	}
	if q.RequestsPerMin == DefaultQuota().RequestsPerMin {
		t.Fatal("transient error raised the throttled tenant to the default quota")
	}

	// Once the store recovers, the short error TTL must let the next
	// lookup re-resolve rather than pinning the fallback for a full TTL.
	time.Sleep(40 * time.Millisecond)
	store.failing = false
	if q, found := c.TenantQuota(context.Background(), "t1"); !found || q != throttled {
		t.Fatalf("after recovery quota = %+v found=%v, want %+v", q, found, throttled)
	}
}

// TestQuotaCacheStoreReadIsBounded verifies that detaching the store read
// from the caller's context does not make it unbounded: the read is
// capped by storeReadTimeout, so a hung store fails closed to the safe
// default quota rather than stalling every quota lookup for the tenant.
func TestQuotaCacheStoreReadIsBounded(t *testing.T) {
	t.Parallel()
	c := NewQuotaCache(&blockingStore{MemoryStore: NewMemoryStore()}, time.Minute)
	c.storeReadTimeout = 50 * time.Millisecond
	defer c.Stop()

	type result struct {
		q     Quota
		found bool
	}
	ch := make(chan result, 1)
	go func() {
		q, found := c.TenantQuota(context.Background(), "t1")
		ch <- result{q, found}
	}()

	select {
	case got := <-ch:
		if got.found {
			t.Fatal("hung store must fail closed with found=false")
		}
		if got.q != DefaultQuota() {
			t.Fatalf("quota = %+v, want default %+v", got.q, DefaultQuota())
		}
	case <-time.After(2 * time.Second):
		t.Fatal("TenantQuota did not return; detached store read was not bounded")
	}
}

// TestQuotaCacheIgnoresCallerCancellation guards the fix for the
// singleflight closure caching a context.Canceled result (default quota,
// found=false) when the triggering request is cancelled — which would
// pin the tenant to defaults for the whole TTL (a quota bypass). The
// cache must detach from the caller's cancellation and still resolve the
// real, configured quota.
func TestQuotaCacheIgnoresCallerCancellation(t *testing.T) {
	t.Parallel()
	store := &ctxAwareStore{MemoryStore: NewMemoryStore()}
	want := Quota{RequestsPerMin: 9, SynthesesPerDay: 3, StorageSoftCapBytes: 1 << 20}
	if err := store.CreateTenant(context.Background(), Tenant{ID: "t1", Config: Config{Quota: want}}); err != nil {
		t.Fatal(err)
	}

	c := NewQuotaCache(store, time.Minute)
	defer c.Stop()

	ctx, cancel := context.WithCancel(context.Background())
	cancel() // caller context already cancelled

	q, found := c.TenantQuota(ctx, "t1")
	if !found {
		t.Fatal("tenant should resolve despite cancelled caller context")
	}
	if q != want {
		t.Fatalf("quota = %+v, want %+v (cancellation poisoned the cache?)", q, want)
	}
}

// TestQuotaCacheUnknownTenantFailsClosed verifies an unknown tenant
// resolves to the safe default quota with found=false rather than an
// unbounded one.
func TestQuotaCacheUnknownTenantFailsClosed(t *testing.T) {
	t.Parallel()
	c := NewQuotaCache(NewMemoryStore(), time.Minute)
	defer c.Stop()

	q, found := c.TenantQuota(context.Background(), "nope")
	if found {
		t.Fatal("unknown tenant must not be found")
	}
	if q != DefaultQuota() {
		t.Fatalf("quota = %+v, want default %+v", q, DefaultQuota())
	}
}

// TestQuotaCacheCachesAndInvalidates verifies repeated lookups hit the
// cache (single store read) and that Invalidate forces a re-read.
func TestQuotaCacheCachesAndInvalidates(t *testing.T) {
	t.Parallel()
	store := &ctxAwareStore{MemoryStore: NewMemoryStore()}
	if err := store.CreateTenant(context.Background(), Tenant{ID: "t1", Config: DefaultConfig()}); err != nil {
		t.Fatal(err)
	}
	c := NewQuotaCache(store, time.Minute)
	defer c.Stop()

	for i := 0; i < 5; i++ {
		c.TenantQuota(context.Background(), "t1")
	}
	if store.calls != 1 {
		t.Fatalf("store reads = %d, want 1 (cache miss only once)", store.calls)
	}
	c.Invalidate("t1")
	c.TenantQuota(context.Background(), "t1")
	if store.calls != 2 {
		t.Fatalf("store reads = %d, want 2 after invalidate", store.calls)
	}
}
