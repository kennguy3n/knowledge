package tenant

import (
	"context"
	"errors"
	"sort"
	"sync"
)

// ErrNotFound is returned when a tenant or member does not exist.
var ErrNotFound = errors.New("tenant: not found")

// ErrConflict is returned on a uniqueness violation (duplicate member).
var ErrConflict = errors.New("tenant: already exists")

// Store is the persistence boundary for the tenant service. The
// Postgres implementation uses parameterised queries exclusively; the
// in-memory implementation backs unit tests.
type Store interface {
	// CreateTenant persists a new tenant.
	CreateTenant(ctx context.Context, t Tenant) error
	// GetTenant loads a tenant by id, or [ErrNotFound].
	GetTenant(ctx context.Context, id string) (Tenant, error)
	// ListTenants returns all tenants ordered by creation time.
	ListTenants(ctx context.Context) ([]Tenant, error)
	// UpdateTenant overwrites a tenant's config and key.
	UpdateTenant(ctx context.Context, t Tenant) error
	// DeleteTenant removes a tenant and its members.
	DeleteTenant(ctx context.Context, id string) error

	// UpsertMember inserts or updates a member.
	UpsertMember(ctx context.Context, m Member) error
	// GetMember loads a member, or [ErrNotFound].
	GetMember(ctx context.Context, tenantID, userID string) (Member, error)
	// ListMembers returns a tenant's members.
	ListMembers(ctx context.Context, tenantID string) ([]Member, error)
	// DeleteMember removes a member.
	DeleteMember(ctx context.Context, tenantID, userID string) error
}

// MemoryStore is a thread-safe in-memory [Store] for tests and local
// development.
type MemoryStore struct {
	mu      sync.RWMutex
	tenants map[string]Tenant
	members map[string]map[string]Member // tenantID -> userID -> member
}

// NewMemoryStore constructs an empty in-memory store.
func NewMemoryStore() *MemoryStore {
	return &MemoryStore{
		tenants: make(map[string]Tenant),
		members: make(map[string]map[string]Member),
	}
}

// CreateTenant implements [Store].
func (m *MemoryStore) CreateTenant(_ context.Context, t Tenant) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if _, ok := m.tenants[t.ID]; ok {
		return ErrConflict
	}
	m.tenants[t.ID] = t
	return nil
}

// GetTenant implements [Store].
func (m *MemoryStore) GetTenant(_ context.Context, id string) (Tenant, error) {
	m.mu.RLock()
	defer m.mu.RUnlock()
	t, ok := m.tenants[id]
	if !ok {
		return Tenant{}, ErrNotFound
	}
	return t, nil
}

// ListTenants implements [Store].
func (m *MemoryStore) ListTenants(_ context.Context) ([]Tenant, error) {
	m.mu.RLock()
	defer m.mu.RUnlock()
	out := make([]Tenant, 0, len(m.tenants))
	for _, t := range m.tenants {
		out = append(out, t)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].CreatedAt.Before(out[j].CreatedAt) })
	return out, nil
}

// UpdateTenant implements [Store].
func (m *MemoryStore) UpdateTenant(_ context.Context, t Tenant) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if _, ok := m.tenants[t.ID]; !ok {
		return ErrNotFound
	}
	m.tenants[t.ID] = t
	return nil
}

// DeleteTenant implements [Store].
func (m *MemoryStore) DeleteTenant(_ context.Context, id string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if _, ok := m.tenants[id]; !ok {
		return ErrNotFound
	}
	delete(m.tenants, id)
	delete(m.members, id)
	return nil
}

// UpsertMember implements [Store].
func (m *MemoryStore) UpsertMember(_ context.Context, mem Member) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if _, ok := m.tenants[mem.TenantID]; !ok {
		return ErrNotFound
	}
	if m.members[mem.TenantID] == nil {
		m.members[mem.TenantID] = make(map[string]Member)
	}
	m.members[mem.TenantID][mem.UserID] = mem
	return nil
}

// GetMember implements [Store].
func (m *MemoryStore) GetMember(_ context.Context, tenantID, userID string) (Member, error) {
	m.mu.RLock()
	defer m.mu.RUnlock()
	byUser, ok := m.members[tenantID]
	if !ok {
		return Member{}, ErrNotFound
	}
	mem, ok := byUser[userID]
	if !ok {
		return Member{}, ErrNotFound
	}
	return mem, nil
}

// ListMembers implements [Store].
func (m *MemoryStore) ListMembers(_ context.Context, tenantID string) ([]Member, error) {
	m.mu.RLock()
	defer m.mu.RUnlock()
	byUser := m.members[tenantID]
	out := make([]Member, 0, len(byUser))
	for _, mem := range byUser {
		out = append(out, mem)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].UserID < out[j].UserID })
	return out, nil
}

// DeleteMember implements [Store].
func (m *MemoryStore) DeleteMember(_ context.Context, tenantID, userID string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	byUser, ok := m.members[tenantID]
	if !ok {
		return ErrNotFound
	}
	if _, ok := byUser[userID]; !ok {
		return ErrNotFound
	}
	delete(byUser, userID)
	return nil
}
