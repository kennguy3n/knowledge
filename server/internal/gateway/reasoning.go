package gateway

import (
	"net/http"
	"strings"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
	"github.com/kennguy3n/knowledge/server/internal/metrics"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
	"github.com/kennguy3n/knowledge/server/internal/validate"
)

// reasoningScopeRequest is the public body of
// POST /api/v1/reasoning/contradictions and /reasoning/drift.
type reasoningScopeRequest struct {
	ScopeID string `json:"scope_id"`
}

// explainRequest is the public body of POST /api/v1/reasoning/explain.
type explainRequest struct {
	ScopeID string `json:"scope_id"`
	Query   string `json:"query"`
}

// reasoningContradictions handles POST /api/v1/reasoning/contradictions:
// it returns the opposing canonical claims in a scope (the "what
// contradicts" surface). Like conceptGraph the scope_id is validated as
// a UUID (fail-closed 400 otherwise) before the request reaches the
// substrate, and the scan is bound to that single scope. A scope with no
// contradictions — or a forgotten scope — yields an empty array (200),
// never a 404.
func (h *handlers) reasoningContradictions(w http.ResponseWriter, r *http.Request) {
	var req reasoningScopeRequest
	if err := httpx.DecodeJSON(r, &req); err != nil {
		httpx.WriteError(w, err)
		return
	}
	scope, err := validate.ScopeID(req.ScopeID)
	if err != nil {
		httpx.WriteError(w, httpx.BadRequest("scope_id must be a UUID"))
		return
	}
	raw, err := h.sub.ReasoningContradictions(r.Context(), substrate.ReasoningScopeRequest{ScopeID: scope})
	if err != nil {
		metrics.ErrorsTotal.WithLabelValues("reasoning_contradictions").Inc()
		httpx.WriteError(w, err)
		return
	}
	writeRaw(w, http.StatusOK, raw)
}

// reasoningDrift handles POST /api/v1/reasoning/drift: it returns the
// canonical claims whose evidence base has shifted in a scope (the "what
// changed" surface). Same scope-isolation and empty-is-valid semantics
// as reasoningContradictions.
func (h *handlers) reasoningDrift(w http.ResponseWriter, r *http.Request) {
	var req reasoningScopeRequest
	if err := httpx.DecodeJSON(r, &req); err != nil {
		httpx.WriteError(w, err)
		return
	}
	scope, err := validate.ScopeID(req.ScopeID)
	if err != nil {
		httpx.WriteError(w, httpx.BadRequest("scope_id must be a UUID"))
		return
	}
	raw, err := h.sub.ReasoningDrift(r.Context(), substrate.ReasoningScopeRequest{ScopeID: scope})
	if err != nil {
		metrics.ErrorsTotal.WithLabelValues("reasoning_drift").Inc()
		httpx.WriteError(w, err)
		return
	}
	writeRaw(w, http.StatusOK, raw)
}

// reasoningExplain handles POST /api/v1/reasoning/explain: it returns
// the query planner's rationale for a retrieval (the "why this answer"
// surface). The plan is a pure function of the query text — the
// substrate reads no scope data — but the scope_id is still validated so
// the authorisation envelope is uniform across the reasoning routes.
func (h *handlers) reasoningExplain(w http.ResponseWriter, r *http.Request) {
	var req explainRequest
	if err := httpx.DecodeJSON(r, &req); err != nil {
		httpx.WriteError(w, err)
		return
	}
	scope, err := validate.ScopeID(req.ScopeID)
	if err != nil {
		httpx.WriteError(w, httpx.BadRequest("scope_id must be a UUID"))
		return
	}
	if err := validate.NonEmptyUTF8(strings.TrimSpace(req.Query)); err != nil {
		httpx.WriteError(w, httpx.BadRequest("query must be non-empty valid UTF-8"))
		return
	}
	raw, err := h.sub.ReasoningExplain(r.Context(), substrate.ExplainQueryRequest{
		ScopeID: scope,
		Query:   strings.TrimSpace(req.Query),
	})
	if err != nil {
		metrics.ErrorsTotal.WithLabelValues("reasoning_explain").Inc()
		httpx.WriteError(w, err)
		return
	}
	writeRaw(w, http.StatusOK, raw)
}
