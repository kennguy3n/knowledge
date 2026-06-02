// Package export provides portable concept profile rendering,
// summary views, evidence packs, and audit trail integration.
package export

import (
	"context"
	"fmt"
	"html"
	"time"

	"github.com/google/uuid"
	"go.uber.org/zap"

	"github.com/kennguy3n/knowledge/server/internal/substrate"
)

// ProfileRequest is the payload for generating an export profile.
type ProfileRequest struct {
	ScopeID  string `json:"scope_id"`
	TenantID string `json:"tenant_id"`
	Format   string `json:"format"` // "html", "markdown", "json"
}

// ProfileResponse contains the rendered profile.
type ProfileResponse struct {
	ID        string `json:"id"`
	ScopeID   string `json:"scope_id"`
	Format    string `json:"format"`
	Content   string `json:"content"`
	CreatedAt string `json:"created_at"`
}

// Service manages export operations.
type Service struct {
	substrate *substrate.Client
	logger    *zap.Logger
	auditFn   func(ctx context.Context, event AuditEntry) // callback to audit service
}

// AuditEntry is emitted on every export for the audit trail.
type AuditEntry struct {
	Action   string `json:"action"`
	ActorID  string `json:"actor_id"`
	ScopeID  string `json:"scope_id"`
	TenantID string `json:"tenant_id"`
	ExportID string `json:"export_id"`
}

// NewService creates an export service.
func NewService(sub *substrate.Client, logger *zap.Logger, auditFn func(context.Context, AuditEntry)) *Service {
	return &Service{
		substrate: sub,
		logger:    logger,
		auditFn:   auditFn,
	}
}

// GenerateProfile renders a portable concept profile for a scope.
func (s *Service) GenerateProfile(ctx context.Context, req *ProfileRequest, actorID string) (*ProfileResponse, error) {
	if req.ScopeID == "" {
		return nil, fmt.Errorf("scope_id is required")
	}

	format := req.Format
	if format == "" {
		format = "markdown"
	}

	// Fetch channel memory (synthesis recap) from substrate.
	channelMem, err := s.substrate.GetChannelMemory(ctx, req.ScopeID)
	if err != nil {
		s.logger.Warn("failed to fetch channel memory for export", zap.Error(err))
	}

	// Fetch recent evidence for the scope.
	queryResp, err := s.substrate.Query(ctx, &substrate.QueryRequest{
		ScopeID:   req.ScopeID,
		QueryText: "*",
		Limit:     50,
	})
	if err != nil {
		s.logger.Warn("failed to query evidence for export", zap.Error(err))
	}

	// Render the profile.
	content := renderProfile(format, channelMem, queryResp)

	exportID := uuid.New().String()
	resp := &ProfileResponse{
		ID:        exportID,
		ScopeID:   req.ScopeID,
		Format:    format,
		Content:   content,
		CreatedAt: time.Now().UTC().Format(time.RFC3339),
	}

	// Emit audit entry.
	if s.auditFn != nil {
		s.auditFn(ctx, AuditEntry{
			Action:   "export.profile",
			ActorID:  actorID,
			ScopeID:  req.ScopeID,
			TenantID: req.TenantID,
			ExportID: exportID,
		})
	}

	s.logger.Info("profile exported",
		zap.String("export_id", exportID),
		zap.String("scope_id", req.ScopeID),
		zap.String("format", format),
	)
	return resp, nil
}

func renderProfile(format string, channelMem *substrate.MemoryRecord, queryResp *substrate.QueryResponse) string {
	switch format {
	case "html":
		return renderHTML(channelMem, queryResp)
	case "json":
		return renderJSON(channelMem, queryResp)
	default:
		return renderMarkdown(channelMem, queryResp)
	}
}

func renderMarkdown(channelMem *substrate.MemoryRecord, queryResp *substrate.QueryResponse) string {
	md := "# Knowledge Profile\n\n"

	if channelMem != nil {
		md += "## Synthesis Summary\n\n"
		md += channelMem.Summary + "\n\n"
	}

	if queryResp != nil && len(queryResp.Results) > 0 {
		md += "## Evidence\n\n"
		for i, r := range queryResp.Results {
			md += fmt.Sprintf("%d. %s (score: %.2f)\n", i+1, r.Snippet, r.Score)
		}
	}

	return md
}

func renderHTML(channelMem *substrate.MemoryRecord, queryResp *substrate.QueryResponse) string {
	out := "<html><body><h1>Knowledge Profile</h1>"

	if channelMem != nil {
		out += "<h2>Synthesis Summary</h2><p>" + html.EscapeString(channelMem.Summary) + "</p>"
	}

	if queryResp != nil && len(queryResp.Results) > 0 {
		out += "<h2>Evidence</h2><ol>"
		for _, r := range queryResp.Results {
			out += fmt.Sprintf("<li>%s (score: %.2f)</li>", html.EscapeString(r.Snippet), r.Score)
		}
		out += "</ol>"
	}

	out += "</body></html>"
	return out
}

func renderJSON(channelMem *substrate.MemoryRecord, queryResp *substrate.QueryResponse) string {
	type profile struct {
		Summary  *string              `json:"summary,omitempty"`
		Evidence []substrate.QueryResult `json:"evidence,omitempty"`
	}
	p := profile{}
	if channelMem != nil {
		p.Summary = &channelMem.Summary
	}
	if queryResp != nil {
		p.Evidence = queryResp.Results
	}
	// Use a simple manual encoding to avoid import cycle concerns.
	data := `{"summary":`
	if p.Summary != nil {
		data += fmt.Sprintf("%q", *p.Summary)
	} else {
		data += "null"
	}
	data += fmt.Sprintf(`,"evidence_count":%d}`, len(p.Evidence))
	return data
}
