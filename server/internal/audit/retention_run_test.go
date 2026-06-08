package audit

import (
	"context"
	"errors"
	"testing"
	"time"

	"go.uber.org/zap"
)

type errTenants struct{}

func (errTenants) TenantIDs(context.Context) ([]string, error) { return nil, errors.New("list failed") }

func TestRetentionRunCancels(t *testing.T) {
	t.Parallel()
	store := NewMemoryStore()
	_ = store.Append(context.Background(), Event{ID: "old", TenantID: "t1", Action: "x", Actor: "a", CreatedAt: time.Now().AddDate(0, 0, -40)})
	r := NewRetention(store, fakeTenants{ids: []string{"t1"}}, fakeResolver{"t1": 30}, time.Millisecond, zap.NewNop())

	ctx, cancel := context.WithTimeout(context.Background(), 50*time.Millisecond)
	defer cancel()
	r.Run(ctx) // runs an immediate sweep, ticks, then returns on ctx done

	got, _ := store.Query(context.Background(), Filter{TenantID: "t1"})
	if len(got) != 0 {
		t.Fatalf("expected retention to delete old event, left %d", len(got))
	}
}

func TestRetentionSweepSkipsUnknownAndError(t *testing.T) {
	t.Parallel()
	store := NewMemoryStore()
	// Unknown tenant (resolver returns ok=false) is skipped without error.
	r := NewRetention(store, fakeTenants{ids: []string{"unknown"}}, fakeResolver{}, time.Minute, nil)
	if n, err := r.Sweep(context.Background()); err != nil || n != 0 {
		t.Fatalf("unknown tenant: n=%d err=%v", n, err)
	}
	// Tenant lister error propagates.
	r = NewRetention(store, errTenants{}, fakeResolver{}, time.Minute, nil)
	if _, err := r.Sweep(context.Background()); err == nil {
		t.Fatal("expected lister error")
	}
}

func TestConsumerPersistUnit(t *testing.T) {
	t.Parallel()
	c := NewConsumer(NewMemoryStore(), nil)
	e, err := c.persist(context.Background(), Event{TenantID: "t1", Action: "ingest", Actor: "a"})
	if err != nil || e.ID == "" {
		t.Fatalf("persist: id=%q err=%v", e.ID, err)
	}
}
