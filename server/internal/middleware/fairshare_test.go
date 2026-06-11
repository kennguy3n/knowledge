package middleware

import (
	"context"
	"errors"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
)

// waitForWaiting polls until the semaphore reports n blocked waiters,
// or fails the test. Used to make queue-ordering tests deterministic
// without sleeping for a fixed (flaky) duration.
func waitForWaiting(t *testing.T, s *semaphore, n int32) {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if atomic.LoadInt32(&s.waiting) == n {
			return
		}
		time.Sleep(time.Millisecond)
	}
	t.Fatalf("waiters = %d, want %d", atomic.LoadInt32(&s.waiting), n)
}

func assertThrottled(t *testing.T, err error) {
	t.Helper()
	var apiErr *httpx.Error
	if !errors.As(err, &apiErr) {
		t.Fatalf("error = %v, want *httpx.Error", err)
	}
	if apiErr.Status != 429 {
		t.Fatalf("status = %d, want 429", apiErr.Status)
	}
}

// TestFairShareTenantCapIsolation verifies the core fairness property:
// a tenant at its concurrency cap is throttled, while a *different*
// tenant proceeds unaffected.
func TestFairShareTenantCapIsolation(t *testing.T) {
	t.Parallel()
	f := NewSynthesisFairShare(FairShareConfig{
		TenantConcurrency: 1,
		TenantQueue:       0,
		GlobalConcurrency: 10,
		QueueWait:         50 * time.Millisecond,
	})
	defer f.Stop()
	ctx := context.Background()

	relA, _, err := f.Acquire(ctx, "tenant-a")
	if err != nil {
		t.Fatalf("tenant-a first acquire: %v", err)
	}
	defer relA()

	// tenant-a is at cap (1) with no queue: the next acquire is shed.
	if _, ra, err := f.Acquire(ctx, "tenant-a"); err == nil {
		t.Fatal("tenant-a over-cap acquire should be throttled")
	} else {
		assertThrottled(t, err)
		if ra < 1 {
			t.Fatalf("retry-after = %d, want >= 1", ra)
		}
	}

	// A different tenant is completely unaffected by tenant-a's load.
	relB, _, err := f.Acquire(ctx, "tenant-b")
	if err != nil {
		t.Fatalf("tenant-b acquire starved by tenant-a: %v", err)
	}
	relB()
}

// TestFairShareReleaseFreesSlot verifies a released slot is reusable by
// the same tenant.
func TestFairShareReleaseFreesSlot(t *testing.T) {
	t.Parallel()
	f := NewSynthesisFairShare(FairShareConfig{
		TenantConcurrency: 1, TenantQueue: 0, GlobalConcurrency: 4, QueueWait: 50 * time.Millisecond,
	})
	defer f.Stop()
	ctx := context.Background()

	rel, _, err := f.Acquire(ctx, "t")
	if err != nil {
		t.Fatalf("acquire: %v", err)
	}
	rel()
	rel() // idempotent: a second release must not over-credit the pool.

	// Slot is free again.
	rel2, _, err := f.Acquire(ctx, "t")
	if err != nil {
		t.Fatalf("re-acquire after release: %v", err)
	}
	defer rel2()
	// The idempotent double-release must not have inflated capacity:
	// a second concurrent acquire is still throttled.
	if _, _, err := f.Acquire(ctx, "t"); err == nil {
		t.Fatal("capacity inflated by double release")
	}
}

