package audit

import (
	"context"
	"testing"
	"time"

	"github.com/google/uuid"
	"github.com/jackc/pgx/v5/pgxpool"
	"github.com/testcontainers/testcontainers-go"
	"github.com/testcontainers/testcontainers-go/modules/postgres"
	"github.com/testcontainers/testcontainers-go/wait"
)

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

func TestPostgresAuditIntegration(t *testing.T) {
	pool := startPostgres(t)
	ctx := context.Background()
	store := NewPostgresStore(pool)
	if err := store.Migrate(ctx); err != nil {
		t.Fatal(err)
	}

	now := time.Now().UTC()
	t1, t2 := uuid.NewString(), uuid.NewString()
	s1, s2 := uuid.NewString(), uuid.NewString()
	events := []Event{
		{ID: uuid.NewString(), TenantID: t1, ScopeID: s1, Action: "export", Actor: "a1", CreatedAt: now.Add(-2 * time.Hour)},
		{ID: uuid.NewString(), TenantID: t1, ScopeID: s2, Action: "ingest", Actor: "a2", CreatedAt: now.Add(-1 * time.Hour)},
		{ID: uuid.NewString(), TenantID: t2, ScopeID: s1, Action: "export", Actor: "a1", CreatedAt: now},
	}
	for _, e := range events {
		if err := store.Append(ctx, e); err != nil {
			t.Fatal(err)
		}
	}

	got, err := store.Query(ctx, Filter{TenantID: t1})
	if err != nil || len(got) != 2 {
		t.Fatalf("tenant filter: %v len=%d", err, len(got))
	}
	got, err = store.Query(ctx, Filter{Action: "export", Actor: "a1"})
	if err != nil || len(got) != 2 {
		t.Fatalf("action/actor filter: %v len=%d", err, len(got))
	}
	got, err = store.Query(ctx, Filter{ScopeID: s1, Limit: 1})
	if err != nil || len(got) != 1 {
		t.Fatalf("scope+limit: %v len=%d", err, len(got))
	}

	// Time-range filter.
	got, err = store.Query(ctx, Filter{From: now.Add(-90 * time.Minute)})
	if err != nil || len(got) != 2 {
		t.Fatalf("from filter: %v len=%d", err, len(got))
	}

	n, err := store.DeleteOlderThan(ctx, t1, now.Add(-90*time.Minute))
	if err != nil || n != 1 {
		t.Fatalf("delete: %v n=%d", err, n)
	}
	got, _ = store.Query(ctx, Filter{TenantID: t1})
	if len(got) != 1 {
		t.Fatalf("after retention left %d", len(got))
	}
}
