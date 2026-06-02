// Package main is the API gateway entry point — chi router on :8080.
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"os/signal"
	"syscall"

	"github.com/go-chi/chi/v5"
	chimw "github.com/go-chi/chi/v5/middleware"
	"github.com/go-chi/cors"
	"github.com/google/uuid"
	"github.com/prometheus/client_golang/prometheus/promhttp"
	"go.uber.org/zap"

	"github.com/kennguy3n/knowledge/server/internal/audit"
	"github.com/kennguy3n/knowledge/server/internal/config"
	"github.com/kennguy3n/knowledge/server/internal/connector"
	"github.com/kennguy3n/knowledge/server/internal/export"
	"github.com/kennguy3n/knowledge/server/internal/middleware"
	"github.com/kennguy3n/knowledge/server/internal/permission"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
	"github.com/kennguy3n/knowledge/server/internal/tenant"
	"github.com/kennguy3n/knowledge/server/pkg/httputil"
	"github.com/kennguy3n/knowledge/server/pkg/logging"
)

func main() {
	logger, err := logging.New()
	if err != nil {
		fmt.Fprintf(os.Stderr, "failed to create logger: %v\n", err)
		os.Exit(1)
	}
	defer func() { _ = logger.Sync() }()

	cfg, err := config.Load()
	if err != nil {
		logger.Fatal("failed to load config", zap.Error(err))
	}

	// Substrate client.
	sub := substrate.NewClient(cfg.SubstrateURL, cfg.SubstrateTimeout)

	// Services.
	auditSvc := audit.NewService(logger.Named("audit"))
	connSvc := connector.NewService(sub, logger.Named("connector"))
	permSvc := permission.NewService(logger.Named("permission"))
	tenantSvc := tenant.NewService(sub, logger.Named("tenant"))

	auditCallback := func(ctx context.Context, entry export.AuditEntry) {
		_ = auditSvc.Record(ctx, &audit.Event{
			TenantID: entry.TenantID,
			ScopeID:  entry.ScopeID,
			Action:   entry.Action,
			ActorID:  entry.ActorID,
			Details:  fmt.Sprintf("export_id=%s", entry.ExportID),
		})
	}
	exportSvc := export.NewService(sub, logger.Named("export"), auditCallback)

	// Rate limiters.
	perIPLimiter := middleware.NewRateLimiter(cfg.RateLimitPerIP)
	perTenantLimiter := middleware.NewRateLimiter(cfg.RateLimitPerTenant)

	// Router.
	r := chi.NewRouter()

	// Global middleware.
	r.Use(chimw.RealIP)
	r.Use(middleware.InjectRequestID)
	r.Use(cors.Handler(cors.Options{
		AllowedOrigins:   []string{"*"},
		AllowedMethods:   []string{"GET", "POST", "PUT", "DELETE", "OPTIONS"},
		AllowedHeaders:   []string{"Accept", "Authorization", "Content-Type", "X-Request-Id"},
		ExposedHeaders:   []string{"X-Request-Id"},
		AllowCredentials: true,
		MaxAge:           300,
	}))
	r.Use(middleware.MaxBodySize(logger))
	r.Use(middleware.Auth(cfg.APIKey, cfg.JWTSecret, logger))
	r.Use(middleware.RateLimit(perIPLimiter, perTenantLimiter, logger))

	// API v1 routes.
	r.Route("/api/v1", func(r chi.Router) {
		// Evidence / knowledge.
		r.Post("/ingest", ingestHandler(sub, auditSvc, logger))
		r.Post("/query", queryHandler(sub, logger))
		r.Get("/evidence/{id}", getEvidenceHandler(sub, logger))
		r.Get("/memories", listMemoriesHandler(sub, logger))
		r.Post("/forget/{scope_id}", forgetHandler(sub, auditSvc, logger))

		// Synthesis.
		r.Post("/synthesis/trigger", synthesisTriggerHandler(sub, auditSvc, logger))
		r.Get("/synthesis/{id}/status", synthesisStatusHandler(logger))
		r.Get("/synthesis/recent", synthesisRecentHandler(logger))

		// Connectors.
		r.Post("/connectors", createConnectorHandler(connSvc, auditSvc, logger))
		r.Get("/connectors", listConnectorsHandler(connSvc))
		r.Post("/connectors/{id}/authenticate", authenticateConnectorHandler(connSvc, logger))
		r.Post("/connectors/{id}/sync", syncConnectorHandler(connSvc, auditSvc, logger))
		r.Delete("/connectors/{id}", removeConnectorHandler(connSvc, auditSvc, logger))
		r.Get("/connectors/{id}/status", connectorStatusHandler(connSvc))

		// Tenants.
		r.Post("/tenants", createTenantHandler(tenantSvc, auditSvc, logger))
		r.Get("/tenants/{id}", getTenantHandler(tenantSvc))

		// Export.
		r.Post("/export/profile", exportProfileHandler(exportSvc, logger))

		// Audit.
		r.Get("/audit", queryAuditHandler(auditSvc))

		// Health & metrics.
		r.Get("/health", healthHandler(sub, logger))
		r.Get("/metrics", metricsHandler(sub))

		// SCIM v2.
		r.Route("/scim/v2", func(r chi.Router) {
			r.Post("/Users", scimCreateUserHandler(permSvc))
			r.Get("/Users", scimListUsersHandler(permSvc))
			r.Get("/Users/{id}", scimGetUserHandler(permSvc))
			r.Post("/Groups", scimCreateGroupHandler(permSvc))
			r.Get("/Groups", scimListGroupsHandler(permSvc))
			r.Get("/Groups/{id}", scimGetGroupHandler(permSvc))
		})

		// Permissions.
		r.Post("/permissions/grant", grantPermissionHandler(permSvc))
		r.Post("/permissions/revoke", revokePermissionHandler(permSvc))
		r.Post("/permissions/check", checkPermissionHandler(permSvc))
	})

	// Start background services.
	connSvc.StartScheduler()
	auditSvc.StartRetentionEnforcer()

	srv := &http.Server{
		Addr:    cfg.ListenAddr,
		Handler: r,
	}

	// Graceful shutdown.
	go func() {
		sigCh := make(chan os.Signal, 1)
		signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
		<-sigCh
		logger.Info("shutdown signal received")

		ctx, cancel := context.WithTimeout(context.Background(), cfg.ShutdownTimeout)
		defer cancel()

		connSvc.StopScheduler()
		auditSvc.StopRetentionEnforcer()

		if err := srv.Shutdown(ctx); err != nil {
			logger.Error("server shutdown error", zap.Error(err))
		}
	}()

	logger.Info("gateway starting", zap.String("addr", cfg.ListenAddr))
	if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
		logger.Fatal("server failed", zap.Error(err))
	}
	logger.Info("gateway stopped")
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

