package connector

import (
	"context"
	"encoding/json"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/go-chi/chi/v5"
	"go.uber.org/zap"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
	"github.com/kennguy3n/knowledge/server/internal/metrics"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
	"github.com/kennguy3n/knowledge/server/internal/validate"
)

// defaultMaxWebhookConcurrency bounds how many inbound webhooks may be
// processed concurrently when [Options.MaxWebhookConcurrency] is unset.
// Each inbound webhook kicks off a sync+pipeline run on a background
// goroutine; without a ceiling a burst of provider callbacks could spawn
// unbounded goroutines and exhaust memory/connections.
const defaultMaxWebhookConcurrency = 32

// Service drives connector lifecycle, OAuth, webhooks, scheduling and
// the content pipeline.
type Service struct {
	sub   substrateAPI
	log   *zap.Logger
	store *store
	// regStore durably persists registrations so the orchestration state
	// (ingest scope, webhook, schedule) survives a gateway restart. The
	// in-memory store is the hot read cache; regStore is the source of
	// truth rehydrated on boot.
	regStore      RegistrationStore
	sched         *Scheduler
	publicBaseURL string
	syncInterval  time.Duration
	// webhookSem is a counting semaphore (buffered channel) that caps the
	// number of concurrent webhook-triggered background syncs. A full
	// channel sheds load with HTTP 429 rather than spawning more work.
	webhookSem chan struct{}
	// webhookWG tracks in-flight webhook syncs so [Service.Stop] can drain
	// them for a graceful shutdown.
	webhookWG sync.WaitGroup
}

// Options configures a connector [Service].
type Options struct {
	// PublicBaseURL is the externally reachable base URL used to build
	// OAuth redirect and webhook callback URLs (no trailing slash).
	PublicBaseURL string
	// SyncInterval is the default per-connector sync cadence.
	SyncInterval time.Duration
	// MaxWebhookConcurrency caps concurrent webhook-triggered background
	// syncs. Values <= 0 fall back to [defaultMaxWebhookConcurrency].
	MaxWebhookConcurrency int
	// RegistrationStore durably persists connector registrations. A nil
	// value falls back to [NewNoopRegistrationStore] (process-lifetime
	// only), matching the gateway's other dev-mode stores.
	RegistrationStore RegistrationStore
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
	maxWebhook := opts.MaxWebhookConcurrency
	if maxWebhook <= 0 {
		maxWebhook = defaultMaxWebhookConcurrency
	}
	regStore := opts.RegistrationStore
	if regStore == nil {
		regStore = NewNoopRegistrationStore()
	}
	s := &Service{
		sub:           sub,
		log:           log,
		store:         newStore(),
		regStore:      regStore,
		publicBaseURL: strings.TrimRight(opts.PublicBaseURL, "/"),
		syncInterval:  opts.SyncInterval,
		webhookSem:    make(chan struct{}, maxWebhook),
	}
	s.sched = NewScheduler(s.syncAndProcess)
	return s
}

// Start binds the sync scheduler to ctx; scheduled jobs stop when ctx
// is cancelled.
func (s *Service) Start(ctx context.Context) { s.sched.Start(ctx) }

// Stop tears down the scheduler and drains any in-flight
// webhook-triggered syncs so they are not abandoned mid-shutdown.
func (s *Service) Stop() {
	s.sched.Stop()
	s.webhookWG.Wait()
}

// saveRegistration durably persists a registration and, only on
// success, updates the in-memory read cache. Caching after the durable
// write keeps the cache from diverging from the source of truth.
func (s *Service) saveRegistration(ctx context.Context, reg registration) error {
	if err := s.regStore.Save(ctx, reg); err != nil {
		return err
	}
	s.store.put(reg)
	return nil
}

// deleteRegistration removes a registration from durable storage and
// the in-memory cache. The cache entry is dropped regardless of the
// durable delete outcome so a removed connector stops being served from
// memory immediately.
func (s *Service) deleteRegistration(ctx context.Context, instanceID string) error {
	s.store.delete(instanceID)
	return s.regStore.Delete(ctx, instanceID)
}

