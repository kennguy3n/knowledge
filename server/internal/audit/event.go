// Package audit is the Go audit service: it consumes audit events from
// NATS JetStream, persists them to Postgres, exposes a filtered query
// API, and enforces per-tenant retention.
package audit

import (
	"encoding/json"
	"time"
)

// Event is a single audit record. Detail carries action-specific
// structured context and must never contain raw evidence body text —
// only identifiers and metadata.
type Event struct {
	ID        string          `json:"id"`
	TenantID  string          `json:"tenant_id"`
	ScopeID   string          `json:"scope_id"`
	Action    string          `json:"action"`
	Actor     string          `json:"actor"`
	Detail    json.RawMessage `json:"detail,omitempty"`
	CreatedAt time.Time       `json:"created_at"`
}

// Filter constrains an audit query. Empty string fields are ignored;
// zero time bounds are ignored. Limit defaults to 100 and is capped at
// 1000 by the store.
type Filter struct {
	TenantID string
	ScopeID  string
	Action   string
	Actor    string
	From     time.Time
	To       time.Time
	Limit    int
}
