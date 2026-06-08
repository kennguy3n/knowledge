package audit

import (
	"context"
	"fmt"
	"strconv"
	"strings"
	"time"

	"github.com/jackc/pgx/v5/pgxpool"
)

// PostgresStore is a pgx-backed [Store]. Every query is parameterised;
// the dynamic WHERE clause appends `$N` placeholders only, never user
// input.
type PostgresStore struct {
	pool *pgxpool.Pool
}

// NewPostgresStore wraps an existing pgx pool.
func NewPostgresStore(pool *pgxpool.Pool) *PostgresStore {
	return &PostgresStore{pool: pool}
}

// Migrate creates the audit table and supporting indexes.
func (p *PostgresStore) Migrate(ctx context.Context) error {
	const ddl = `
CREATE TABLE IF NOT EXISTS audit_events (
    id         UUID PRIMARY KEY,
    tenant_id  UUID NOT NULL,
    scope_id   UUID,
    action     TEXT NOT NULL,
    actor      TEXT NOT NULL,
    detail     JSONB,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS audit_events_tenant_time ON audit_events (tenant_id, created_at DESC);
CREATE INDEX IF NOT EXISTS audit_events_scope ON audit_events (scope_id);`
	if _, err := p.pool.Exec(ctx, ddl); err != nil {
		return fmt.Errorf("audit: migrate: %w", err)
	}
	return nil
}

// Append implements [Store]; re-delivery of the same event id is a
// no-op (idempotent JetStream consumption).
func (p *PostgresStore) Append(ctx context.Context, e Event) error {
	const q = `INSERT INTO audit_events (id, tenant_id, scope_id, action, actor, detail, created_at)
        VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT (id) DO NOTHING`
	var scope any
	if e.ScopeID != "" {
		scope = e.ScopeID
	}
	var detail any
	if len(e.Detail) > 0 {
		detail = []byte(e.Detail)
	}
	_, err := p.pool.Exec(ctx, q, e.ID, e.TenantID, scope, e.Action, e.Actor, detail, e.CreatedAt)
	if err != nil {
		return fmt.Errorf("audit: append: %w", err)
	}
	return nil
}

// Query implements [Store] using a parameterised dynamic filter.
func (p *PostgresStore) Query(ctx context.Context, f Filter) ([]Event, error) {
	var (
		clauses []string
		args    []any
	)
	add := func(expr string, val any) {
		args = append(args, val)
		clauses = append(clauses, expr+"$"+strconv.Itoa(len(args)))
	}
	if f.TenantID != "" {
		add("tenant_id = ", f.TenantID)
	}
	if f.ScopeID != "" {
		add("scope_id = ", f.ScopeID)
	}
	if f.Action != "" {
		add("action = ", f.Action)
	}
	if f.Actor != "" {
		add("actor = ", f.Actor)
	}
	if !f.From.IsZero() {
		add("created_at >= ", f.From)
	}
	if !f.To.IsZero() {
		add("created_at <= ", f.To)
	}

	q := "SELECT id, tenant_id, COALESCE(scope_id::text, ''), action, actor, detail, created_at FROM audit_events"
	if len(clauses) > 0 {
		q += " WHERE " + strings.Join(clauses, " AND ")
	}
	args = append(args, clampLimit(f.Limit, maxLimit))
	q += " ORDER BY created_at DESC LIMIT $" + strconv.Itoa(len(args))

	rows, err := p.pool.Query(ctx, q, args...)
	if err != nil {
		return nil, fmt.Errorf("audit: query: %w", err)
	}
	defer rows.Close()
	var out []Event
	for rows.Next() {
		var e Event
		var detail []byte
		if err := rows.Scan(&e.ID, &e.TenantID, &e.ScopeID, &e.Action, &e.Actor, &detail, &e.CreatedAt); err != nil {
			return nil, fmt.Errorf("audit: scan: %w", err)
		}
		e.Detail = detail
		out = append(out, e)
	}
	return out, rows.Err()
}

// DeleteOlderThan implements [Store].
func (p *PostgresStore) DeleteOlderThan(ctx context.Context, tenantID string, cutoff time.Time) (int64, error) {
	tag, err := p.pool.Exec(ctx,
		`DELETE FROM audit_events WHERE tenant_id = $1 AND created_at < $2`, tenantID, cutoff)
	if err != nil {
		return 0, fmt.Errorf("audit: delete: %w", err)
	}
	return tag.RowsAffected(), nil
}

var _ Store = (*PostgresStore)(nil)