// Rehydrate restores connector orchestration state after a restart. It
// loads persisted registrations, reconciles them against the
// substrate's authoritative connector list (pruning any whose connector
// no longer exists), repopulates the in-memory cache, and reschedules
// periodic syncs. Call it once before [Service.Start].
//
// If the substrate connector list is unavailable, rehydration proceeds
// from the persisted registrations alone without pruning; stale entries
// are reconciled on a later restart rather than dropping every schedule
// when the loopback is briefly down.
func (s *Service) Rehydrate(ctx context.Context) error {
	regs, err := s.regStore.List(ctx)
	if err != nil {
		return err
	}
	live, liveErr := s.liveConnectorIDs(ctx)
	switch {
	case liveErr != nil:
		s.log.Warn("connector rehydrate: substrate list unavailable; skipping reconciliation",
			zap.Error(liveErr))
		live = nil
	case len(regs) > 0 && len(live) == 0:
		// A successful but empty list while we hold persisted
		// registrations is more likely a not-yet-ready substrate (or a
		// transient wipe) than a genuine "every connector was deleted".
		// Pruning here would drop every schedule on a false signal, so
		// skip reconciliation and let a later boot reconcile once the
		// substrate reports a non-empty authoritative list.
		s.log.Warn("connector rehydrate: substrate reported zero connectors but registrations exist; skipping prune",
			zap.Int("registrations", len(regs)))
		live = nil
	}
	var restored, pruned int
	for _, reg := range regs {
		if live != nil {
			if _, ok := live[reg.InstanceID]; !ok {
				if derr := s.regStore.Delete(ctx, reg.InstanceID); derr != nil {
					s.log.Warn("connector rehydrate: prune stale registration",
						zap.String("instance_id", reg.InstanceID), zap.Error(derr))
				}
				pruned++
				continue
			}
		}
		interval := reg.SyncInterval
		if interval <= 0 {
			interval = s.syncInterval
		}
		s.store.put(reg)
		s.sched.Schedule(reg.InstanceID, interval)
		restored++
	}
	s.log.Info("connector registrations rehydrated",
		zap.Int("restored", restored), zap.Int("pruned", pruned))
	return nil
}

// liveConnectorIDs returns the set of connector instance ids the
// substrate currently knows about. Only the id field of each
// [ffi::ConnectorStatus] row (camelCase `instanceId`) is decoded.
func (s *Service) liveConnectorIDs(ctx context.Context) (map[string]struct{}, error) {
	raw, err := s.sub.ListConnectors(ctx)
	if err != nil {
		return nil, err
	}
	var rows []struct {
		InstanceID string `json:"instanceId"`
	}
	if err := json.Unmarshal(raw, &rows); err != nil {
		return nil, httpx.Internal("connector: decode connector list")
	}
	live := make(map[string]struct{}, len(rows))
	for _, row := range rows {
		if row.InstanceID != "" {
			live[row.InstanceID] = struct{}{}
		}
	}
	return live, nil
}

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
	// Persist before caching/scheduling: a registration the gateway
	// cannot durably record must not be silently accepted, or it would
	// vanish on restart while the substrate connector lingers. If
	// persistence fails, roll the substrate connector back so a transient
	// DB blip does not leave an orphaned, unschedulable connector behind.
	if err := s.saveRegistration(r.Context(), reg); err != nil {
		s.log.Error("connector: persist registration; rolling back substrate connector",
			zap.String("instance_id", reg.InstanceID), zap.Error(err))
		if rerr := s.sub.RemoveConnector(r.Context(), id.ID); rerr != nil {
			s.log.Error("connector: rollback substrate connector after persist failure",
				zap.String("instance_id", reg.InstanceID), zap.Error(rerr))
		}
		httpx.WriteError(w, httpx.Internal("connector: persist registration"))
		return
	}
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
	if err := s.deleteRegistration(r.Context(), id); err != nil {
		// The substrate connector is already gone; a stale persisted
		// registration would be pruned on the next rehydrate, so surface
		// the error but do not fail the delete.
		s.log.Warn("connector: delete persisted registration",
			zap.String("instance_id", id), zap.Error(err))
	}
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
	// Look up registration first so reg.Kind is available for failure
	// metrics on every error path (SyncConnector, Unmarshal, pipeline).
	reg, ok := s.store.get(instanceID)
	if !ok {
		return syncReport{}, PipelineResult{}, httpx.Internal("connector: no registration for instance; cannot resolve ingest scope")
	}
	raw, err := s.sub.SyncConnector(ctx, instanceID)
	if err != nil {
		metrics.ConnectorSyncFailure.WithLabelValues(reg.Kind).Inc()
		return syncReport{}, PipelineResult{}, err
	}
	var report syncReport
	if err := json.Unmarshal(raw, &report); err != nil {
		metrics.ConnectorSyncFailure.WithLabelValues(reg.Kind).Inc()
		return syncReport{}, PipelineResult{}, httpx.Internal("connector: decode sync report")
	}
	result, err := s.runPipeline(ctx, instanceID, reg.ScopeID, reg.Kind, report.IngestedEvidenceIDs)
	if err != nil {
		metrics.ConnectorSyncFailure.WithLabelValues(reg.Kind).Inc()
		return report, PipelineResult{}, err
	}
	metrics.ConnectorSyncSuccess.WithLabelValues(reg.Kind).Inc()
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
