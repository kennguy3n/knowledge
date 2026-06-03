package permission

import (
	"context"
	"encoding/json"
	"fmt"

	"github.com/jackc/pgx/v5/pgxpool"
)

// DirectoryStore persists the SCIM directory (users and groups) so the
// gateway can rehydrate identity state after a restart. Without it the
// in-memory directory is lost on restart while the substrate membership
// tuples persist, leaving the directory diverged from the authorization
// state it is meant to drive.
//
// The authoritative authorization state lives in the substrate (relation
// tuples); this store holds only the SCIM identity records the gateway
// serves and reconciles against those tuples. All write handlers persist
// through this store before committing the in-memory cache, so a handler
// that returns success has durably recorded the change.
type DirectoryStore interface {
	// SaveUser inserts or updates a user keyed by id.
	SaveUser(ctx context.Context, u User) error
	// SaveGroup inserts or updates a group keyed by id.
	SaveGroup(ctx context.Context, g Group) error
	// DeleteUser removes the user and, in the same atomic operation,
	// persists the updated member lists of the groups the user was
	// stripped from, so a deleted user never lingers in a persisted group.
	DeleteUser(ctx context.Context, userID string, updatedGroups []Group) error
	// DeleteGroup removes a group. Deleting an absent group is not an
	// error (delete is idempotent).
	DeleteGroup(ctx context.Context, groupID string) error
	// ListUsers returns every persisted user ordered by creation time.
	ListUsers(ctx context.Context) ([]User, error)
	// ListGroups returns every persisted group ordered by creation time.
	ListGroups(ctx context.Context) ([]Group, error)
}

// noopDirectoryStore is the default backend when no database is
// configured: the directory lives only in the in-memory cache for the
// process lifetime, matching the gateway's other dev-mode stores. It is
// safe for concurrent use because it holds no state.
type noopDirectoryStore struct{}

// NewNoopDirectoryStore returns a [DirectoryStore] that persists nothing.
// Used for local development and tests where durability across restarts
// is not required.
func NewNoopDirectoryStore() DirectoryStore { return noopDirectoryStore{} }

func (noopDirectoryStore) SaveUser(context.Context, User) error              { return nil }
func (noopDirectoryStore) SaveGroup(context.Context, Group) error            { return nil }
func (noopDirectoryStore) DeleteUser(context.Context, string, []Group) error { return nil }
func (noopDirectoryStore) DeleteGroup(context.Context, string) error         { return nil }
func (noopDirectoryStore) ListUsers(context.Context) ([]User, error)         { return nil, nil }
func (noopDirectoryStore) ListGroups(context.Context) ([]Group, error)       { return nil, nil }

// PostgresDirectoryStore is a pgx-backed [DirectoryStore]. Every query is
// parameterised; no statement is built by string concatenation of caller
// input. Slice-valued fields (a user's emails, a group's members) are
// stored as JSONB.
type PostgresDirectoryStore struct {
	pool *pgxpool.Pool
}

// NewPostgresDirectoryStore wraps an existing pgx pool.
func NewPostgresDirectoryStore(pool *pgxpool.Pool) *PostgresDirectoryStore {
	return &PostgresDirectoryStore{pool: pool}
}

// Migrate creates the scim_users and scim_groups tables if they do not
// already exist. It is idempotent and safe to call on every startup.
func (p *PostgresDirectoryStore) Migrate(ctx context.Context) error {
	const ddl = `
CREATE TABLE IF NOT EXISTS scim_users (
    id            TEXT PRIMARY KEY,
    user_name     TEXT NOT NULL,
    active        BOOLEAN NOT NULL,
    emails        JSONB NOT NULL DEFAULT '[]'::jsonb,
    created       TIMESTAMPTZ NOT NULL,
    last_modified TIMESTAMPTZ NOT NULL
);
CREATE TABLE IF NOT EXISTS scim_groups (
    id            TEXT PRIMARY KEY,
    display_name  TEXT NOT NULL,
    members       JSONB NOT NULL DEFAULT '[]'::jsonb,
    created       TIMESTAMPTZ NOT NULL,
    last_modified TIMESTAMPTZ NOT NULL
);`
	if _, err := p.pool.Exec(ctx, ddl); err != nil {
		return fmt.Errorf("permission: migrate scim directory: %w", err)
	}
	return nil
}

// SaveUser implements [DirectoryStore].
func (p *PostgresDirectoryStore) SaveUser(ctx context.Context, u User) error {
	emails, err := json.Marshal(u.Emails)
	if err != nil {
		return fmt.Errorf("permission: marshal user emails: %w", err)
	}
	// created is intentionally omitted from the DO UPDATE set so an upsert
	// preserves the original creation timestamp rather than resetting it
	// (same rationale as connector_registrations).
	const q = `INSERT INTO scim_users (id, user_name, active, emails, created, last_modified)
        VALUES ($1,$2,$3,$4,$5,$6)
        ON CONFLICT (id) DO UPDATE SET
            user_name = EXCLUDED.user_name,
            active = EXCLUDED.active,
            emails = EXCLUDED.emails,
            last_modified = EXCLUDED.last_modified`
	if _, err := p.pool.Exec(ctx, q,
		u.ID, u.UserName, u.Active, emails, u.Meta.Created, u.Meta.LastModified); err != nil {
		return fmt.Errorf("permission: save scim user: %w", err)
	}
	return nil
}

