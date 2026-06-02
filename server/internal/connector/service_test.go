package connector

import (
	"context"
	"testing"

	"go.uber.org/zap"

	"github.com/kennguy3n/knowledge/server/internal/substrate"
)

func newTestService() *Service {
	sub := substrate.NewClient("http://127.0.0.1:19090", 0)
	return NewService(sub, zap.NewNop())
}

func TestCreate(t *testing.T) {
	svc := newTestService()
	inst, err := svc.Create(context.Background(), &CreateRequest{
		TenantID: "tenant-1",
		ScopeID:  "scope-1",
		Kind:     KindSlack,
		Name:     "My Slack",
	})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if inst.ID == "" {
		t.Error("expected non-empty ID")
	}
	if inst.Kind != KindSlack {
		t.Errorf("kind = %q, want %q", inst.Kind, KindSlack)
	}
	if inst.Status != "created" {
		t.Errorf("status = %q, want %q", inst.Status, "created")
	}
}

func TestCreate_CustomSyncInterval(t *testing.T) {
	svc := newTestService()
	interval := "30m"
	inst, err := svc.Create(context.Background(), &CreateRequest{
		TenantID:     "tenant-1",
		ScopeID:      "scope-1",
		Kind:         KindNotion,
		Name:         "Notion",
		SyncInterval: &interval,
	})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if inst.SyncInterval.Minutes() != 30 {
		t.Errorf("sync interval = %v, want 30m", inst.SyncInterval)
	}
}

func TestCreate_InvalidSyncInterval(t *testing.T) {
	svc := newTestService()
	interval := "not-a-duration"
	_, err := svc.Create(context.Background(), &CreateRequest{
		TenantID:     "tenant-1",
		ScopeID:      "scope-1",
		Kind:         KindSlack,
		Name:         "Bad",
		SyncInterval: &interval,
	})
	if err == nil {
		t.Fatal("expected error for invalid sync interval")
	}
}

func TestList(t *testing.T) {
	svc := newTestService()

	_, _ = svc.Create(context.Background(), &CreateRequest{
		TenantID: "tenant-1", ScopeID: "s1", Kind: KindSlack, Name: "A",
	})
	_, _ = svc.Create(context.Background(), &CreateRequest{
		TenantID: "tenant-2", ScopeID: "s2", Kind: KindEmail, Name: "B",
	})

	all := svc.List(context.Background(), "")
	if len(all) != 2 {
		t.Errorf("all count = %d, want 2", len(all))
	}

	filtered := svc.List(context.Background(), "tenant-1")
	if len(filtered) != 1 {
		t.Errorf("filtered count = %d, want 1", len(filtered))
	}
}

func TestGet_NotFound(t *testing.T) {
	svc := newTestService()
	_, err := svc.Get("nonexistent")
	if err == nil {
		t.Fatal("expected error for nonexistent connector")
	}
}

func TestAuthenticate(t *testing.T) {
	svc := newTestService()
	inst, _ := svc.Create(context.Background(), &CreateRequest{
		TenantID: "t1", ScopeID: "s1", Kind: KindGitHub, Name: "GH",
	})

	resp, err := svc.Authenticate(context.Background(), inst.ID)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if resp.RedirectURL == "" {
		t.Error("expected non-empty redirect URL")
	}

	got, _ := svc.Get(inst.ID)
	if !got.Authenticated {
		t.Error("connector should be authenticated after Authenticate()")
	}
}

func TestAuthenticate_NotFound(t *testing.T) {
	svc := newTestService()
	_, err := svc.Authenticate(context.Background(), "nonexistent")
	if err == nil {
		t.Fatal("expected error for nonexistent connector")
	}
}

func TestRemove(t *testing.T) {
	svc := newTestService()
	inst, _ := svc.Create(context.Background(), &CreateRequest{
		TenantID: "t1", ScopeID: "s1", Kind: KindSlack, Name: "S",
	})

	if err := svc.Remove(context.Background(), inst.ID); err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	_, err := svc.Get(inst.ID)
	if err == nil {
		t.Fatal("expected error after removal")
	}
}

func TestRemove_NotFound(t *testing.T) {
	svc := newTestService()
	err := svc.Remove(context.Background(), "nonexistent")
	if err == nil {
		t.Fatal("expected error for nonexistent connector")
	}
}

func TestGetStatus(t *testing.T) {
	svc := newTestService()
	inst, _ := svc.Create(context.Background(), &CreateRequest{
		TenantID: "t1", ScopeID: "s1", Kind: KindSlack, Name: "S",
	})

	status, err := svc.GetStatus(inst.ID)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if status.Status != "created" {
		t.Errorf("status = %q, want %q", status.Status, "created")
	}
}

func TestGetStatus_NotFound(t *testing.T) {
	svc := newTestService()
	_, err := svc.GetStatus("nonexistent")
	if err == nil {
		t.Fatal("expected error for nonexistent connector")
	}
}
