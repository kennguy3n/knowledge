package tenant

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
	tenant, err := svc.Create(context.Background(), &CreateRequest{
		Name: "Acme Corp",
	})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if tenant.ID == "" {
		t.Error("expected non-empty ID")
	}
	if tenant.Name != "Acme Corp" {
		t.Errorf("name = %q, want %q", tenant.Name, "Acme Corp")
	}
	if tenant.Config.ConnectorLimit != 10 {
		t.Errorf("connector_limit = %d, want 10", tenant.Config.ConnectorLimit)
	}
	if tenant.Config.SynthesisTier != "standard" {
		t.Errorf("synthesis_tier = %q, want %q", tenant.Config.SynthesisTier, "standard")
	}
	if tenant.Config.RetentionDays != 365 {
		t.Errorf("retention_days = %d, want 365", tenant.Config.RetentionDays)
	}
}

func TestCreate_EmptyName(t *testing.T) {
	svc := newTestService()
	_, err := svc.Create(context.Background(), &CreateRequest{Name: ""})
	if err == nil {
		t.Fatal("expected error for empty name")
	}
}

func TestCreate_CustomConfig(t *testing.T) {
	svc := newTestService()
	limit := 5
	tier := "premium"
	days := 730
	tenant, err := svc.Create(context.Background(), &CreateRequest{
		Name:           "Custom Corp",
		ConnectorLimit: &limit,
		SynthesisTier:  &tier,
		RetentionDays:  &days,
	})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if tenant.Config.ConnectorLimit != 5 {
		t.Errorf("connector_limit = %d, want 5", tenant.Config.ConnectorLimit)
	}
	if tenant.Config.SynthesisTier != "premium" {
		t.Errorf("synthesis_tier = %q, want %q", tenant.Config.SynthesisTier, "premium")
	}
	if tenant.Config.RetentionDays != 730 {
		t.Errorf("retention_days = %d, want 730", tenant.Config.RetentionDays)
	}
}

func TestGet(t *testing.T) {
	svc := newTestService()
	created, _ := svc.Create(context.Background(), &CreateRequest{Name: "Test"})

	got, err := svc.Get(context.Background(), created.ID)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if got.Name != "Test" {
		t.Errorf("name = %q, want %q", got.Name, "Test")
	}
}

func TestGet_NotFound(t *testing.T) {
	svc := newTestService()
	_, err := svc.Get(context.Background(), "nonexistent")
	if err == nil {
		t.Fatal("expected error for nonexistent tenant")
	}
}

func TestInviteMember(t *testing.T) {
	svc := newTestService()
	ctx := context.Background()
	tenant, _ := svc.Create(ctx, &CreateRequest{Name: "T"})

	member, err := svc.InviteMember(ctx, tenant.ID, &InviteRequest{
		Email: "user@example.com",
		Role:  "admin",
	})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if member.Status != "invited" {
		t.Errorf("status = %q, want %q", member.Status, "invited")
	}
}

func TestActivateMember(t *testing.T) {
	svc := newTestService()
	ctx := context.Background()
	tenant, _ := svc.Create(ctx, &CreateRequest{Name: "T"})
	member, _ := svc.InviteMember(ctx, tenant.ID, &InviteRequest{Email: "u@e.com", Role: "member"})

	if err := svc.ActivateMember(ctx, tenant.ID, member.ID); err != nil {
		t.Fatalf("unexpected error: %v", err)
	}

	got, _ := svc.Get(ctx, tenant.ID)
	if got.Members[member.ID].Status != "active" {
		t.Errorf("status = %q, want %q", got.Members[member.ID].Status, "active")
	}
}

func TestSuspendMember(t *testing.T) {
	svc := newTestService()
	ctx := context.Background()
	tenant, _ := svc.Create(ctx, &CreateRequest{Name: "T"})
	member, _ := svc.InviteMember(ctx, tenant.ID, &InviteRequest{Email: "u@e.com", Role: "member"})
	_ = svc.ActivateMember(ctx, tenant.ID, member.ID)

	if err := svc.SuspendMember(ctx, tenant.ID, member.ID); err != nil {
		t.Fatalf("unexpected error: %v", err)
	}

	got, _ := svc.Get(ctx, tenant.ID)
	if got.Members[member.ID].Status != "suspended" {
		t.Errorf("status = %q, want %q", got.Members[member.ID].Status, "suspended")
	}
}

func TestRemoveMember(t *testing.T) {
	svc := newTestService()
	ctx := context.Background()
	tenant, _ := svc.Create(ctx, &CreateRequest{Name: "T"})
	member, _ := svc.InviteMember(ctx, tenant.ID, &InviteRequest{Email: "u@e.com", Role: "member"})

	if err := svc.RemoveMember(ctx, tenant.ID, member.ID); err != nil {
		t.Fatalf("unexpected error: %v", err)
	}

	got, _ := svc.Get(ctx, tenant.ID)
	if _, exists := got.Members[member.ID]; exists {
		t.Error("expected member to be removed")
	}
}

func TestMember_TenantNotFound(t *testing.T) {
	svc := newTestService()
	ctx := context.Background()

	_, err := svc.InviteMember(ctx, "nonexistent", &InviteRequest{Email: "u@e.com", Role: "member"})
	if err == nil {
		t.Fatal("expected error")
	}

	err = svc.ActivateMember(ctx, "nonexistent", "m1")
	if err == nil {
		t.Fatal("expected error")
	}

	err = svc.SuspendMember(ctx, "nonexistent", "m1")
	if err == nil {
		t.Fatal("expected error")
	}

	err = svc.RemoveMember(ctx, "nonexistent", "m1")
	if err == nil {
		t.Fatal("expected error")
	}
}
