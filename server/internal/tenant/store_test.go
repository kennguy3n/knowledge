package tenant

import (
	"context"
	"errors"
	"testing"
	"time"
)

func TestMemoryStoreTenantCRUD(t *testing.T) {
	t.Parallel()
	ctx := context.Background()
	s := NewMemoryStore()

	tn := Tenant{ID: "t1", Name: "Acme", Config: DefaultConfig(), CreatedAt: time.Now()}
	if err := s.CreateTenant(ctx, tn); err != nil {
		t.Fatal(err)
	}
	if err := s.CreateTenant(ctx, tn); !errors.Is(err, ErrConflict) {
		t.Fatalf("expected conflict, got %v", err)
	}
	got, err := s.GetTenant(ctx, "t1")
	if err != nil || got.Name != "Acme" {
		t.Fatalf("get: %+v err=%v", got, err)
	}
	if _, err := s.GetTenant(ctx, "missing"); !errors.Is(err, ErrNotFound) {
		t.Fatalf("expected not found, got %v", err)
	}

	tn.Name = "Acme2"
	if err := s.UpdateTenant(ctx, tn); err != nil {
		t.Fatal(err)
	}
	got, _ = s.GetTenant(ctx, "t1")
	if got.Name != "Acme2" {
		t.Fatalf("update not applied: %+v", got)
	}

	list, err := s.ListTenants(ctx)
	if err != nil || len(list) != 1 {
		t.Fatalf("list: %v len=%d", err, len(list))
	}

	if err := s.DeleteTenant(ctx, "t1"); err != nil {
		t.Fatal(err)
	}
	if _, err := s.GetTenant(ctx, "t1"); !errors.Is(err, ErrNotFound) {
		t.Fatalf("expected gone, got %v", err)
	}
}

func TestMemoryStoreMembers(t *testing.T) {
	t.Parallel()
	ctx := context.Background()
	s := NewMemoryStore()
	_ = s.CreateTenant(ctx, Tenant{ID: "t1", Name: "Acme", Config: DefaultConfig()})

	m := Member{TenantID: "t1", UserID: "u1", Email: "u1@x.io", Status: StatusInvited}
	if err := s.UpsertMember(ctx, m); err != nil {
		t.Fatal(err)
	}
	m.Status = StatusActive
	if err := s.UpsertMember(ctx, m); err != nil {
		t.Fatal(err)
	}
	got, err := s.GetMember(ctx, "t1", "u1")
	if err != nil || got.Status != StatusActive {
		t.Fatalf("member: %+v err=%v", got, err)
	}
	members, err := s.ListMembers(ctx, "t1")
	if err != nil || len(members) != 1 {
		t.Fatalf("list members: %v len=%d", err, len(members))
	}
	if err := s.DeleteMember(ctx, "t1", "u1"); err != nil {
		t.Fatal(err)
	}
	if _, err := s.GetMember(ctx, "t1", "u1"); !errors.Is(err, ErrNotFound) {
		t.Fatalf("expected gone, got %v", err)
	}
}
