package connector

import (
	"context"
	"encoding/json"
	"net/http"
	"strings"
	"time"

	"github.com/go-chi/chi/v5"
	"go.uber.org/zap"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
	"github.com/kennguy3n/knowledge/server/internal/validate"
)

// Service drives connector lifecycle, OAuth, webhooks, scheduling and
// the content pipeline.
type Service struct {
	sub           substrateAPI
	log           *zap.Logger
	store         *store
	sched         *Scheduler
	publicBaseURL string
	syncInterval  time.Duration
}

// Options configures a connector [Service].
type Options struct {
	// PublicBaseURL is the externally reachable base URL used to build
	// OAuth redirect and webhook callback URLs (no trailing slash).
	PublicBaseURL string
	// SyncInterval is the default per-connector sync cadence.
	SyncInterval time.Duration
}

// New constructs a connector Service. The scheduler is created but not
// started; call [Service.Start] to bind its lifecycle to a context.
func New(sub substrateAPI, log *zap.Logger, opts Options) *Service {
	if log == nil {
		log = zap.NewNop()
	}
	if opts.SyncInterval <= 0 {
		opts.SyncInterval = 15 * time.Minute
	}
	s := &Service{
		sub:           sub,
		log:           log,
		store:         newStore(),
		publicBaseURL: strings.TrimRight(opts.PublicBaseURL, "/"),
		syncInterval:  opts.SyncInterval,
	}
	s.sched = NewScheduler(s.syncAndProcess)
	return s
}

// Start binds the sync scheduler to ctx; scheduled jobs stop when ctx
// is cancelled.
func (s *Service) Start(ctx context.Context) { s.sched.Start(ctx) }

// Stop tears down the scheduler.
func (s *Service) Stop() { s.sched.Stop() }

// Routes returns the connector chi router.
func (s *Service) Routes() http.Handler {
	r := chi.NewRouter()
	r.Post("/", s.handleCreate)
	r.Get("/", s.handleList)
	r.Get("/oauth/callback", s.handleOAuthCallback)
	r.Route("/{id}", func(r chi.Router) {
		r.Post("/authenticate", s.handleAuthenticate)
		r.Post("/sync", s.handleSync)
		r.Delete("/", s.handleRemove)
		r.Get("/status", s.handleStatus)
		r.Get("/oauth/start", s.handleOAuthStart)
		r.Post("/webhook/register", s.handleWebhookRegister)
		r.Post("/webhook", s.handleWebhookReceive)
	})
	return r
}

// CreateRequest is the body of POST /connectors.
type CreateRequest struct {
	Kind       string `json:"kind"`
	ScopeID    string `json:"scope_id"`
	ConfigJSON string `json:"config_json"`
}

func (s *Service) handleCreate(w http.ResponseWriter, r *http.Request) {
	var req CreateRequest
	if err := httpx.DecodeJSON(r, &req); err != nil {
		httpx.WriteError(w, err)
		return
	}
	scope, err := validate.ScopeID(req.ScopeID)
	if err != nil {
		httpx.WriteError(w, httpx.BadRequest("scope_id must be a UUID"))
		return
	}
	if req.Kind == "" {
		httpx.WriteError(w, httpx.BadRequest("kind is required"))
		return
	}
	cfg := req.ConfigJSON
	if cfg == "" {
		cfg = "{}"
	}
	id, err := s.sub.CreateConnector(r.Context(), substrate.CreateConnectorRequest{
		Kind:       req.Kind,
		ScopeID:    scope,
		ConfigJSON: cfg,
	})
	if err != nil {
		httpx.WriteError(w, err)
		return
	}
	reg := registration{
		InstanceID:   id.ID,
		Kind:         req.Kind,
		ScopeID:      scope,
		SyncInterval: s.syncInterval,
		CreatedAt:    time.Now().UTC(),
	}
	s.store.put(reg)
	s.sched.Schedule(id.ID, s.syncInterval)
	httpx.WriteJSON(w, http.StatusCreated, reg)
}

func (s *Service) handleList(w http.ResponseWriter, r *http.Request) {
	raw, err := s.sub.ListConnectors(r.Context())
	if err != nil {
		httpx.WriteError(w, err)
		return
	}
	writeRaw(w, http.StatusOK, raw)
}

func (s *Service) handleAuthenticate(w http.ResponseWriter, r *http.Request) {
	var req substrate.AuthenticateRequest
	if err := httpx.DecodeJSON(r, &req); err != nil {
		httpx.WriteError(w, err)
		return
	}
	raw, err := s.sub.AuthenticateConnector(r.Context(), chi.URLParam(r, "id"), req)
	if err != nil {
		httpx.WriteError(w, err)
		return
	}
	writeRaw(w, http.StatusOK, raw)
}

func (s *Service) handleSync(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "id")
	report, result, err := s.syncOnce(r.Context(), id)
	if err != nil {
		httpx.WriteError(w, err)
		return
	}
	httpx.WriteJSON(w, http.StatusOK, map[string]any{
		"sync":     report,
		"pipeline": result,
	})
}

func (s *Service) handleRemove(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "id")
	if err := s.sub.RemoveConnector(r.Context(), id); err != nil {
		httpx.WriteError(w, err)
		return
	}
	s.sched.Unschedule(id)
	s.store.delete(id)
	w.WriteHeader(http.StatusNoContent)
}

func (s *Service) handleStatus(w http.ResponseWriter, r *http.Request) {
	raw, err := s.sub.ConnectorStatus(r.Context(), chi.URLParam(r, "id"))
	if err != nil {
		httpx.WriteError(w, err)
		return
	}
	writeRaw(w, http.StatusOK, raw)
}

// syncReport mirrors the camelCase JSON of `ffi::SyncReport`.
type syncReport struct {
	InstanceID          string   `json:"instanceId"`
	Mode                string   `json:"mode"`
	EventsTotal         int      `json:"eventsTotal"`
	EventsIngested      int      `json:"eventsIngested"`
	IngestedEvidenceIDs []string `json:"ingestedEvidenceIds"`
	NextCursor          *string  `json:"nextCursor"`
}

// syncOnce runs a single sync and the follow-on content pipeline.
func (s *Service) syncOnce(ctx context.Context, instanceID string) (syncReport, PipelineResult, error) {
	raw, err := s.sub.SyncConnector(ctx, instanceID)
	if err != nil {
		return syncReport{}, PipelineResult{}, err
	}
	var report syncReport
	if err := json.Unmarshal(raw, &report); err != nil {
		return syncReport{}, PipelineResult{}, httpx.Internal("connector: decode sync report")
	}
	reg, ok := s.store.get(instanceID)
	scope := report.InstanceID
	kind := ""
	if ok {
		scope = reg.ScopeID
		kind = reg.Kind
	}
	result, err := s.runPipeline(ctx, instanceID, scope, kind, report.IngestedEvidenceIDs)
	if err != nil {
		return report, PipelineResult{}, err
	}
	return report, result, nil
}

// syncAndProcess is the scheduler callback; it logs but never panics.
func (s *Service) syncAndProcess(ctx context.Context, instanceID string) {
	if _, _, err := s.syncOnce(ctx, instanceID); err != nil {
		s.log.Warn("scheduled sync failed",
			zap.String("instance_id", instanceID), zap.Error(err))
	}
}

// writeRaw forwards a pre-serialised JSON document to the client.
func writeRaw(w http.ResponseWriter, status int, raw json.RawMessage) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(status)
	if len(raw) == 0 {
		_, _ = w.Write([]byte("null"))
		return
	}
	_, _ = w.Write(raw)
}