func ingestHandler(sub *substrate.Client, auditSvc *audit.Service, logger *zap.Logger) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		var req struct {
			ScopeID    string `json:"scope_id"`
			Body       string `json:"body"`
			Source     string `json:"source"`
			Importance string `json:"importance"`
		}
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			httputil.Error(w, http.StatusBadRequest, "invalid request body")
			return
		}
		if !isValidUUID(req.ScopeID) {
			httputil.Error(w, http.StatusBadRequest, "scope_id must be a valid UUID")
			return
		}
		if !middleware.ValidateUTF8Body(req.Body) {
			httputil.Error(w, http.StatusBadRequest, "body must be valid UTF-8")
			return
		}

		resp, err := sub.Ingest(r.Context(), &substrate.IngestRequest{
			ScopeID:    req.ScopeID,
			Body:       req.Body,
			Source:     req.Source,
			Importance: req.Importance,
		})
		if err != nil {
			logger.Error("ingest failed", zap.Error(err))
			httputil.Error(w, http.StatusInternalServerError, err.Error())
			return
		}

		_ = auditSvc.Record(r.Context(), &audit.Event{
			TenantID: middleware.TenantID(r.Context()),
			ScopeID:  req.ScopeID,
			Action:   "evidence.ingest",
			ActorID:  middleware.ActorID(r.Context()),
		})

		httputil.JSON(w, http.StatusCreated, resp)
	}
}

func queryHandler(sub *substrate.Client, logger *zap.Logger) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		var req struct {
			ScopeID   string `json:"scope_id"`
			QueryText string `json:"query_text"`
			Limit     int    `json:"limit"`
		}
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			httputil.Error(w, http.StatusBadRequest, "invalid request body")
			return
		}
		if !isValidUUID(req.ScopeID) {
			httputil.Error(w, http.StatusBadRequest, "scope_id must be a valid UUID")
			return
		}
		if req.Limit <= 0 {
			req.Limit = 20
		}

		resp, err := sub.Query(r.Context(), &substrate.QueryRequest{
			ScopeID:   req.ScopeID,
			QueryText: req.QueryText,
			Limit:     req.Limit,
		})
		if err != nil {
			logger.Error("query failed", zap.Error(err))
			httputil.Error(w, http.StatusInternalServerError, err.Error())
			return
		}
		httputil.JSON(w, http.StatusOK, resp)
	}
}

