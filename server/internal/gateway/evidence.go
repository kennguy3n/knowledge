package gateway

import (
	"encoding/json"
	"net/http"
	"strconv"

	"github.com/go-chi/chi/v5"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
	"github.com/kennguy3n/knowledge/server/internal/metrics"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
	"github.com/kennguy3n/knowledge/server/internal/validate"
)

// ingestRequest is the public body of POST /api/v1/ingest.
type ingestRequest struct {
	ScopeID    string `json:"scope_id"`
	Body       string `json:"body"`
	Source     string `json:"source"`
	Importance string `json:"importance"`
}

func (h *handlers) ingest(w http.ResponseWriter, r *http.Request) {
	var req ingestRequest
	if err := httpx.DecodeJSON(r, &req); err != nil {
		httpx.WriteError(w, err)
		return
	}
	scope, err := validate.ScopeID(req.ScopeID)
	if err != nil {
		httpx.WriteError(w, httpx.BadRequest("scope_id must be a UUID"))
		return
	}
	if err := validate.NonEmptyUTF8(req.Body); err != nil {
		httpx.WriteError(w, httpx.BadRequest("body must be non-empty valid UTF-8"))
		return
	}
	source := req.Source
	if source == "" {
		source = "Manual"
	}
	importance := req.Importance
	if importance == "" {
		importance = "Useful"
	}
	id, err := h.sub.Ingest(r.Context(), substrate.IngestRequest{
		ScopeID:    scope,
		Body:       req.Body,
		Source:     source,
		Importance: importance,
	})
	if err != nil {
		metrics.ErrorsTotal.WithLabelValues("ingest").Inc()
		httpx.WriteError(w, err)
		return
	}
	metrics.IngestTotal.Inc()
	httpx.WriteJSON(w, http.StatusCreated, id)
}

// queryRequest is the public body of POST /api/v1/query.
type queryRequest struct {
	ScopeID   string `json:"scope_id"`
	QueryText string `json:"query_text"`
	Limit     uint32 `json:"limit"`
}

func (h *handlers) query(w http.ResponseWriter, r *http.Request) {
	var req queryRequest
	if err := httpx.DecodeJSON(r, &req); err != nil {
		httpx.WriteError(w, err)
		return
	}
	scope, err := validate.ScopeID(req.ScopeID)
	if err != nil {
		httpx.WriteError(w, httpx.BadRequest("scope_id must be a UUID"))
		return
	}
	if err := validate.NonEmptyUTF8(req.QueryText); err != nil {
		httpx.WriteError(w, httpx.BadRequest("query_text must be non-empty valid UTF-8"))
		return
	}
	limit := req.Limit
	if limit == 0 {
		limit = 20
	}
	raw, err := h.sub.Query(r.Context(), substrate.QueryRequest{
		ScopeID:   scope,
		QueryText: req.QueryText,
		Limit:     limit,
	})
	if err != nil {
		metrics.ErrorsTotal.WithLabelValues("query").Inc()
		httpx.WriteError(w, err)
		return
	}
	metrics.QueryTotal.Inc()
	writeRaw(w, http.StatusOK, raw)
}

func (h *handlers) getEvidence(w http.ResponseWriter, r *http.Request) {
	id, err := validate.ScopeID(chi.URLParam(r, "id"))
	if err != nil {
		httpx.WriteError(w, httpx.BadRequest("evidence id must be a UUID"))
		return
	}
	raw, err := h.sub.GetEvidence(r.Context(), id)
	if err != nil {
		httpx.WriteError(w, err)
		return
	}
	writeRaw(w, http.StatusOK, raw)
}

func (h *handlers) listMemories(w http.ResponseWriter, r *http.Request) {
	q := r.URL.Query()
	scope, err := validate.ScopeID(q.Get("scope_id"))
	if err != nil {
		httpx.WriteError(w, httpx.BadRequest("scope_id must be a UUID"))
		return
	}
	filter := substrate.MemoryFilter{}
	if state := q.Get("filter"); state != "" {
		if state == "pinned" {
			filter.PinnedOnly = true
		} else {
			s := state
			filter.State = &s
		}
	}
	raw, err := h.sub.ListMemories(r.Context(), substrate.ListMemoriesRequest{
		ScopeID: scope,
		Filter:  filter,
	})
	if err != nil {
		httpx.WriteError(w, err)
		return
	}
	if limit := q.Get("limit"); limit != "" {
		if n, perr := strconv.Atoi(limit); perr == nil && n >= 0 {
			raw = trimArray(raw, n)
		}
	}
	writeRaw(w, http.StatusOK, raw)
}

