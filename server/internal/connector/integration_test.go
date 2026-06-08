package connector

import (
	"context"
	"encoding/json"
	"net/http"
	"strings"
	"testing"
	"time"

	"github.com/jackc/pgx/v5/pgxpool"
	"github.com/testcontainers/testcontainers-go"
	"github.com/testcontainers/testcontainers-go/modules/postgres"
	"github.com/testcontainers/testcontainers-go/wait"
)

// startPostgres spins up a throwaway Postgres container and returns a
// connected pool. The test is skipped when Docker is unavailable so the
// suite stays green on machines without a container runtime (the
// embedded-substrate unit tests always run).
func startPostgres(t *testing.T) *pgxpool.Pool {
	t.Helper()
	ctx := context.Background()
	ctr, err := postgres.Run(ctx, "postgres:16-alpine",
		postgres.WithDatabase("knowledge"),
		postgres.WithUsername("test"),
		postgres.WithPassword("test"),
		testcontainers.WithWaitStrategy(
			wait.ForLog("database system is ready to accept connections").
				WithOccurrence(2).WithStartupTimeout(60*time.Second)),
	)
	if err != nil {
		t.Skipf("docker/postgres unavailable: %v", err)
	}
	t.Cleanup(func() { _ = ctr.Terminate(ctx) })

	dsn, err := ctr.ConnectionString(ctx, "sslmode=disable")
	if err != nil {
		t.Fatal(err)
	}
	pool, err := pgxpool.New(ctx, dsn)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(pool.Close)
	return pool
}

// TestConnectorGatewayPostgresPersistence exercises the connector
// gateway against a real Postgres substrate (not the noop in-memory
// store): a registration created through the HTTP surface is durably
// written, survives a simulated gateway restart via Rehydrate, and is
// removed from durable storage on delete. The substrate FFI calls are
// faked; the persistence substrate under test is real.
func TestConnectorGatewayPostgresPersistence(t *testing.T) {
	pool := startPostgres(t)
	ctx := context.Background()

	regStore := NewPostgresRegistrationStore(pool)
	if err := regStore.Migrate(ctx); err != nil {
		t.Fatalf("migrate: %v", err)
	}

	const instanceID = "inst-pg-1"
	// The substrate's authoritative connector list reports our instance
	// so rehydration reconciliation keeps (does not prune) it.
	sub := &fakeSub{
		createID: instanceID,
		listRaw:  json.RawMessage(`[{"instanceId":"` + instanceID + `"}]`),
	}
	svc := New(sub, nil, Options{
		PublicBaseURL:     "https://api.example.com",
		SyncInterval:      time.Minute,
		RegistrationStore: regStore,
	})
	h := svc.Routes()

	// Create a connector through the gateway HTTP surface.
	rec := req(h, http.MethodPost, "/", `{"kind":"google_drive","scope_id":"`+scopeUUID+`"}`)
	if rec.Code != http.StatusCreated {
		t.Fatalf("create code = %d body=%s", rec.Code, rec.Body.String())
	}

	// Activate a webhook so a non-trivial orchestration field is
	// persisted and asserted to survive the restart.
	rec = req(h, http.MethodPost, "/"+instanceID+"/webhook/register", "")
	if rec.Code != http.StatusOK {
		t.Fatalf("webhook register code = %d body=%s", rec.Code, rec.Body.String())
	}

	// The registration must be in durable storage, not just the cache.
	persisted, err := regStore.List(ctx)
	if err != nil {
		t.Fatalf("list persisted: %v", err)
	}
	if len(persisted) != 1 || persisted[0].InstanceID != instanceID {
		t.Fatalf("durable registrations = %+v, want one for %s", persisted, instanceID)
	}
	if !persisted[0].WebhookActive ||
		!strings.HasSuffix(persisted[0].WebhookURL, "/api/v1/connectors/"+instanceID+"/webhook") {
		t.Fatalf("webhook state not persisted: %+v", persisted[0])
	}

	// Simulate a gateway restart: a fresh Service backed by the same
	// pool must rehydrate the registration, its webhook state, and its
	// sync schedule from Postgres alone.
	svc2 := New(sub, nil, Options{
		PublicBaseURL:     "https://api.example.com",
		SyncInterval:      time.Minute,
		RegistrationStore: regStore,
	})
	if err := svc2.Rehydrate(ctx); err != nil {
		t.Fatalf("rehydrate: %v", err)
	}
	reg, ok := svc2.store.get(instanceID)
	if !ok {
		t.Fatal("rehydrated cache missing the registration")
	}
	if !reg.WebhookActive || reg.Kind != "google_drive" {
		t.Fatalf("rehydrated registration lost state: %+v", reg)
	}
	if svc2.sched.Count() != 1 {
		t.Fatalf("rehydrate scheduled %d jobs, want 1", svc2.sched.Count())
	}
	svc2.sched.Stop()

	// Deleting through the gateway removes the durable row.
	rec = req(h, http.MethodDelete, "/"+instanceID, "")
	if rec.Code != http.StatusNoContent {
		t.Fatalf("delete code = %d", rec.Code)
	}
	persisted, err = regStore.List(ctx)
	if err != nil {
		t.Fatalf("list after delete: %v", err)
	}
	if len(persisted) != 0 {
		t.Fatalf("durable registrations after delete = %+v, want none", persisted)
	}
	svc.Stop()
}

// TestConnectorGatewayPostgresPrunesStale verifies the rehydrate
// reconciliation against the real store: a persisted registration whose
// connector no longer exists in the substrate's authoritative list is
// pruned from durable storage on restart.
func TestConnectorGatewayPostgresPrunesStale(t *testing.T) {
	pool := startPostgres(t)
	ctx := context.Background()

	regStore := NewPostgresRegistrationStore(pool)
	if err := regStore.Migrate(ctx); err != nil {
		t.Fatalf("migrate: %v", err)
	}

	live := registration{InstanceID: "live-1", Kind: "google_drive", ScopeID: scopeUUID,
		SyncInterval: time.Minute, CreatedAt: time.Now().UTC()}
	stale := registration{InstanceID: "stale-1", Kind: "slack", ScopeID: scopeUUID,
		SyncInterval: time.Minute, CreatedAt: time.Now().UTC()}
	for _, r := range []registration{live, stale} {
		if err := regStore.Save(ctx, r); err != nil {
			t.Fatalf("seed save %s: %v", r.InstanceID, err)
		}
	}

	// The substrate only knows about live-1, so stale-1 must be pruned.
	sub := &fakeSub{listRaw: json.RawMessage(`[{"instanceId":"live-1"}]`)}
	svc := New(sub, nil, Options{PublicBaseURL: "https://api.example.com", SyncInterval: time.Minute, RegistrationStore: regStore})
	if err := svc.Rehydrate(ctx); err != nil {
		t.Fatalf("rehydrate: %v", err)
	}
	svc.sched.Stop()

	if _, ok := svc.store.get("live-1"); !ok {
		t.Fatal("live registration was not restored")
	}
	if _, ok := svc.store.get("stale-1"); ok {
		t.Fatal("stale registration should have been pruned from the cache")
	}
	persisted, err := regStore.List(ctx)
	if err != nil {
		t.Fatalf("list: %v", err)
	}
	if len(persisted) != 1 || persisted[0].InstanceID != "live-1" {
		t.Fatalf("durable registrations = %+v, want only live-1", persisted)
	}
}