func getEvidenceHandler(sub *substrate.Client, logger *zap.Logger) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		id := chi.URLParam(r, "id")
		resp, err := sub.GetEvidence(r.Context(), id)
		if err != nil {
			logger.Error("get evidence failed", zap.Error(err))
			httputil.Error(w, http.StatusInternalServerError, err.Error())
			return
		}
		httputil.JSON(w, http.StatusOK, resp)
	}
}

func listMemoriesHandler(sub *substrate.Client, logger *zap.Logger) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		scopeID := r.URL.Query().Get("scope_id")
		if !isValidUUID(scopeID) {
			httputil.Error(w, http.StatusBadRequest, "scope_id must be a valid UUID")
			return
		}

		var state *string
		if s := r.URL.Query().Get("state"); s != "" {
			state = &s
		}
		pinnedOnly := r.URL.Query().Get("pinned_only") == "true"

		memories, err := sub.ListMemories(r.Context(), scopeID, state, pinnedOnly)
		if err != nil {
			logger.Error("list memories failed", zap.Error(err))
			httputil.Error(w, http.StatusInternalServerError, err.Error())
			return
		}
		httputil.JSON(w, http.StatusOK, memories)
	}
}

func forgetHandler(sub *substrate.Client, auditSvc *audit.Service, logger *zap.Logger) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		scopeID := chi.URLParam(r, "scope_id")
		if !isValidUUID(scopeID) {
			httputil.Error(w, http.StatusBadRequest, "scope_id must be a valid UUID")
			return
		}

		if err := sub.ForgetScope(r.Context(), scopeID); err != nil {
			logger.Error("forget scope failed", zap.Error(err))
			httputil.Error(w, http.StatusInternalServerError, err.Error())
			return
		}

		_ = auditSvc.Record(r.Context(), &audit.Event{
			TenantID: middleware.TenantID(r.Context()),
			ScopeID:  scopeID,
			Action:   "evidence.forget_scope",
			ActorID:  middleware.ActorID(r.Context()),
		})

		w.WriteHeader(http.StatusNoContent)
	}
}

func synthesisTriggerHandler(sub *substrate.Client, auditSvc *audit.Service, logger *zap.Logger) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		var req struct {
			ScopeID string `json:"scope_id"`
			Trigger string `json:"trigger"`
		}
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			httputil.Error(w, http.StatusBadRequest, "invalid request body")
			return
		}

		resp, err := sub.TriggerSynthesis(r.Context(), &substrate.SynthesisTriggerRequest{
			ScopeID: req.ScopeID,
			Trigger: req.Trigger,
		})
		if err != nil {
			logger.Error("synthesis trigger failed", zap.Error(err))
			httputil.Error(w, http.StatusInternalServerError, err.Error())
			return
		}

		_ = auditSvc.Record(r.Context(), &audit.Event{
			TenantID: middleware.TenantID(r.Context()),
			ScopeID:  req.ScopeID,
			Action:   "synthesis.trigger",
			ActorID:  middleware.ActorID(r.Context()),
		})

		httputil.JSON(w, http.StatusAccepted, resp)
	}
}

func synthesisStatusHandler(logger *zap.Logger) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		id := chi.URLParam(r, "id")
		// SSE/NDJSON streaming support — for now return a status stub.
		// In production this would poll the substrate synthesis_status endpoint.
		httputil.JSON(w, http.StatusOK, map[string]string{
			"id":     id,
			"status": "completed",
		})
	}
}

func synthesisRecentHandler(logger *zap.Logger) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		// In production this calls substrate list_recent_syntheses.
		httputil.JSON(w, http.StatusOK, []interface{}{})
	}
}

func createConnectorHandler(svc *connector.Service, auditSvc *audit.Service, logger *zap.Logger) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		var req connector.CreateRequest
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			httputil.Error(w, http.StatusBadRequest, "invalid request body")
			return
		}
		req.TenantID = middleware.TenantID(r.Context())

		inst, err := svc.Create(r.Context(), &req)
		if err != nil {
			logger.Error("create connector failed", zap.Error(err))
			httputil.Error(w, http.StatusInternalServerError, err.Error())
			return
		}

		_ = auditSvc.Record(r.Context(), &audit.Event{
			TenantID: req.TenantID,
			Action:   "connector.create",
			ActorID:  middleware.ActorID(r.Context()),
			Details:  fmt.Sprintf("connector_id=%s kind=%s", inst.ID, inst.Kind),
		})

		httputil.JSON(w, http.StatusCreated, inst)
	}
}

