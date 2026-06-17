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
	"github.com/kennguy3n/knowledge/server/internal/metrics"
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
	CreateMemory(ctx context.Context, req substrate.CreateMemoryRequest) (json.RawMessage, error)
	Pin(ctx context.Context, id string) error
	Unpin(ctx context.Context, id string) error
	ChannelMemory(ctx context.Context, scopeID string) (json.RawMessage, error)
	ConceptGraph(ctx context.Context, scopeID string) (json.RawMessage, error)
	ReasoningContradictions(ctx context.Context, req substrate.ReasoningScopeRequest) (json.RawMessage, error)
	ReasoningDrift(ctx context.Context, req substrate.ReasoningScopeRequest) (json.RawMessage, error)
	ReasoningExplain(ctx context.Context, req substrate.ExplainQueryRequest) (json.RawMessage, error)
	ForgetScope(ctx context.Context, scopeID string) error
	TriggerSynthesis(ctx context.Context, req substrate.SynthesisTriggerRequest) (json.RawMessage, error)
	TriggerDomainSynthesis(ctx context.Context, req substrate.ServerSynthesisRequest) (json.RawMessage, error)
	TriggerTenantSynthesis(ctx context.Context, req substrate.ServerSynthesisRequest) (json.RawMessage, error)
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
	// Quota, when set, enforces per-tenant volume quotas (requests/min,
	// syntheses/day, advisory storage soft cap) post-auth. nil disables
	// quota enforcement (e.g. in unit tests).
	Quota *middleware.QuotaEnforcer
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

	// Wire the authenticated-tenant resolver into the observability
	// metrics package so per-tenant counters use the post-auth
	// identity instead of raw headers (bounded cardinality).
	metrics.SetTenantIDFunc(middleware.TenantID)

	r := chi.NewRouter()
	r.Use(middleware.InjectRequestID)
	r.Use(middleware.Recover(d.Log))
	r.Use(m.middleware)
	r.Use(metrics.Middleware)
	r.Use(middleware.CORS(d.CORSOrigins))

	// Unauthenticated operational endpoints.
	r.Get("/health", h.health)
	r.Method(http.MethodGet, "/metrics", m.handler())
	// Observability metrics (knowledge_* prefix).
	r.Method(http.MethodGet, "/metrics/knowledge", metrics.Handler())

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
		// Per-tenant observability + rate limiting run *after* auth: they
		// key on the resolved tenant from the request context.
		r.Use(metrics.TenantMiddleware)
		// Per-tenant SLO latency + error-rate metrics for the ingest/
		// query/synthesis route classes (cardinality-bounded). Sits with
		// TenantMiddleware post-auth so it sees the resolved tenant.
		r.Use(metrics.SLOMiddleware)
		if d.RateLimiter != nil {
			r.Use(d.RateLimiter.PerTenantMiddleware)
		}
		// Per-tenant quotas bound sustained volume (the rate limiter above
		// only sheds short bursts). Runs after auth so it keys on the
		// resolved tenant; over-quota requests get 429 + Retry-After.
		if d.Quota != nil {
			r.Use(d.Quota.Middleware)
		}

		// Evidence.
		r.Post("/ingest", h.ingest)
		r.Post("/query", h.query)
		r.Get("/evidence/{id}", h.getEvidence)
		r.Get("/memories", h.listMemories)
		r.Post("/memories", h.createMemory)
		r.Post("/memories/{id}/pin", h.pinMemory)
		r.Post("/memories/{id}/unpin", h.unpinMemory)
		r.Get("/memories/channel", h.channelMemory)
		r.Get("/memories/concept-graph", h.conceptGraph)
		r.Post("/forget/{scope_id}", h.forget)

		// Reasoning plane — "what changed / what contradicts / why this
		// answer". All three are scope-bound reads forwarded to the
		// substrate's /reasoning/* endpoints.
		r.Post("/reasoning/contradictions", h.reasoningContradictions)
		r.Post("/reasoning/drift", h.reasoningDrift)
		r.Post("/reasoning/explain", h.reasoningExplain)

		// Synthesis. /trigger is the on-device channel tier; /domain and
		// /tenant are the server-side hierarchical tiers (domain rolls up
		// channel outputs, tenant rolls up domain outputs + approved docs).
		r.Post("/synthesis/trigger", h.triggerSynthesis)
		r.Post("/synthesis/domain", h.triggerDomainSynthesis)
		r.Post("/synthesis/tenant", h.triggerTenantSynthesis)
		r.Get("/synthesis/recent", h.recentSyntheses)
		r.Get("/synthesis/{id}/status", h.synthesisStatus)

		if d.Connectors != nil {
			r.Mount("/connectors", d.Connectors.Routes())
		}
		// Control plane authorization: the service principal (trusted
		// backend / static API key) bypasses every gate below; tenant-user
		// JWT principals are constrained. Platform-global operations —
		// tenant lifecycle, SCIM, and authorization-graph mutation — are
		// service-only; per-tenant reads/exports are ReBAC-authorized
		// against the tenant resolved from the request.
		if d.Tenants != nil {
			r.Mount("/tenants", d.Tenants.Routes(tenantAuthz(d.Permissions)))
		}
		if d.Exports != nil {
			guard := controlGuard(d.Permissions, "admin", exportTenantFromBody)
			r.Mount("/export", guard(d.Exports.Routes()))
		}
		if d.Audit != nil {
			guard := controlGuard(d.Permissions, "viewer", auditTenantFromQuery)
			r.Mount("/audit", guard(d.Audit.Routes()))
		}
		if d.Permissions != nil {
			r.Mount("/permission", middleware.RequireService(d.Permissions.Routes()))
			r.Mount("/scim/v2", middleware.RequireService(d.Permissions.SCIMRoutes()))
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
