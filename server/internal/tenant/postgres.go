package tenant

import (
	"context"
	"errors"
	"fmt"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgconn"
	"github.com/jackc/pgx/v5/pgxpool"
)

// pgUniqueViolation is the SQLSTATE for a unique-constraint violation.
const pgUniqueViolation = "23505"

// PostgresStore is a pgx-backed [Store]. All queries are parameterised;
// no statement is ever built by string concatenation of user input.
type PostgresStore struct {
	pool *pgxpool.Pool
}

// NewPostgresStore wraps an existing pgx pool.
func NewPostgresStore(pool *pgxpool.Pool) *PostgresStore {
	return &PostgresStore{pool: pool}
}

// Migrate creates the tenant tables if they do not already exist.
func (p *PostgresStore) Migrate(ctx context.Context) error {
	const ddl = `
CREATE TABLE IF NOT EXISTS tenants (
    id              UUID PRIMARY KEY,
    name            TEXT NOT NULL,
    connector_limit INTEGER NOT NULL,
    synthesis_tier  TEXT NOT NULL,
    retention_days  INTEGER NOT NULL,
    key_algorithm   TEXT NOT NULL,
    key_public_hex  TEXT NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL
);
CREATE TABLE IF NOT EXISTS tenant_members (
    tenant_id  UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    user_id    UUID NOT NULL,
    email      TEXT NOT NULL,
    status     TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (tenant_id, user_id)
);`
	if _, err := p.pool.Exec(ctx, ddl); err != nil {
		return fmt.Errorf("tenant: migrate: %w", err)
	}
	return nil
}

// CreateTenant implements [Store].
func (p *PostgresStore) CreateTenant(ctx context.Context, t Tenant) error {
	const q = `INSERT INTO tenants
        (id, name, connector_limit, synthesis_tier, retention_days, key_algorithm, key_public_hex, created_at)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8)`
	_, err := p.pool.Exec(ctx, q,
		t.ID, t.Name, t.Config.ConnectorLimit, string(t.Config.SynthesisTier),
		t.Config.RetentionDays, t.Key.Algorithm, t.Key.PublicKeyHex, t.CreatedAt)
	return mapPgError(err)
}

// GetTenant implements [Store].
func (p *PostgresStore) GetTenant(ctx context.Context, id string) (Tenant, error) {
	const q = `SELECT id, name, connector_limit, synthesis_tier, retention_days,
        key_algorithm, key_public_hex, created_at FROM tenants WHERE id = $1`
	var t Tenant
	var tier string
	err := p.pool.QueryRow(ctx, q, id).Scan(
		&t.ID, &t.Name, &t.Config.ConnectorLimit, &tier, &t.Config.RetentionDays,
		&t.Key.Algorithm, &t.Key.PublicKeyHex, &t.CreatedAt)
	if errors.Is(err, pgx.ErrNoRows) {
		return Tenant{}, ErrNotFound
	}
	if err != nil {
		return Tenant{}, fmt.Errorf("tenant: get: %w", err)
	}
	t.Config.SynthesisTier = SynthesisTier(tier)
	return t, nil
}

// ListTenants implements [Store].
func (p *PostgresStore) ListTenants(ctx context.Context) ([]Tenant, error) {
	const q = `SELECT id, name, connector_limit, synthesis_tier, retention_days,
        key_algorithm, key_public_hex, created_at FROM tenants ORDER BY created_at`
	rows, err := p.pool.Query(ctx, q)
	if err != nil {
		return nil, fmt.Errorf("tenant: list: %w", err)
	}
	defer rows.Close()
	var out []Tenant
	for rows.Next() {
		var t Tenant
		var tier string
		if err := rows.Scan(&t.ID, &t.Name, &t.Config.ConnectorLimit, &tier,
			&t.Config.RetentionDays, &t.Key.Algorithm, &t.Key.PublicKeyHex, &t.CreatedAt); err != nil {
			return nil, fmt.Errorf("tenant: scan: %w", err)
		}
		t.Config.SynthesisTier = SynthesisTier(tier)
		out = append(out, t)
	}
	return out, rows.Err()
}