func listConnectorsHandler(svc *connector.Service) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		tenantID := middleware.TenantID(r.Context())
		list := svc.List(r.Context(), tenantID)
		httputil.JSON(w, http.StatusOK, list)
	}
}

func authenticateConnectorHandler(svc *connector.Service, logger *zap.Logger) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		id := chi.URLParam(r, "id")
		resp, err := svc.Authenticate(r.Context(), id)
		if err != nil {
			logger.Error("authenticate connector failed", zap.Error(err))
			httputil.Error(w, http.StatusNotFound, err.Error())
			return
		}
		httputil.JSON(w, http.StatusOK, resp)
	}
}

func syncConnectorHandler(svc *connector.Service, auditSvc *audit.Service, logger *zap.Logger) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		id := chi.URLParam(r, "id")
		resp, err := svc.Sync(r.Context(), id)
		if err != nil {
			logger.Error("sync connector failed", zap.Error(err))
			httputil.Error(w, http.StatusInternalServerError, err.Error())
			return
		}

		_ = auditSvc.Record(r.Context(), &audit.Event{
			TenantID: middleware.TenantID(r.Context()),
			Action:   "connector.sync",
			ActorID:  middleware.ActorID(r.Context()),
			Details:  fmt.Sprintf("connector_id=%s", id),
		})

		httputil.JSON(w, http.StatusOK, resp)
	}
}

func removeConnectorHandler(svc *connector.Service, auditSvc *audit.Service, logger *zap.Logger) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		id := chi.URLParam(r, "id")
		if err := svc.Remove(r.Context(), id); err != nil {
			logger.Error("remove connector failed", zap.Error(err))
			httputil.Error(w, http.StatusNotFound, err.Error())
			return
		}

		_ = auditSvc.Record(r.Context(), &audit.Event{
			TenantID: middleware.TenantID(r.Context()),
			Action:   "connector.remove",
			ActorID:  middleware.ActorID(r.Context()),
			Details:  fmt.Sprintf("connector_id=%s", id),
		})

		w.WriteHeader(http.StatusNoContent)
	}
}

func connectorStatusHandler(svc *connector.Service) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		id := chi.URLParam(r, "id")
		status, err := svc.GetStatus(id)
		if err != nil {
			httputil.Error(w, http.StatusNotFound, err.Error())
			return
		}
		httputil.JSON(w, http.StatusOK, status)
	}
}

func createTenantHandler(svc *tenant.Service, auditSvc *audit.Service, logger *zap.Logger) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		var req tenant.CreateRequest
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			httputil.Error(w, http.StatusBadRequest, "invalid request body")
			return
		}

		t, err := svc.Create(r.Context(), &req)
		if err != nil {
			logger.Error("create tenant failed", zap.Error(err))
			httputil.Error(w, http.StatusInternalServerError, err.Error())
			return
		}

		_ = auditSvc.Record(r.Context(), &audit.Event{
			TenantID: t.ID,
			Action:   "tenant.create",
			ActorID:  middleware.ActorID(r.Context()),
		})

		httputil.JSON(w, http.StatusCreated, t)
	}
}

func getTenantHandler(svc *tenant.Service) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		id := chi.URLParam(r, "id")
		t, err := svc.Get(r.Context(), id)
		if err != nil {
			httputil.Error(w, http.StatusNotFound, err.Error())
			return
		}
		httputil.JSON(w, http.StatusOK, t)
	}
}

func exportProfileHandler(svc *export.Service, logger *zap.Logger) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		var req export.ProfileRequest
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			httputil.Error(w, http.StatusBadRequest, "invalid request body")
			return
		}
		req.TenantID = middleware.TenantID(r.Context())

		resp, err := svc.GenerateProfile(r.Context(), &req, middleware.ActorID(r.Context()))
		if err != nil {
			logger.Error("export failed", zap.Error(err))
			httputil.Error(w, http.StatusInternalServerError, err.Error())
			return
		}
		httputil.JSON(w, http.StatusOK, resp)
	}
}

func queryAuditHandler(svc *audit.Service) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		params := &audit.QueryParams{
			TenantID: middleware.TenantID(r.Context()),
			ScopeID:  r.URL.Query().Get("scope_id"),
			Action:   r.URL.Query().Get("action"),
			ActorID:  r.URL.Query().Get("actor_id"),
		}
		events, err := svc.Query(r.Context(), params)
		if err != nil {
			httputil.Error(w, http.StatusInternalServerError, err.Error())
			return
		}
		httputil.JSON(w, http.StatusOK, events)
	}
}

