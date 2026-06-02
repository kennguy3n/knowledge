// Command gateway is the public API gateway for the knowledge
// substrate. It loads configuration from the environment, wires the
// core services to the substrate_server loopback, and serves the
// /api/v1 surface with graceful shutdown.
package main

import (
	"context"
	"errors"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/jackc/pgx/v5/pgxpool"
	"github.com/nats-io/nats.go"
	"go.uber.org/zap"

	"github.com/kennguy3n/knowledge/server/internal/audit"
	"github.com/kennguy3n/knowledge/server/internal/config"
	"github.com/kennguy3n/knowledge/server/internal/connector"
	"github.com/kennguy3n/knowledge/server/internal/export"
	"github.com/kennguy3n/knowledge/server/internal/gateway"
	"github.com/kennguy3n/knowledge/server/internal/httpx"
	"github.com/kennguy3n/knowledge/server/internal/logging"
	"github.com/kennguy3n/knowledge/server/internal/middleware"
	"github.com/kennguy3n/knowledge/server/internal/permission"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
	"github.com/kennguy3n/knowledge/server/internal/tenant"
)

func main() {
	if err := run(); err != nil {
		// Logger may not exist yet; fall back to stderr via zap.
		l, _ := zap.NewProduction()
		l.Error("gateway exited with error", zap.Error(err))
		os.Exit(1)
	}
}

func run() error {
	cfg, err := config.Load()
	if err != nil {
		return err
	}
	log := logging.New()
	defer func() { _ = log.Sync() }()
	log.Info("gateway starting", zap.Any("config", cfg.Redacted()))

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()

	sub := substrate.New(cfg.SubstrateURL, httpx.NewClient(30*time.Second))
	ready := map[string]bool{}

	// Tenant + audit stores: Postgres when configured, else in-memory.
	var (
		tenantStore tenant.Store
		auditStore  audit.Store
		pool        *pgxpool.Pool
	)
	if cfg.DatabaseURL != "" {
		pool, err = pgxpool.New(ctx, cfg.DatabaseURL)
		if err != nil {
			return err
		}
		defer pool.Close()
		tps := tenant.NewPostgresStore(pool)
		aps := audit.NewPostgresStore(pool)
		if err := tps.Migrate(ctx); err != nil {
			return err
		}
		if err := aps.Migrate(ctx); err != nil {
			return err
		}
		tenantStore, auditStore = tps, aps
		ready["postgres"] = true
	} else {
		tenantStore = tenant.NewMemoryStore()
		auditStore = audit.NewMemoryStore()
		log.Warn("KNOWLEDGE_DATABASE_URL unset; using in-memory stores (development only)")
	}

	auditSvc := audit.New(auditStore)
	tenantSvc := tenant.New(tenantStore, sub)
	permSvc := permission.New(sub)
	exportSvc := export.New(sub, auditSvc)
	connSvc := connector.New(sub, log, connector.Options{
		PublicBaseURL: cfg.PublicBaseURL,
		SyncInterval:  cfg.SyncInterval,
	})
	connSvc.Start(ctx)
	defer connSvc.Stop()

	// Optional NATS JetStream audit consumer.
	if cfg.NATSURL != "" {
		nc, nerr := nats.Connect(cfg.NATSURL)
		if nerr != nil {
			return nerr
		}
		defer nc.Close()
		consumer := audit.NewConsumer(auditStore, log)
		go func() {
			if cerr := consumer.Run(ctx, nc); cerr != nil && !errors.Is(cerr, context.Canceled) {
				log.Error("audit consumer stopped", zap.Error(cerr))
			}
		}()
		retention := audit.NewRetention(auditStore,
			tenantLister{tenantStore}, retentionResolver{tenantStore}, time.Hour, log)
		go retention.Run(ctx)
		ready["nats"] = true
	}

	router := gateway.NewRouter(gateway.Deps{
		Substrate:   sub,
		Connectors:  connSvc,
		Permissions: permSvc,
		Tenants:     tenantSvc,
		Exports:     exportSvc,
		Audit:       auditSvc,
		Auth:        middleware.NewAuthenticator(cfg.APIKey, cfg.JWTSecret),
		RateLimiter: middleware.NewRateLimiter(cfg.RateIPRPS, cfg.RateTenantRPS, cfg.RateBurst),
		CORSOrigins: cfg.CORSOrigins,
		Log:         log,
		Ready:       ready,
	})

	srv := &http.Server{
		Addr:              cfg.ListenAddr,
		Handler:           router,
		ReadHeaderTimeout: 10 * time.Second,
		ReadTimeout:       60 * time.Second,
		WriteTimeout:      0, // unbounded: SSE synthesis streams are long-lived
		IdleTimeout:       120 * time.Second,
	}

	errCh := make(chan error, 1)
	go func() {
		log.Info("gateway listening", zap.String("addr", cfg.ListenAddr))
		if serr := srv.ListenAndServe(); serr != nil && !errors.Is(serr, http.ErrServerClosed) {
			errCh <- serr
		}
	}()

	select {
	case <-ctx.Done():
		log.Info("shutdown signal received")
	case serr := <-errCh:
		return serr
	}

	shutdownCtx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	return srv.Shutdown(shutdownCtx)
}

// tenantLister adapts a tenant.Store to audit.TenantLister.
type tenantLister struct{ store tenant.Store }

func (t tenantLister) TenantIDs(ctx context.Context) ([]string, error) {
	ts, err := t.store.ListTenants(ctx)
	if err != nil {
		return nil, err
	}
	ids := make([]string, 0, len(ts))
	for _, tn := range ts {
		ids = append(ids, tn.ID)
	}
	return ids, nil
}

// retentionResolver adapts a tenant.Store to audit.RetentionResolver.
type retentionResolver struct{ store tenant.Store }

func (r retentionResolver) RetentionDays(ctx context.Context, tenantID string) (int, bool) {
	tn, err := r.store.GetTenant(ctx, tenantID)
	if err != nil {
		return 0, false
	}
	return tn.Config.RetentionDays, true
}
