package connector

import (
	"context"
	"fmt"
	"time"

	"github.com/jackc/pgx/v5/pgxpool"
)

// RegistrationStore persists connector orchestration metadata so the
// gateway can rehydrate ingest scopes, webhook state, and sync
// schedules after a restart. The authoritative connector record lives
// in the substrate; this store holds only the Go-side supplemental
// state ([registration]).
type RegistrationStore interface {
	// Save inserts or updates a registration keyed by instance id.
	Save(ctx context.Context, r registration) error
	// Delete removes a registration. Deleting an absent registration is
	// not an error (delete is idempotent).
	Delete(ctx context.Context, instanceID string) error
	// List returns every persisted registration ordered by creation time.
	List(ctx context.Context) ([]registration, error)
}

// noopRegistrationStore is the default backend when no database is
// configured: registrations live only in the in-memory cache for the
// process lifetime, matching the gateway's other dev-mode stores. It is
// safe for concurrent use because it holds no state.
type noopRegistrationStore struct{}

// NewNoopRegistrationStore returns a [RegistrationStore] that persists
// nothing. Used for local development and tests where durability across
// restarts is not required.
func NewNoopRegistrationStore() RegistrationStore { return noopRegistrationStore{} }

func (noopRegistrationStore) Save(context.Context, registration) error     { return nil }
func (noopRegistrationStore) Delete(context.Context, string) error         { return nil }
func (noopRegistrationStore) List(context.Context) ([]registration, error) { return nil, nil }

// PostgresRegistrationStore is a pgx-backed [RegistrationStore]. Every
// query is parameterised; no statement is built by string concatenation
// of caller input.
type PostgresRegistrationStore struct {
	pool *pgxpool.Pool
}

// NewPostgresRegistrationStore wraps an existing pgx pool.
func NewPostgresRegistrationStore(pool *pgxpool.Pool) *PostgresRegistrationStore {
	return &PostgresRegistrationStore{pool: pool}
}

// Migrate creates the connector_registrations table if it does not
// already exist. It is idempotent and safe to call on every startup.
func (p *PostgresRegistrationStore) Migrate(ctx context.Context) error {
	const ddl = `
CREATE TABLE IF NOT EXISTS connector_registrations (
    instance_id      TEXT PRIMARY KEY,
    kind             TEXT NOT NULL,
    scope_id         UUID NOT NULL,
    webhook_url      TEXT NOT NULL DEFAULT '',
    webhook_active   BOOLEAN NOT NULL DEFAULT FALSE,
    sync_interval_ns BIGINT NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL
);`
	if _, err := p.pool.Exec(ctx, ddl); err != nil {
		return fmt.Errorf("connector: migrate: %w", err)
	}
	return nil
}

// Save implements [RegistrationStore]. The sync interval is stored as
// an integer nanosecond count so it round-trips through [time.Duration]
// without precision loss.
func (p *PostgresRegistrationStore) Save(ctx context.Context, r registration) error {
	const q = `INSERT INTO connector_registrations
        (instance_id, kind, scope_id, webhook_url, webhook_active, sync_interval_ns, created_at)
        VALUES ($1,$2,$3,$4,$5,$6,$7)
        ON CONFLICT (instance_id) DO UPDATE SET
            kind = EXCLUDED.kind,
            scope_id = EXCLUDED.scope_id,
            webhook_url = EXCLUDED.webhook_url,
            webhook_active = EXCLUDED.webhook_active,
            sync_interval_ns = EXCLUDED.sync_interval_ns`
	_, err := p.pool.Exec(ctx, q,
		r.InstanceID, r.Kind, r.ScopeID, r.WebhookURL, r.WebhookActive,
		int64(r.SyncInterval), r.CreatedAt)
	if err != nil {
		return fmt.Errorf("connector: save registration: %w", err)
	}
	return nil
}

// Delete implements [RegistrationStore].
func (p *PostgresRegistrationStore) Delete(ctx context.Context, instanceID string) error {
	if _, err := p.pool.Exec(ctx,
		`DELETE FROM connector_registrations WHERE instance_id = $1`, instanceID); err != nil {
		return fmt.Errorf("connector: delete registration: %w", err)
	}
	return nil
}

// List implements [RegistrationStore].
func (p *PostgresRegistrationStore) List(ctx context.Context) ([]registration, error) {
	const q = `SELECT instance_id, kind, scope_id, webhook_url, webhook_active, sync_interval_ns, created_at
        FROM connector_registrations ORDER BY created_at`
	rows, err := p.pool.Query(ctx, q)
	if err != nil {
		return nil, fmt.Errorf("connector: list registrations: %w", err)
	}
	defer rows.Close()
	var out []registration
	for rows.Next() {
		var (
			r          registration
			intervalNs int64
		)
		if err := rows.Scan(&r.InstanceID, &r.Kind, &r.ScopeID, &r.WebhookURL,
			&r.WebhookActive, &intervalNs, &r.CreatedAt); err != nil {
			return nil, fmt.Errorf("connector: scan registration: %w", err)
		}
		r.SyncInterval = time.Duration(intervalNs)
		out = append(out, r)
	}
	if err := rows.Err(); err != nil {
		return nil, fmt.Errorf("connector: iterate registrations: %w", err)
	}
	return out, nil
}