// TestFairShareBoundedQueue verifies the bounded FIFO queue: requests
// beyond cap+queue are shed immediately (no wait), and a queued request
// is admitted FIFO once a slot frees.
func TestFairShareBoundedQueue(t *testing.T) {
	t.Parallel()
	f := NewSynthesisFairShare(FairShareConfig{
		TenantConcurrency: 1,
		TenantQueue:       1,
		GlobalConcurrency: 10,
		QueueWait:         2 * time.Second,
	})
	defer f.Stop()
	ctx := context.Background()
	g := f.gate("t")

	// Hold the single slot.
	rel, _, err := f.Acquire(ctx, "t")
	if err != nil {
		t.Fatalf("first acquire: %v", err)
	}

	// One queued waiter is allowed; it blocks until the slot frees.
	queued := make(chan error, 1)
	go func() {
		r, _, e := f.Acquire(ctx, "t")
		if e == nil {
			r()
		}
		queued <- e
	}()
	waitForWaiting(t, g.sem, 1)

	// Queue is now full (cap=1 in use, 1 waiting): a further acquire is
	// shed immediately rather than waiting QueueWait.
	start := time.Now()
	if _, _, err := f.Acquire(ctx, "t"); err == nil {
		t.Fatal("acquire past cap+queue should be throttled")
	} else {
		assertThrottled(t, err)
	}
	if elapsed := time.Since(start); elapsed > 500*time.Millisecond {
		t.Fatalf("over-queue acquire blocked for %v; should fail fast", elapsed)
	}

	// Free the slot: the queued waiter must now be admitted.
	rel()
	select {
	case e := <-queued:
		if e != nil {
			t.Fatalf("queued waiter not admitted after release: %v", e)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("queued waiter never admitted")
	}
}

// TestFairShareGlobalCap verifies the global cap protects the shared
// pool even when spread across many distinct tenants, each below its
// own per-tenant cap.
func TestFairShareGlobalCap(t *testing.T) {
	t.Parallel()
	f := NewSynthesisFairShare(FairShareConfig{
		TenantConcurrency: 5,
		TenantQueue:       0,
		GlobalConcurrency: 2,
		QueueWait:         50 * time.Millisecond,
	})
	defer f.Stop()
	ctx := context.Background()

	r1, _, err := f.Acquire(ctx, "a")
	if err != nil {
		t.Fatalf("acquire a: %v", err)
	}
	defer r1()
	r2, _, err := f.Acquire(ctx, "b")
	if err != nil {
		t.Fatalf("acquire b: %v", err)
	}
	defer r2()

	// Global cap (2) is now exhausted; a third distinct tenant, despite
	// being far under its per-tenant cap, is shed to protect the pool.
	if _, _, err := f.Acquire(ctx, "c"); err == nil {
		t.Fatal("global cap not enforced across tenants")
	} else {
		assertThrottled(t, err)
	}
}

// TestFairShareContextCancel verifies a cancelled context aborts a
// queued wait promptly (fail-closed).
func TestFairShareContextCancel(t *testing.T) {
	t.Parallel()
	f := NewSynthesisFairShare(FairShareConfig{
		TenantConcurrency: 1, TenantQueue: 4, GlobalConcurrency: 10, QueueWait: 10 * time.Second,
	})
	defer f.Stop()

	rel, _, err := f.Acquire(context.Background(), "t")
	if err != nil {
		t.Fatalf("acquire: %v", err)
	}
	defer rel()

	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() {
		_, _, e := f.Acquire(ctx, "t")
		done <- e
	}()
	waitForWaiting(t, f.gate("t").sem, 1)
	cancel()
	select {
	case e := <-done:
		if e == nil {
			t.Fatal("cancelled acquire should not succeed")
		}
	case <-time.After(2 * time.Second):
		t.Fatal("cancelled acquire did not return promptly")
	}
}

// TestFairShareDefaults verifies invalid (negative / zero-concurrency)
// config falls back to safe production defaults (fail-closed), while an
// explicit zero queue is preserved as "cap with no queue".
func TestFairShareDefaults(t *testing.T) {
	t.Parallel()
	// Non-positive concurrency / wait must be replaced; a disabled
	// limiter would let a single tenant starve the pool.
	cfg := FairShareConfig{TenantConcurrency: -1, GlobalConcurrency: 0, QueueWait: 0}.withDefaults()
	if cfg.TenantConcurrency != defaultSynthTenantConcurrency ||
		cfg.GlobalConcurrency != defaultSynthGlobalConcurrency ||
		cfg.QueueWait != defaultSynthQueueWait {
		t.Fatalf("defaults not applied: %+v", cfg)
	}
	// An explicit zero queue is a valid "cap, no waiting" configuration
	// and must be preserved.
	if got := (FairShareConfig{TenantConcurrency: 1, TenantQueue: 0, GlobalConcurrency: 1, QueueWait: time.Second}).withDefaults(); got.TenantQueue != 0 {
		t.Fatalf("explicit zero queue not preserved: %d", got.TenantQueue)
	}
	// A negative queue (invalid) falls back to the default depth.
	if got := (FairShareConfig{TenantQueue: -3}).withDefaults(); got.TenantQueue != defaultSynthTenantQueue {
		t.Fatalf("negative queue not defaulted: %d", got.TenantQueue)
	}
}

// TestFairShareGlobalDefaultMatchesSingleReplicaPool pins the global
// concurrency fallback to 2 so a bare gateway process (env unset) sizes
// admission to the chart's default single-replica llama-server pool
// instead of oversubscribing it 4×. The deploy surfaces
// (docker-compose, Helm values) pin the same value; this keeps the code
// default consistent with them.
func TestFairShareGlobalDefaultMatchesSingleReplicaPool(t *testing.T) {
	if defaultSynthGlobalConcurrency != 2 {
		t.Fatalf("global concurrency default = %d, want 2 (single-replica pool; do not oversubscribe)", defaultSynthGlobalConcurrency)
	}
	// With the env var unset, the fallback must flow through to the
	// live controller config.
	t.Setenv(EnvSynthGlobalConcurrency, "")
	f := NewSynthesisFairShareFromEnv()
	defer f.Stop()
	if f.cfg.GlobalConcurrency != 2 {
		t.Fatalf("FromEnv GlobalConcurrency = %d, want 2", f.cfg.GlobalConcurrency)
	}
}

// TestFairShareGateStampedBeforeVisible verifies a freshly-created gate
// carries a fresh lastSeen and survives an immediate reap — guarding
// against the race where a zero lastSeen let the sweeper evict a gate
// before its first acquire (which would transiently double the cap).
func TestFairShareGateStampedBeforeVisible(t *testing.T) {
	t.Parallel()
	f := NewSynthesisFairShare(FairShareConfig{
		TenantConcurrency: 1, TenantQueue: 0, GlobalConcurrency: 4, QueueWait: 50 * time.Millisecond,
	})
	defer f.Stop()

	g := f.gate("fresh")
	if atomic.LoadInt64(&g.lastSeen) == 0 {
		t.Fatal("new gate has zero lastSeen; reap() could evict it before first use")
	}
	f.reap() // idle but recently stamped: must NOT be reclaimed.
	f.mu.Lock()
	_, ok := f.gates["fresh"]
	f.mu.Unlock()
	if !ok {
		t.Fatal("freshly-created gate was reaped")
	}
}

// TestFairShareConcurrentInvariant hammers the controller from many
// goroutines and asserts the per-tenant and global caps are never
// exceeded. Run under -race to catch data races.
func TestFairShareConcurrentInvariant(t *testing.T) {
	t.Parallel()
	const (
		tenants    = 8
		perTenant  = 3
		globalCap  = 6
		goroutines = 200
	)
	f := NewSynthesisFairShare(FairShareConfig{
		TenantConcurrency: perTenant,
		TenantQueue:       8,
		GlobalConcurrency: globalCap,
		QueueWait:         200 * time.Millisecond,
	})
	defer f.Stop()

	var (
		globalInFlight int64
		maxGlobal      int64
		perInFlight    [tenants]int64
		maxPer         [tenants]int64
		muMax          sync.Mutex
	)
	var wg sync.WaitGroup
	for i := 0; i < goroutines; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			tid := []string{"t0", "t1", "t2", "t3", "t4", "t5", "t6", "t7"}[i%tenants]
			rel, _, err := f.Acquire(context.Background(), tid)
			if err != nil {
				return // throttled is a valid outcome under contention
			}
			gNow := atomic.AddInt64(&globalInFlight, 1)
			pNow := atomic.AddInt64(&perInFlight[i%tenants], 1)
			muMax.Lock()
			if gNow > maxGlobal {
				maxGlobal = gNow
			}
			if pNow > maxPer[i%tenants] {
				maxPer[i%tenants] = pNow
			}
			muMax.Unlock()
			time.Sleep(time.Millisecond)
			atomic.AddInt64(&perInFlight[i%tenants], -1)
			atomic.AddInt64(&globalInFlight, -1)
			rel()
		}(i)
	}
	wg.Wait()

	if maxGlobal > globalCap {
		t.Fatalf("global cap exceeded: peak %d > %d", maxGlobal, globalCap)
	}
	for i := 0; i < tenants; i++ {
		if maxPer[i] > perTenant {
			t.Fatalf("tenant %d cap exceeded: peak %d > %d", i, maxPer[i], perTenant)
		}
	}
}