// UpdateTenant implements [Store].
func (p *PostgresStore) UpdateTenant(ctx context.Context, t Tenant) error {
	const q = `UPDATE tenants SET name=$2, connector_limit=$3, synthesis_tier=$4,
        retention_days=$5, key_algorithm=$6, key_public_hex=$7 WHERE id=$1`
	tag, err := p.pool.Exec(ctx, q, t.ID, t.Name, t.Config.ConnectorLimit,
		string(t.Config.SynthesisTier), t.Config.RetentionDays, t.Key.Algorithm, t.Key.PublicKeyHex)
	if err != nil {
		return fmt.Errorf("tenant: update: %w", err)
	}
	if tag.RowsAffected() == 0 {
		return ErrNotFound
	}
	return nil
}

// DeleteTenant implements [Store].
func (p *PostgresStore) DeleteTenant(ctx context.Context, id string) error {
	tag, err := p.pool.Exec(ctx, `DELETE FROM tenants WHERE id = $1`, id)
	if err != nil {
		return fmt.Errorf("tenant: delete: %w", err)
	}
	if tag.RowsAffected() == 0 {
		return ErrNotFound
	}
	return nil
}

// UpsertMember implements [Store].
func (p *PostgresStore) UpsertMember(ctx context.Context, m Member) error {
	const q = `INSERT INTO tenant_members (tenant_id, user_id, email, status, updated_at)
        VALUES ($1,$2,$3,$4,$5)
        ON CONFLICT (tenant_id, user_id)
        DO UPDATE SET email = EXCLUDED.email, status = EXCLUDED.status, updated_at = EXCLUDED.updated_at`
	_, err := p.pool.Exec(ctx, q, m.TenantID, m.UserID, m.Email, string(m.Status), m.UpdatedAt)
	if err != nil {
		return fmt.Errorf("tenant: upsert member: %w", err)
	}
	return nil
}

// GetMember implements [Store].
func (p *PostgresStore) GetMember(ctx context.Context, tenantID, userID string) (Member, error) {
	const q = `SELECT tenant_id, user_id, email, status, updated_at
        FROM tenant_members WHERE tenant_id = $1 AND user_id = $2`
	var m Member
	var status string
	err := p.pool.QueryRow(ctx, q, tenantID, userID).Scan(
		&m.TenantID, &m.UserID, &m.Email, &status, &m.UpdatedAt)
	if errors.Is(err, pgx.ErrNoRows) {
		return Member{}, ErrNotFound
	}
	if err != nil {
		return Member{}, fmt.Errorf("tenant: get member: %w", err)
	}
	m.Status = MemberStatus(status)
	return m, nil
}

// ListMembers implements [Store].
func (p *PostgresStore) ListMembers(ctx context.Context, tenantID string) ([]Member, error) {
	const q = `SELECT tenant_id, user_id, email, status, updated_at
        FROM tenant_members WHERE tenant_id = $1 ORDER BY user_id`
	rows, err := p.pool.Query(ctx, q, tenantID)
	if err != nil {
		return nil, fmt.Errorf("tenant: list members: %w", err)
	}
	defer rows.Close()
	var out []Member
	for rows.Next() {
		var m Member
		var status string
		if err := rows.Scan(&m.TenantID, &m.UserID, &m.Email, &status, &m.UpdatedAt); err != nil {
			return nil, fmt.Errorf("tenant: scan member: %w", err)
		}
		m.Status = MemberStatus(status)
		out = append(out, m)
	}
	return out, rows.Err()
}

// DeleteMember implements [Store].
func (p *PostgresStore) DeleteMember(ctx context.Context, tenantID, userID string) error {
	tag, err := p.pool.Exec(ctx,
		`DELETE FROM tenant_members WHERE tenant_id = $1 AND user_id = $2`, tenantID, userID)
	if err != nil {
		return fmt.Errorf("tenant: delete member: %w", err)
	}
	if tag.RowsAffected() == 0 {
		return ErrNotFound
	}
	return nil
}

// mapPgError converts a unique-violation into [ErrConflict].
func mapPgError(err error) error {
	if err == nil {
		return nil
	}
	var pgErr *pgconn.PgError
	if errors.As(err, &pgErr) && pgErr.Code == pgUniqueViolation {
		return ErrConflict
	}
	return fmt.Errorf("tenant: %w", err)
}

// compile-time assertion that PostgresStore satisfies Store.
var _ Store = (*PostgresStore)(nil)
