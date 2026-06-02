package tenant

import (
	"context"
	"errors"
	"testing"
	"time"

	"github.com/google/uuid"
	"github.com/jackc/pgx/v5/pgxpool"
	"github.com/testcontainers/testcontainers-go"
	"github.com/testcontainers/testcontainers-go/modules/postgres"
	"github.com/testcontainers/testcontainers-go/wait"
)

// startPostgres spins up a throwaway Postgres container and returns a
// connected pool. The test is skipped when Docker is unavailable.
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

func TestPostgresStoreIntegration(t *testing.T) {
	pool := startPostgres(t)
	ctx := context.Background()
	store := NewPostgresStore(pool)
	if err := store.Migrate(ctx); err != nil {
		t.Fatal(err)
	}

	id := uuid.NewString()
	tn := Tenant{
		ID:        id,
		Name:      "Acme",
		Config:    DefaultConfig(),
		Key:       CryptoKey{Algorithm: "hybrid", PublicKeyHex: "ab"},
		CreatedAt: time.Now().UTC(),
	}
	if err := store.CreateTenant(ctx, tn); err != nil {
		t.Fatal(err)
	}
	if err := store.CreateTenant(ctx, tn); !errors.Is(err, ErrConflict) {
		t.Fatalf("expected conflict, got %v", err)
	}

	got, err := store.GetTenant(ctx, id)
	if err != nil || got.Name != "Acme" {
		t.Fatalf("get: %+v err=%v", got, err)
	}
	if _, err := store.GetTenant(ctx, uuid.NewString()); !errors.Is(err, ErrNotFound) {
		t.Fatalf("expected not found, got %v", err)
	}

	tn.Config.ConnectorLimit = 99
	if err := store.UpdateTenant(ctx, tn); err != nil {
		t.Fatal(err)
	}
	got, _ = store.GetTenant(ctx, id)
	if got.Config.ConnectorLimit != 99 {
		t.Fatalf("update not applied: %+v", got.Config)
	}

	list, err := store.ListTenants(ctx)
	if err != nil || len(list) != 1 {
		t.Fatalf("list: %v len=%d", err, len(list))
	}

	// Members.
	uid := uuid.NewString()
	m := Member{TenantID: id, UserID: uid, Email: "u@x.io", Status: StatusInvited, UpdatedAt: time.Now().UTC()}
	if err := store.UpsertMember(ctx, m); err != nil {
		t.Fatal(err)
	}
	m.Status = StatusActive
	if err := store.UpsertMember(ctx, m); err != nil {
		t.Fatal(err)
	}
	gm, err := store.GetMember(ctx, id, uid)
	if err != nil || gm.Status != StatusActive {
		t.Fatalf("member: %+v err=%v", gm, err)
	}
	members, err := store.ListMembers(ctx, id)
	if err != nil || len(members) != 1 {
		t.Fatalf("list members: %v len=%d", err, len(members))
	}
	if err := store.DeleteMember(ctx, id, uid); err != nil {
		t.Fatal(err)
	}
	if _, err := store.GetMember(ctx, id, uid); !errors.Is(err, ErrNotFound) {
		t.Fatalf("expected member gone, got %v", err)
	}

	// Delete tenant.
	if err := store.DeleteTenant(ctx, id); err != nil {
		t.Fatal(err)
	}
	if _, err := store.GetTenant(ctx, id); !errors.Is(err, ErrNotFound) {
		t.Fatalf("expected tenant gone, got %v", err)
	}
}
