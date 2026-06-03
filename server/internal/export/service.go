package export

import (
	"context"
	"encoding/json"
	"net/http"

	"github.com/go-chi/chi/v5"

	"github.com/kennguy3n/knowledge/server/internal/audit"
	"github.com/kennguy3n/knowledge/server/internal/httpx"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
	"github.com/kennguy3n/knowledge/server/internal/validate"
)

// exporter is the subset of [substrate.Client] the export service needs.
type exporter interface {
	ExportEvaluate(ctx context.Context, req substrate.ExportEvaluateRequest) (substrate.ExportDecision, error)
}

// AuditRecorder records an audit entry for every export.
type AuditRecorder interface {
	Record(ctx context.Context, e audit.Event) (audit.Event, error)
}

// Service renders policy-enforced concept profile exports.
type Service struct {
	sub   exporter
	audit AuditRecorder
}

// New constructs an export Service.
func New(sub exporter, recorder AuditRecorder) *Service {
	return &Service{sub: sub, audit: recorder}
}

// Routes returns the export router (POST /profile).
func (s *Service) Routes() http.Handler {
	r := chi.NewRouter()
	r.Post("/profile", s.handleProfile)
	return r
}

// ProfileRequest is the body of POST /export/profile.
type ProfileRequest struct {
	ScopeID  string          `json:"scope_id"`
	TenantID string          `json:"tenant_id"`
	Actor    string          `json:"actor"`
	Format   Format          `json:"format"`
	Profile  json.RawMessage `json:"profile"`
	Policy   json.RawMessage `json:"policy,omitempty"`
}

// Result is the structured (JSON-format) export response.
type Result struct {
	Pack     EvidencePack `json:"pack"`
	Markdown string       `json:"markdown,omitempty"`
}

// Export evaluates the profile against the policy and returns a
// policy-enforced evidence pack, recording an audit entry.
func (s *Service) Export(ctx context.Context, req ProfileRequest) (EvidencePack, error) {
	if _, err := validate.ScopeID(req.ScopeID); err != nil {
		return EvidencePack{}, httpx.BadRequest("scope_id must be a UUID")
	}
	if _, err := validate.ScopeID(req.TenantID); err != nil {
		return EvidencePack{}, httpx.BadRequest("tenant_id must be a UUID")
	}
	if len(req.Profile) == 0 {
		return EvidencePack{}, httpx.BadRequest("profile is required")
	}
	actor := req.Actor
	if actor == "" {
		actor = "system"
	}
	decision, err := s.sub.ExportEvaluate(ctx, substrate.ExportEvaluateRequest{
		Policy:  req.Policy,
		Profile: req.Profile,
	})
	if err != nil {
		return EvidencePack{}, err
	}
	pack := buildPack(decision)

	detail, _ := json.Marshal(map[string]any{
		"approved":             len(pack.Approved),
		"rejected":             pack.RejectedCount,
		"raw_evidence_omitted": pack.RawEvidenceOmitted,
	})
	if _, err := s.audit.Record(ctx, audit.Event{
		TenantID: req.TenantID,
		ScopeID:  req.ScopeID,
		Action:   "export.profile",
		Actor:    actor,
		Detail:   detail,
	}); err != nil {
		return EvidencePack{}, err
	}
	return pack, nil
}

func (s *Service) handleProfile(w http.ResponseWriter, r *http.Request) {
	var req ProfileRequest
	if err := httpx.DecodeJSON(r, &req); err != nil {
		httpx.WriteError(w, err)
		return
	}
	pack, err := s.Export(r.Context(), req)
	if err != nil {
		httpx.WriteError(w, err)
		return
	}
	switch req.Format {
	case FormatMarkdown:
		w.Header().Set("Content-Type", "text/markdown; charset=utf-8")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte(renderMarkdown(pack)))
	case FormatHTML:
		w.Header().Set("Content-Type", "text/html; charset=utf-8")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte(renderHTML(pack)))
	default:
		httpx.WriteJSON(w, http.StatusOK, Result{Pack: pack, Markdown: renderMarkdown(pack)})
	}
}
