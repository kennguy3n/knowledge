package tenant

import (
	"context"
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