func healthHandler(sub *substrate.Client, logger *zap.Logger) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		resp := map[string]interface{}{
			"status":  "ok",
			"gateway": "healthy",
		}

		// Probe substrate server.
		health, err := sub.Health(r.Context())
		if err != nil {
			resp["substrate"] = map[string]string{"status": "unhealthy", "error": err.Error()}
			resp["status"] = "degraded"
		} else {
			resp["substrate"] = health
		}

		httputil.JSON(w, http.StatusOK, resp)
	}
}

func metricsHandler(sub *substrate.Client) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		// Serve Go process metrics via prometheus.
		promhttp.Handler().ServeHTTP(w, r)
	}
}

// SCIM handlers.
func scimCreateUserHandler(svc *permission.Service) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		var u permission.SCIMUser
		if err := json.NewDecoder(r.Body).Decode(&u); err != nil {
			httputil.Error(w, http.StatusBadRequest, "invalid SCIM user payload")
			return
		}
		created, err := svc.CreateUser(r.Context(), &u)
		if err != nil {
			httputil.Error(w, http.StatusInternalServerError, err.Error())
			return
		}
		httputil.JSON(w, http.StatusCreated, created)
	}
}

func scimListUsersHandler(svc *permission.Service) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		users := svc.ListUsers(r.Context())
		httputil.JSON(w, http.StatusOK, map[string]interface{}{
			"schemas":      []string{"urn:ietf:params:scim:api:messages:2.0:ListResponse"},
			"totalResults": len(users),
			"Resources":    users,
		})
	}
}

func scimGetUserHandler(svc *permission.Service) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		id := chi.URLParam(r, "id")
		u, err := svc.GetUser(r.Context(), id)
		if err != nil {
			httputil.Error(w, http.StatusNotFound, err.Error())
			return
		}
		httputil.JSON(w, http.StatusOK, u)
	}
}

func scimCreateGroupHandler(svc *permission.Service) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		var g permission.SCIMGroup
		if err := json.NewDecoder(r.Body).Decode(&g); err != nil {
			httputil.Error(w, http.StatusBadRequest, "invalid SCIM group payload")
			return
		}
		created, err := svc.CreateGroup(r.Context(), &g)
		if err != nil {
			httputil.Error(w, http.StatusInternalServerError, err.Error())
			return
		}
		httputil.JSON(w, http.StatusCreated, created)
	}
}

func scimListGroupsHandler(svc *permission.Service) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		groups := svc.ListGroups(r.Context())
		httputil.JSON(w, http.StatusOK, map[string]interface{}{
			"schemas":      []string{"urn:ietf:params:scim:api:messages:2.0:ListResponse"},
			"totalResults": len(groups),
			"Resources":    groups,
		})
	}
}

func scimGetGroupHandler(svc *permission.Service) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		id := chi.URLParam(r, "id")
		g, err := svc.GetGroup(r.Context(), id)
		if err != nil {
			httputil.Error(w, http.StatusNotFound, err.Error())
			return
		}
		httputil.JSON(w, http.StatusOK, g)
	}
}

func grantPermissionHandler(svc *permission.Service) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		var t permission.Tuple
		if err := json.NewDecoder(r.Body).Decode(&t); err != nil {
			httputil.Error(w, http.StatusBadRequest, "invalid tuple payload")
			return
		}
		if err := svc.Grant(r.Context(), &t); err != nil {
			httputil.Error(w, http.StatusInternalServerError, err.Error())
			return
		}
		w.WriteHeader(http.StatusNoContent)
	}
}

func revokePermissionHandler(svc *permission.Service) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		var t permission.Tuple
		if err := json.NewDecoder(r.Body).Decode(&t); err != nil {
			httputil.Error(w, http.StatusBadRequest, "invalid tuple payload")
			return
		}
		if err := svc.Revoke(r.Context(), &t); err != nil {
			httputil.Error(w, http.StatusInternalServerError, err.Error())
			return
		}
		w.WriteHeader(http.StatusNoContent)
	}
}

func checkPermissionHandler(svc *permission.Service) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		var req permission.CheckRequest
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			httputil.Error(w, http.StatusBadRequest, "invalid check payload")
			return
		}
		resp := svc.Check(r.Context(), &req)
		httputil.JSON(w, http.StatusOK, resp)
	}
}

func isValidUUID(s string) bool {
	_, err := uuid.Parse(s)
	return err == nil
}