// SaveGroup implements [DirectoryStore].
func (p *PostgresDirectoryStore) SaveGroup(ctx context.Context, g Group) error {
	members, err := json.Marshal(g.Members)
	if err != nil {
		return fmt.Errorf("permission: marshal group members: %w", err)
	}
	const q = `INSERT INTO scim_groups (id, display_name, members, created, last_modified)
        VALUES ($1,$2,$3,$4,$5)
        ON CONFLICT (id) DO UPDATE SET
            display_name = EXCLUDED.display_name,
            members = EXCLUDED.members,
            last_modified = EXCLUDED.last_modified`
	if _, err := p.pool.Exec(ctx, q,
		g.ID, g.DisplayName, members, g.Meta.Created, g.Meta.LastModified); err != nil {
		return fmt.Errorf("permission: save scim group: %w", err)
	}
	return nil
}

// DeleteUser implements [DirectoryStore]. The user row is removed and the
// supplied groups' member lists are rewritten in a single transaction, so
// the user is never left referenced by a persisted group.
func (p *PostgresDirectoryStore) DeleteUser(ctx context.Context, userID string, updatedGroups []Group) error {
	tx, err := p.pool.Begin(ctx)
	if err != nil {
		return fmt.Errorf("permission: begin delete scim user: %w", err)
	}
	defer func() { _ = tx.Rollback(ctx) }()
	if _, err := tx.Exec(ctx, `DELETE FROM scim_users WHERE id = $1`, userID); err != nil {
		return fmt.Errorf("permission: delete scim user: %w", err)
	}
	for _, g := range updatedGroups {
		members, err := json.Marshal(g.Members)
		if err != nil {
			return fmt.Errorf("permission: marshal group members: %w", err)
		}
		if _, err := tx.Exec(ctx,
			`UPDATE scim_groups SET members = $2 WHERE id = $1`, g.ID, members); err != nil {
			return fmt.Errorf("permission: update scim group members: %w", err)
		}
	}
	if err := tx.Commit(ctx); err != nil {
		return fmt.Errorf("permission: commit delete scim user: %w", err)
	}
	return nil
}

// DeleteGroup implements [DirectoryStore].
func (p *PostgresDirectoryStore) DeleteGroup(ctx context.Context, groupID string) error {
	if _, err := p.pool.Exec(ctx,
		`DELETE FROM scim_groups WHERE id = $1`, groupID); err != nil {
		return fmt.Errorf("permission: delete scim group: %w", err)
	}
	return nil
}

// ListUsers implements [DirectoryStore].
func (p *PostgresDirectoryStore) ListUsers(ctx context.Context) ([]User, error) {
	const q = `SELECT id, user_name, active, emails, created, last_modified
        FROM scim_users ORDER BY created`
	rows, err := p.pool.Query(ctx, q)
	if err != nil {
		return nil, fmt.Errorf("permission: list scim users: %w", err)
	}
	defer rows.Close()
	var out []User
	for rows.Next() {
		var (
			u      User
			emails []byte
		)
		if err := rows.Scan(&u.ID, &u.UserName, &u.Active, &emails,
			&u.Meta.Created, &u.Meta.LastModified); err != nil {
			return nil, fmt.Errorf("permission: scan scim user: %w", err)
		}
		if len(emails) > 0 {
			if err := json.Unmarshal(emails, &u.Emails); err != nil {
				return nil, fmt.Errorf("permission: unmarshal user emails: %w", err)
			}
		}
		u.Schemas = []string{schemaUser}
		u.Meta.ResourceType = "User"
		out = append(out, u)
	}
	if err := rows.Err(); err != nil {
		return nil, fmt.Errorf("permission: iterate scim users: %w", err)
	}
	return out, nil
}

// ListGroups implements [DirectoryStore].
func (p *PostgresDirectoryStore) ListGroups(ctx context.Context) ([]Group, error) {
	const q = `SELECT id, display_name, members, created, last_modified
        FROM scim_groups ORDER BY created`
	rows, err := p.pool.Query(ctx, q)
	if err != nil {
		return nil, fmt.Errorf("permission: list scim groups: %w", err)
	}
	defer rows.Close()
	var out []Group
	for rows.Next() {
		var (
			g       Group
			members []byte
		)
		if err := rows.Scan(&g.ID, &g.DisplayName, &members,
			&g.Meta.Created, &g.Meta.LastModified); err != nil {
			return nil, fmt.Errorf("permission: scan scim group: %w", err)
		}
		if len(members) > 0 {
			if err := json.Unmarshal(members, &g.Members); err != nil {
				return nil, fmt.Errorf("permission: unmarshal group members: %w", err)
			}
		}
		g.Schemas = []string{schemaGroup}
		g.Meta.ResourceType = "Group"
		out = append(out, g)
	}
	if err := rows.Err(); err != nil {
		return nil, fmt.Errorf("permission: iterate scim groups: %w", err)
	}
	return out, nil
}
