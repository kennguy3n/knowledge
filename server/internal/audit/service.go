package audit

import (
	"context"
	"net/http"
	"strconv"
	"time"

	"github.com/go-chi/chi/v5"
	"github.com/google/uuid"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
)

// Service records and queries audit events.
type Service struct {
	store Store
}

// New constructs an audit Service over the given store.
func New(store Store) *Service {
	return &Service{store: store}
}

// Routes returns the audit query router (GET /audit).
func (s *Service) Routes() http.Handler {
	r := chi.NewRouter()
	r.Get("/", s.handleQuery)
	return r
}

// Record persists an audit event, assigning an id and timestamp when
// absent. It is safe for concurrent use and idempotent on event id.
func (s *Service) Record(ctx context.Context, e Event) (Event, error) {
	if e.TenantID == "" || e.Action == "" || e.Actor == "" {
		return Event{}, httpx.BadRequest("tenant_id, action and actor are required")
	}
	if e.ID == "" {
		e.ID = uuid.NewString()
	}
	if e.CreatedAt.IsZero() {
		e.CreatedAt = time.Now().UTC()
	}
	if err := s.store.Append(ctx, e); err != nil {
		return Event{}, httpx.Internal("audit: persist failed")
	}
	return e, nil
}

// Query returns events matching the filter.
func (s *Service) Query(ctx context.Context, f Filter) ([]Event, error) {
	evs, err := s.store.Query(ctx, f)
	if err != nil {
		return nil, httpx.Internal("audit: query failed")
	}
	return evs, nil
}

func (s *Service) handleQuery(w http.ResponseWriter, r *http.Request) {
	q := r.URL.Query()
	f := Filter{
		TenantID: q.Get("tenant_id"),
		ScopeID:  q.Get("scope_id"),
		Action:   q.Get("action"),
		Actor:    q.Get("actor"),
	}
	if v := q.Get("from"); v != "" {
		t, err := time.Parse(time.RFC3339, v)
		if err != nil {
			httpx.WriteError(w, httpx.BadRequest("from must be RFC3339"))
			return
		}
		f.From = t
	}
	if v := q.Get("to"); v != "" {
		t, err := time.Parse(time.RFC3339, v)
		if err != nil {
			httpx.WriteError(w, httpx.BadRequest("to must be RFC3339"))
			return
		}
		f.To = t
	}
	if v := q.Get("limit"); v != "" {
		n, err := strconv.Atoi(v)
		if err != nil || n < 0 {
			httpx.WriteError(w, httpx.BadRequest("limit must be a non-negative integer"))
			return
		}
		f.Limit = n
	}
	evs, err := s.Query(r.Context(), f)
	if err != nil {
		httpx.WriteError(w, err)
		return
	}
	if evs == nil {
		evs = []Event{}
	}
	httpx.WriteJSON(w, http.StatusOK, evs)
}
