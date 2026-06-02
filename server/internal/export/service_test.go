package export

import (
	"context"
	"testing"

	"go.uber.org/zap"

	"github.com/kennguy3n/knowledge/server/internal/substrate"
)

func newTestService() *Service {
	sub := substrate.NewClient("http://127.0.0.1:19090", 0)
	var auditEntries []AuditEntry
	auditFn := func(_ context.Context, entry AuditEntry) {
		auditEntries = append(auditEntries, entry)
	}
	return NewService(sub, zap.NewNop(), auditFn)
}

func TestGenerateProfile_RequiresScopeID(t *testing.T) {
	svc := newTestService()
	_, err := svc.GenerateProfile(context.Background(), &ProfileRequest{
		TenantID: "t1",
		Format:   "markdown",
	}, "actor-1")
	if err == nil {
		t.Fatal("expected error for missing scope_id")
	}
}

func TestGenerateProfile_DefaultFormat(t *testing.T) {
	svc := newTestService()
	resp, err := svc.GenerateProfile(context.Background(), &ProfileRequest{
		ScopeID:  "scope-1",
		TenantID: "t1",
	}, "actor-1")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if resp.Format != "markdown" {
		t.Errorf("format = %q, want %q", resp.Format, "markdown")
	}
	if resp.ID == "" {
		t.Error("expected non-empty export ID")
	}
}

func TestGenerateProfile_HTMLFormat(t *testing.T) {
	svc := newTestService()
	resp, err := svc.GenerateProfile(context.Background(), &ProfileRequest{
		ScopeID:  "scope-1",
		TenantID: "t1",
		Format:   "html",
	}, "actor-1")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if resp.Format != "html" {
		t.Errorf("format = %q, want %q", resp.Format, "html")
	}
}

func TestGenerateProfile_JSONFormat(t *testing.T) {
	svc := newTestService()
	resp, err := svc.GenerateProfile(context.Background(), &ProfileRequest{
		ScopeID:  "scope-1",
		TenantID: "t1",
		Format:   "json",
	}, "actor-1")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if resp.Format != "json" {
		t.Errorf("format = %q, want %q", resp.Format, "json")
	}
}

func TestRenderMarkdown_WithData(t *testing.T) {
	mem := &substrate.MemoryRecord{Summary: "Test recap"}
	query := &substrate.QueryResponse{
		Results: []substrate.QueryResult{
			{Snippet: "evidence 1", Score: 0.95},
			{Snippet: "evidence 2", Score: 0.85},
		},
	}
	md := renderMarkdown(mem, query)
	if md == "" {
		t.Error("expected non-empty markdown")
	}
}

func TestRenderMarkdown_NilData(t *testing.T) {
	md := renderMarkdown(nil, nil)
	if md == "" {
		t.Error("expected non-empty markdown even with nil data")
	}
}

func TestRenderHTML(t *testing.T) {
	html := renderHTML(nil, nil)
	if html == "" {
		t.Error("expected non-empty HTML")
	}
}

func TestRenderJSON(t *testing.T) {
	j := renderJSON(nil, nil)
	if j == "" {
		t.Error("expected non-empty JSON")
	}
}

func TestGenerateProfile_EmitsAudit(t *testing.T) {
	sub := substrate.NewClient("http://127.0.0.1:19090", 0)
	var auditEntries []AuditEntry
	auditFn := func(_ context.Context, entry AuditEntry) {
		auditEntries = append(auditEntries, entry)
	}
	svc := NewService(sub, zap.NewNop(), auditFn)

	_, _ = svc.GenerateProfile(context.Background(), &ProfileRequest{
		ScopeID:  "scope-1",
		TenantID: "t1",
	}, "actor-1")

	if len(auditEntries) != 1 {
		t.Fatalf("audit entries = %d, want 1", len(auditEntries))
	}
	if auditEntries[0].Action != "export.profile" {
		t.Errorf("action = %q, want %q", auditEntries[0].Action, "export.profile")
	}
	if auditEntries[0].ActorID != "actor-1" {
		t.Errorf("actor = %q, want %q", auditEntries[0].ActorID, "actor-1")
	}
}
