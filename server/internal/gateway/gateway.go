// Package gateway assembles the public API gateway: the middleware
// chain, all /api/v1 routes, health and Prometheus metrics endpoints,
// and SSE streaming for synthesis status. Handlers either call the
// substrate loopback directly (evidence, synthesis) or delegate to the
// mounted core services (connector, permission, tenant, export, audit).
package gateway

import (
	"context"
	"encoding/json"
	"net/http"

	"github.com/go-chi/chi/v5"
	"go.uber.org/zap"

	"github.com/kennguy3n/knowledge/server/internal/audit"
	"github.com/kennguy3n/knowledge/server/internal/connector"
	"github.com/kennguy3n/knowledge/server/internal/export"
	"github.com/kennguy3n/knowledge/server/internal/middleware"
	"github.com/kennguy3n/knowledge/server/internal/permission"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
	"github.com/kennguy3n/knowledge/server/internal/tenant"
)

// substrateAPI is the subset of [substrate.Client] the gateway's own
// evidence/synthesis/health handlers depend on.
type substrateAPI interface {
	Ingest(ctx context.Context, req substrate.IngestRequest) (substrate.IDResponse, error)
	Query(ctx context.Context, req substrate.QueryRequest) (json.RawMessage, error)
	GetEvidence(ctx context.Context, id string) (json.RawMessage, error)
	ListMemories(ctx context.Context, req substrate.ListMemoriesRequest) (json.RawMessage, error)
	ForgetScope(ctx context.Context, scopeID string) error
	TriggerSynthesis(ctx context.Context, req substrate.SynthesisTriggerRequest) (json.RawMessage, error)
	SynthesisStatus(ctx context.Context, id string) (json.RawMessage, error)
	RecentSyntheses(ctx context.Context, req substrate.RecentSynthesisRequest) (json.RawMessage, error)
	Health(ctx context.Context) (json.RawMessage, error)
}

// handlers carries the dependencies for the gateway's own endpoints.
type handlers struct {
	sub   substrateAPI
	log   *zap.Logger
	ready map[string]bool
}

// Deps is the set of collaborators the gateway router needs.
type Deps struct {
	// Substrate is the loopback client (required).
	Substrate substrateAPI
	// Connectors, Permissions, Tenants, Exports, Audit are the mounted
	// core services. Any may be nil, in which case its routes are
	// omitted.
	Connectors  *connector.Service
	Permissions *permission.Service
	Tenants     *tenant.Service
	Exports     *export.Service
	Audit       *audit.Service
	// Auth and RateLimiter are the request-gating middleware.
	Auth        *middleware.Authenticator
	RateLimiter *middleware.RateLimiter
	// CORSOrigins is the CORS allow-list (empty means "*").
	CORSOrigins []string
	// Log is the structured logger (required).
	Log *zap.Logger
	// Ready reports optional-subsystem readiness for /health (e.g.
	// {"postgres": true, "nats": false}).
	Ready map[string]bool
}

// NewRouter builds the fully-wired gateway HTTP handler.
func NewRouter(d Deps) http.Handler {
	if d.Log == nil {
		d.Log = zap.NewNop()
	}
	h := &handlers{sub: d.Substrate, log: d.Log, ready: d.Ready}
	m := newMetrics()

	r := chi.NewRouter()
	r.Use(middleware.InjectRequestID)
	r.Use(middleware.Recover(d.Log))
	r.Use(m.middleware)
	r.Use(middleware.CORS(d.CORSOrigins))

	// Unauthenticated operational endpoints.
	r.Get("/health", h.health)
	r.Method(http.MethodGet, "/metrics", m.handler())

	r.Route("/api/v1", func(r chi.Router) {
		r.Use(middleware.BodyLimit)
		// Per-IP rate limiting runs *before* auth so unauthenticated
		// traffic (credential stuffing, scanners) is throttled per source
		// IP before it can hammer the auth layer.
		if d.RateLimiter != nil {
			r.Use(d.RateLimiter.PerIPMiddleware)
		}
		if d.Auth != nil {
			r.Use(d.Auth.Middleware)
		}
		// Per-tenant rate limiting runs *after* auth: it keys on the
		// resolved tenant from the request context.
		if d.RateLimiter != nil {
			r.Use(d.RateLimiter.PerTenantMiddleware)
		}

		// Evidence.
		r.Post("/ingest", h.ingest)
		r.Post("/query", h.query)
		r.Get("/evidence/{id}", h.getEvidence)
		r.Get("/memories", h.listMemories)
		r.Post("/forget/{scope_id}", h.forget)

		// Synthesis.
		r.Post("/synthesis/trigger", h.triggerSynthesis)
		r.Get("/synthesis/recent", h.recentSyntheses)
		r.Get("/synthesis/{id}/status", h.synthesisStatus)

		if d.Connectors != nil {
			r.Mount("/connectors", d.Connectors.Routes())
		}
		if d.Tenants != nil {
			r.Mount("/tenants", d.Tenants.Routes())
		}
		if d.Exports != nil {
			r.Mount("/export", d.Exports.Routes())
		}
		if d.Audit != nil {
			r.Mount("/audit", d.Audit.Routes())
		}
		if d.Permissions != nil {
			r.Mount("/permission", d.Permissions.Routes())
			r.Mount("/scim/v2", d.Permissions.SCIMRoutes())
		}
	})

	return r
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
