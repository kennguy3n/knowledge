package audit

import (
	"context"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"go.uber.org/zap"
)

func TestMemoryStoreAppendQuery(t *testing.T) {
	t.Parallel()
	ctx := context.Background()
	s := NewMemoryStore()
	now := time.Now().UTC()
	events := []Event{
		{ID: "1", TenantID: "t1", Action: "export", Actor: "a1", CreatedAt: now.Add(-2 * time.Hour)},
		{ID: "2", TenantID: "t1", Action: "ingest", Actor: "a2", CreatedAt: now.Add(-1 * time.Hour)},
		{ID: "3", TenantID: "t2", Action: "export", Actor: "a1", CreatedAt: now},
	}
	for _, e := range events {
		if err := s.Append(ctx, e); err != nil {
			t.Fatal(err)
		}
	}

	// Filter by tenant; newest-first.
	got, err := s.Query(ctx, Filter{TenantID: "t1"})
	if err != nil {
		t.Fatal(err)
	}
	if len(got) != 2 || got[0].ID != "2" {
		t.Fatalf("tenant filter wrong: %+v", got)
	}

	// Filter by action+actor.
	got, _ = s.Query(ctx, Filter{Action: "export", Actor: "a1"})
	if len(got) != 2 {
		t.Fatalf("action/actor filter: %d", len(got))
	}

	// Limit clamp.
	got, _ = s.Query(ctx, Filter{Limit: 1})
	if len(got) != 1 {
		t.Fatalf("limit: %d", len(got))
	}
}

func TestMemoryStoreDeleteOlderThan(t *testing.T) {
	t.Parallel()
	ctx := context.Background()
	s := NewMemoryStore()
	now := time.Now().UTC()
	_ = s.Append(ctx, Event{ID: "old", TenantID: "t1", Action: "x", Actor: "a", CreatedAt: now.Add(-48 * time.Hour)})
	_ = s.Append(ctx, Event{ID: "new", TenantID: "t1", Action: "x", Actor: "a", CreatedAt: now})

	n, err := s.DeleteOlderThan(ctx, "t1", now.Add(-24*time.Hour))
	if err != nil || n != 1 {
		t.Fatalf("deleted=%d err=%v", n, err)
	}
	got, _ := s.Query(ctx, Filter{TenantID: "t1"})
	if len(got) != 1 || got[0].ID != "new" {
		t.Fatalf("remaining wrong: %+v", got)
	}
}

func TestServiceRecordValidation(t *testing.T) {
	t.Parallel()
	s := New(NewMemoryStore())
	if _, err := s.Record(context.Background(), Event{}); err == nil {
		t.Fatal("expected validation error")
	}
	e, err := s.Record(context.Background(), Event{TenantID: "t1", Action: "export", Actor: "a"})
	if err != nil {
		t.Fatal(err)
	}
	if e.ID == "" || e.CreatedAt.IsZero() {
		t.Fatalf("id/timestamp not minted: %+v", e)
	}
}

func TestServiceHandleQuery(t *testing.T) {
	t.Parallel()
	s := New(NewMemoryStore())
	_, _ = s.Record(context.Background(), Event{TenantID: "t1", Action: "export", Actor: "a"})

	rec := httptest.NewRecorder()
	req := httptest.NewRequest(http.MethodGet, "/?tenant_id=t1&action=export", nil)
	s.Routes().ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Fatalf("code = %d body=%s", rec.Code, rec.Body.String())
	}
}

type fakeTenants struct{ ids []string }

func (f fakeTenants) TenantIDs(context.Context) ([]string, error) { return f.ids, nil }

type fakeResolver map[string]int

func (f fakeResolver) RetentionDays(_ context.Context, id string) (int, bool) {
	d, ok := f[id]
	return d, ok
}

func TestRetentionSweep(t *testing.T) {
	t.Parallel()
	ctx := context.Background()
	store := NewMemoryStore()
	now := time.Now().UTC()
	_ = store.Append(ctx, Event{ID: "old", TenantID: "t1", Action: "x", Actor: "a", CreatedAt: now.AddDate(0, 0, -40)})
	_ = store.Append(ctx, Event{ID: "new", TenantID: "t1", Action: "x", Actor: "a", CreatedAt: now})

	r := NewRetention(store, fakeTenants{ids: []string{"t1"}}, fakeResolver{"t1": 30}, time.Minute, zap.NewNop())
	n, err := r.Sweep(ctx)
	if err != nil || n != 1 {
		t.Fatalf("swept=%d err=%v", n, err)
	}
	got, _ := store.Query(ctx, Filter{TenantID: "t1"})
	if len(got) != 1 {
		t.Fatalf("retention left %d events", len(got))
	}
}
