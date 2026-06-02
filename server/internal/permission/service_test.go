package permission

import (
	"context"
	"testing"

	"go.uber.org/zap"
)

func newTestService() *Service {
	return NewService(zap.NewNop())
}

func TestGrant_And_Check(t *testing.T) {
	svc := newTestService()
	ctx := context.Background()

	tuple := &Tuple{User: "user-1", Relation: "viewer", Object: "doc:123"}
	if err := svc.Grant(ctx, tuple); err != nil {
		t.Fatalf("grant failed: %v", err)
	}

	resp := svc.Check(ctx, &CheckRequest{User: "user-1", Relation: "viewer", Object: "doc:123"})
	if !resp.Allowed {
		t.Error("expected allowed after grant")
	}
}

func TestCheck_Denied(t *testing.T) {
	svc := newTestService()
	ctx := context.Background()

	resp := svc.Check(ctx, &CheckRequest{User: "user-1", Relation: "viewer", Object: "doc:123"})
	if resp.Allowed {
		t.Error("expected denied without grant")
	}
}

func TestRevoke(t *testing.T) {
	svc := newTestService()
	ctx := context.Background()

	tuple := &Tuple{User: "user-1", Relation: "editor", Object: "doc:456"}
	_ = svc.Grant(ctx, tuple)
	_ = svc.Revoke(ctx, tuple)

	resp := svc.Check(ctx, &CheckRequest{User: "user-1", Relation: "editor", Object: "doc:456"})
	if resp.Allowed {
		t.Error("expected denied after revoke")
	}
}

func TestGrant_MultipleRelations(t *testing.T) {
	svc := newTestService()
	ctx := context.Background()

	_ = svc.Grant(ctx, &Tuple{User: "u1", Relation: "viewer", Object: "doc:1"})
	_ = svc.Grant(ctx, &Tuple{User: "u1", Relation: "editor", Object: "doc:1"})

	if !svc.Check(ctx, &CheckRequest{User: "u1", Relation: "viewer", Object: "doc:1"}).Allowed {
		t.Error("viewer should be allowed")
	}
	if !svc.Check(ctx, &CheckRequest{User: "u1", Relation: "editor", Object: "doc:1"}).Allowed {
		t.Error("editor should be allowed")
	}
	if svc.Check(ctx, &CheckRequest{User: "u1", Relation: "owner", Object: "doc:1"}).Allowed {
		t.Error("owner should be denied")
	}
}

func TestSCIM_CreateAndGetUser(t *testing.T) {
	svc := newTestService()
	ctx := context.Background()

	created, err := svc.CreateUser(ctx, &SCIMUser{
		UserName:    "jdoe",
		DisplayName: "John Doe",
		Active:      true,
	})
	if err != nil {
		t.Fatalf("create user failed: %v", err)
	}
	if created.ID == "" {
		t.Error("expected non-empty user ID")
	}

	got, err := svc.GetUser(ctx, created.ID)
	if err != nil {
		t.Fatalf("get user failed: %v", err)
	}
	if got.UserName != "jdoe" {
		t.Errorf("username = %q, want %q", got.UserName, "jdoe")
	}
}

func TestSCIM_GetUser_NotFound(t *testing.T) {
	svc := newTestService()
	_, err := svc.GetUser(context.Background(), "nonexistent")
	if err == nil {
		t.Fatal("expected error for nonexistent user")
	}
}

func TestSCIM_ListUsers(t *testing.T) {
	svc := newTestService()
	ctx := context.Background()

	_, _ = svc.CreateUser(ctx, &SCIMUser{UserName: "a"})
	_, _ = svc.CreateUser(ctx, &SCIMUser{UserName: "b"})

	users := svc.ListUsers(ctx)
	if len(users) != 2 {
		t.Errorf("user count = %d, want 2", len(users))
	}
}

func TestSCIM_CreateAndGetGroup(t *testing.T) {
	svc := newTestService()
	ctx := context.Background()

	created, err := svc.CreateGroup(ctx, &SCIMGroup{
		DisplayName: "Admins",
		Members:     []string{"user-1", "user-2"},
	})
	if err != nil {
		t.Fatalf("create group failed: %v", err)
	}
	if created.ID == "" {
		t.Error("expected non-empty group ID")
	}

	got, err := svc.GetGroup(ctx, created.ID)
	if err != nil {
		t.Fatalf("get group failed: %v", err)
	}
	if got.DisplayName != "Admins" {
		t.Errorf("display name = %q, want %q", got.DisplayName, "Admins")
	}
	if len(got.Members) != 2 {
		t.Errorf("member count = %d, want 2", len(got.Members))
	}
}

func TestSCIM_GetGroup_NotFound(t *testing.T) {
	svc := newTestService()
	_, err := svc.GetGroup(context.Background(), "nonexistent")
	if err == nil {
		t.Fatal("expected error for nonexistent group")
	}
}

func TestSCIM_ListGroups(t *testing.T) {
	svc := newTestService()
	ctx := context.Background()

	_, _ = svc.CreateGroup(ctx, &SCIMGroup{DisplayName: "A"})
	_, _ = svc.CreateGroup(ctx, &SCIMGroup{DisplayName: "B"})

	groups := svc.ListGroups(ctx)
	if len(groups) != 2 {
		t.Errorf("group count = %d, want 2", len(groups))
	}
}
