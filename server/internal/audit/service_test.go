package audit

import (
	"context"
	"testing"
	"time"

	"go.uber.org/zap"
)

func newTestService() *Service {
	return NewService(zap.NewNop())
}

func TestRecord(t *testing.T) {
	svc := newTestService()
	err := svc.Record(context.Background(), &Event{
		TenantID: "t1",
		ScopeID:  "s1",
		Action:   "test.action",
		ActorID:  "actor-1",
	})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}

	events, _ := svc.Query(context.Background(), &QueryParams{TenantID: "t1"})
	if len(events) != 1 {
		t.Fatalf("event count = %d, want 1", len(events))
	}
	if events[0].Action != "test.action" {
		t.Errorf("action = %q, want %q", events[0].Action, "test.action")
	}
	if events[0].ID == "" {
		t.Error("expected non-empty event ID")
	}
}

func TestQuery_FilterByTenant(t *testing.T) {
	svc := newTestService()
	ctx := context.Background()

	_ = svc.Record(ctx, &Event{TenantID: "t1", Action: "a1", ActorID: "u1"})
	_ = svc.Record(ctx, &Event{TenantID: "t2", Action: "a2", ActorID: "u2"})
	_ = svc.Record(ctx, &Event{TenantID: "t1", Action: "a3", ActorID: "u1"})

	events, _ := svc.Query(ctx, &QueryParams{TenantID: "t1"})
	if len(events) != 2 {
		t.Errorf("event count = %d, want 2", len(events))
	}
}

func TestQuery_FilterByAction(t *testing.T) {
	svc := newTestService()
	ctx := context.Background()

	_ = svc.Record(ctx, &Event{TenantID: "t1", Action: "ingest", ActorID: "u1"})
	_ = svc.Record(ctx, &Event{TenantID: "t1", Action: "query", ActorID: "u1"})
	_ = svc.Record(ctx, &Event{TenantID: "t1", Action: "ingest", ActorID: "u1"})

	events, _ := svc.Query(ctx, &QueryParams{Action: "ingest"})
	if len(events) != 2 {
		t.Errorf("event count = %d, want 2", len(events))
	}
}

func TestQuery_FilterByActor(t *testing.T) {
	svc := newTestService()
	ctx := context.Background()

	_ = svc.Record(ctx, &Event{TenantID: "t1", Action: "a", ActorID: "u1"})
	_ = svc.Record(ctx, &Event{TenantID: "t1", Action: "a", ActorID: "u2"})

	events, _ := svc.Query(ctx, &QueryParams{ActorID: "u1"})
	if len(events) != 1 {
		t.Errorf("event count = %d, want 1", len(events))
	}
}

func TestQuery_FilterByTimeRange(t *testing.T) {
	svc := newTestService()
	ctx := context.Background()

	now := time.Now().UTC()
	past := now.Add(-2 * time.Hour)
	future := now.Add(2 * time.Hour)

	_ = svc.Record(ctx, &Event{TenantID: "t1", Action: "a", ActorID: "u1", CreatedAt: past})
	_ = svc.Record(ctx, &Event{TenantID: "t1", Action: "a", ActorID: "u1", CreatedAt: now})
	_ = svc.Record(ctx, &Event{TenantID: "t1", Action: "a", ActorID: "u1", CreatedAt: future})

	since := now.Add(-time.Hour)
	until := now.Add(time.Hour)
	events, _ := svc.Query(ctx, &QueryParams{Since: &since, Until: &until})
	if len(events) != 1 {
		t.Errorf("event count = %d, want 1", len(events))
	}
}

func TestQuery_Limit(t *testing.T) {
	svc := newTestService()
	ctx := context.Background()

	for i := 0; i < 10; i++ {
		_ = svc.Record(ctx, &Event{TenantID: "t1", Action: "a", ActorID: "u1"})
	}

	events, _ := svc.Query(ctx, &QueryParams{Limit: 3})
	if len(events) != 3 {
		t.Errorf("event count = %d, want 3", len(events))
	}
}

func TestQuery_DefaultLimit(t *testing.T) {
	svc := newTestService()
	ctx := context.Background()

	for i := 0; i < 150; i++ {
		_ = svc.Record(ctx, &Event{TenantID: "t1", Action: "a", ActorID: "u1"})
	}

	events, _ := svc.Query(ctx, &QueryParams{})
	if len(events) != 100 {
		t.Errorf("event count = %d, want 100 (default limit)", len(events))
	}
}

func TestSetRetentionPolicy(t *testing.T) {
	svc := newTestService()
	ctx := context.Background()

	err := svc.SetRetentionPolicy(ctx, &RetentionPolicy{
		TenantID:      "t1",
		RetentionDays: 90,
	})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestSetRetentionPolicy_InvalidDays(t *testing.T) {
	svc := newTestService()
	err := svc.SetRetentionPolicy(context.Background(), &RetentionPolicy{
		TenantID:      "t1",
		RetentionDays: 0,
	})
	if err == nil {
		t.Fatal("expected error for zero retention days")
	}
}

func TestRetentionEnforcement(t *testing.T) {
	svc := newTestService()
	ctx := context.Background()

	old := time.Now().UTC().Add(-100 * 24 * time.Hour)
	recent := time.Now().UTC()

	_ = svc.Record(ctx, &Event{TenantID: "t1", Action: "old", ActorID: "u1", CreatedAt: old})
	_ = svc.Record(ctx, &Event{TenantID: "t1", Action: "recent", ActorID: "u1", CreatedAt: recent})

	_ = svc.SetRetentionPolicy(ctx, &RetentionPolicy{TenantID: "t1", RetentionDays: 30})

	svc.enforceRetention()

	events, _ := svc.Query(ctx, &QueryParams{TenantID: "t1"})
	if len(events) != 1 {
		t.Fatalf("event count = %d, want 1 after retention enforcement", len(events))
	}
	if events[0].Action != "recent" {
		t.Errorf("remaining event action = %q, want %q", events[0].Action, "recent")
	}
}
