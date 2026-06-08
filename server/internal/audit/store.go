package audit

import (
	"context"
	"sort"
	"sync"
	"time"
)

// maxLimit caps the number of rows any single query may return.
const maxLimit = 1000

// defaultLimit is applied when a filter omits a positive limit.
const defaultLimit = 100

// Store is the persistence boundary for audit events.
type Store interface {
	// Append durably records an event (idempotent on event id).
	Append(ctx context.Context, e Event) error
	// Query returns events matching the filter, newest first.
	Query(ctx context.Context, f Filter) ([]Event, error)
	// DeleteOlderThan removes a tenant's events created before cutoff
	// and returns the number deleted.
	DeleteOlderThan(ctx context.Context, tenantID string, cutoff time.Time) (int64, error)
}

// MemoryStore is a thread-safe in-memory [Store] for tests.
type MemoryStore struct {
	mu     sync.RWMutex
	events map[string]Event
}

// NewMemoryStore constructs an empty in-memory store.
func NewMemoryStore() *MemoryStore {
	return &MemoryStore{events: make(map[string]Event)}
}

// Append implements [Store].
func (m *MemoryStore) Append(_ context.Context, e Event) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.events[e.ID] = e
	return nil
}

// Query implements [Store].
func (m *MemoryStore) Query(_ context.Context, f Filter) ([]Event, error) {
	m.mu.RLock()
	out := make([]Event, 0, len(m.events))
	for _, e := range m.events {
		if !matches(e, f) {
			continue
		}
		out = append(out, e)
	}
	m.mu.RUnlock()

	sort.Slice(out, func(i, j int) bool { return out[i].CreatedAt.After(out[j].CreatedAt) })
	return out[:clampLimit(f.Limit, len(out))], nil
}

// DeleteOlderThan implements [Store].
func (m *MemoryStore) DeleteOlderThan(_ context.Context, tenantID string, cutoff time.Time) (int64, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	var n int64
	for id, e := range m.events {
		if e.TenantID == tenantID && e.CreatedAt.Before(cutoff) {
			delete(m.events, id)
			n++
		}
	}
	return n, nil
}

// matches reports whether e satisfies every set field of f.
func matches(e Event, f Filter) bool {
	if f.TenantID != "" && e.TenantID != f.TenantID {
		return false
	}
	if f.ScopeID != "" && e.ScopeID != f.ScopeID {
		return false
	}
	if f.Action != "" && e.Action != f.Action {
		return false
	}
	if f.Actor != "" && e.Actor != f.Actor {
		return false
	}
	if !f.From.IsZero() && e.CreatedAt.Before(f.From) {
		return false
	}
	if !f.To.IsZero() && e.CreatedAt.After(f.To) {
		return false
	}
	return true
}

// clampLimit resolves the effective row count for a query.
func clampLimit(limit, n int) int {
	if limit <= 0 {
		limit = defaultLimit
	}
	if limit > maxLimit {
		limit = maxLimit
	}
	if limit > n {
		limit = n
	}
	return limit
}