// createMemoryRequest is the public body of POST /api/v1/memories.
type createMemoryRequest struct {
	ScopeID         string `json:"scope_id"`
	ObservationType string `json:"observation_type"`
	Content         string `json:"content"`
	Sensitivity     string `json:"sensitivity"`
}

// validMemorySensitivity is the closed set of FFI importance-class tags
// the substrate accepts for a user-memory write. An empty string is
// allowed and lets the substrate apply its default ("Useful").
var validMemorySensitivity = map[string]struct{}{
	"Critical":  {},
	"Important": {},
	"Useful":    {},
	"Noise":     {},
}

// createMemory handles POST /api/v1/memories: it writes a new user
// memory observation for a scope and returns the created record. This
// is the write counterpart to listMemories (GET /api/v1/memories);
// channel/domain/tenant memory is synthesised, not written here.
//
// Validation is fail-closed and mirrors ingest: scope_id must be a
// UUID, observation_type and content must be non-empty valid UTF-8,
// and sensitivity (when present) must be a known importance class.
func (h *handlers) createMemory(w http.ResponseWriter, r *http.Request) {
	var req createMemoryRequest
	if err := httpx.DecodeJSON(r, &req); err != nil {
		httpx.WriteError(w, err)
		return
	}
	scope, err := validate.ScopeID(req.ScopeID)
	if err != nil {
		httpx.WriteError(w, httpx.BadRequest("scope_id must be a UUID"))
		return
	}
	if err := validate.NonEmptyUTF8(req.ObservationType); err != nil {
		httpx.WriteError(w, httpx.BadRequest("observation_type must be non-empty valid UTF-8"))
		return
	}
	if err := validate.NonEmptyUTF8(req.Content); err != nil {
		httpx.WriteError(w, httpx.BadRequest("content must be non-empty valid UTF-8"))
		return
	}
	if req.Sensitivity != "" {
		if _, ok := validMemorySensitivity[req.Sensitivity]; !ok {
			httpx.WriteError(w, httpx.BadRequest("sensitivity must be one of Critical, Important, Useful, Noise"))
			return
		}
	}
	raw, err := h.sub.CreateMemory(r.Context(), substrate.CreateMemoryRequest{
		ScopeID:         scope,
		ObservationType: req.ObservationType,
		Content:         req.Content,
		Sensitivity:     req.Sensitivity,
	})
	if err != nil {
		metrics.ErrorsTotal.WithLabelValues("create_memory").Inc()
		httpx.WriteError(w, err)
		return
	}
	writeRaw(w, http.StatusCreated, raw)
}

// channelMemory handles GET /api/v1/memories/channel?scope_id=… and
// returns the latest synthesised channel recap for a scope. This is the
// read side of synthesis: POST /api/v1/synthesis/trigger writes the
// recap into the scope's channel memory, and this endpoint reads it
// back. A 404 means synthesis has not yet produced a recap for the
// scope.
func (h *handlers) channelMemory(w http.ResponseWriter, r *http.Request) {
	scope, err := validate.ScopeID(r.URL.Query().Get("scope_id"))
	if err != nil {
		httpx.WriteError(w, httpx.BadRequest("scope_id must be a UUID"))
		return
	}
	raw, err := h.sub.ChannelMemory(r.Context(), scope)
	if err != nil {
		httpx.WriteError(w, err)
		return
	}
	writeRaw(w, http.StatusOK, raw)
}

func (h *handlers) forget(w http.ResponseWriter, r *http.Request) {
	scope, err := validate.ScopeID(chi.URLParam(r, "scope_id"))
	if err != nil {
		httpx.WriteError(w, httpx.BadRequest("scope_id must be a UUID"))
		return
	}
	if err := h.sub.ForgetScope(r.Context(), scope); err != nil {
		httpx.WriteError(w, err)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

// trimArray truncates a JSON array document to at most n elements. A
// non-array document is returned unchanged.
func trimArray(raw json.RawMessage, n int) json.RawMessage {
	var items []json.RawMessage
	if err := json.Unmarshal(raw, &items); err != nil {
		return raw
	}
	if n < len(items) {
		items = items[:n]
	}
	out, err := json.Marshal(items)
	if err != nil {
		return raw
	}
	return out
}
