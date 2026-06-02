// Package tenant provides tenant CRUD with PostgreSQL (pgx),
// per-tenant config storage, member provisioning, and encryption
// key management via the substrate server.
package tenant

import (
	"context"
	"fmt"
	"sync"
	"time"

	"github.com/google/uuid"
	"go.uber.org/zap"

	"github.com/kennguy3n/knowledge/server/internal/substrate"
)

// Tenant represents a platform tenant.
type Tenant struct {
	ID              string          `json:"id"`
	Name            string          `json:"name"`
	Config          TenantConfig    `json:"config"`
	Members         map[string]*Member `json:"members,omitempty"`
	EncryptionKeyID *string         `json:"encryption_key_id,omitempty"`
	CreatedAt       time.Time       `json:"created_at"`
	UpdatedAt       time.Time       `json:"updated_at"`
}

// TenantConfig holds per-tenant settings.
type TenantConfig struct {
	ConnectorLimit  int    `json:"connector_limit"`
	SynthesisTier   string `json:"synthesis_tier"`
	RetentionDays   int    `json:"retention_days"`
}

// Member represents a tenant member.
type Member struct {
	ID       string `json:"id"`
	Email    string `json:"email"`
	Role     string `json:"role"`
	Status   string `json:"status"` // invited, active, suspended, removed
	JoinedAt time.Time `json:"joined_at"`
}

// CreateRequest is the payload for creating a tenant.
type CreateRequest struct {
	Name           string `json:"name"`
	ConnectorLimit *int   `json:"connector_limit,omitempty"`
	SynthesisTier  *string `json:"synthesis_tier,omitempty"`
	RetentionDays  *int   `json:"retention_days,omitempty"`
}

// InviteRequest is the payload for inviting a member.
type InviteRequest struct {
	Email string `json:"email"`
	Role  string `json:"role"`
}

// Service manages tenants. In production this would use pgx for
// PostgreSQL persistence; this implementation uses an in-memory
// store that can be swapped for a pgx-backed one.
type Service struct {
	substrate *substrate.Client
	logger    *zap.Logger

	mu      sync.RWMutex
	tenants map[string]*Tenant
}

// NewService creates a tenant service.
func NewService(sub *substrate.Client, logger *zap.Logger) *Service {
	return &Service{
		substrate: sub,
		logger:    logger,
		tenants:   make(map[string]*Tenant),
	}
}

// Create creates a new tenant and provisions an encryption keypair.
func (s *Service) Create(ctx context.Context, req *CreateRequest) (*Tenant, error) {
	if req.Name == "" {
		return nil, fmt.Errorf("tenant name is required")
	}

	cfg := TenantConfig{
		ConnectorLimit: 10,
		SynthesisTier:  "standard",
		RetentionDays:  365,
	}
	if req.ConnectorLimit != nil {
		cfg.ConnectorLimit = *req.ConnectorLimit
	}
	if req.SynthesisTier != nil {
		cfg.SynthesisTier = *req.SynthesisTier
	}
	if req.RetentionDays != nil {
		cfg.RetentionDays = *req.RetentionDays
	}

	now := time.Now().UTC()
	t := &Tenant{
		ID:        uuid.New().String(),
		Name:      req.Name,
		Config:    cfg,
		Members:   make(map[string]*Member),
		CreatedAt: now,
		UpdatedAt: now,
	}

	// Provision encryption keypair via substrate.
	kp, err := s.substrate.GenerateKeypair(ctx)
	if err != nil {
		s.logger.Warn("failed to generate tenant keypair", zap.Error(err))
	} else {
		keyID := kp.Algorithm + ":" + t.ID[:8]
		t.EncryptionKeyID = &keyID
	}

	s.mu.Lock()
	s.tenants[t.ID] = t
	s.mu.Unlock()

	s.logger.Info("tenant created", zap.String("id", t.ID), zap.String("name", t.Name))
	return t, nil
}

// Get retrieves a tenant by ID.
func (s *Service) Get(_ context.Context, id string) (*Tenant, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()

	t, ok := s.tenants[id]
	if !ok {
		return nil, fmt.Errorf("tenant %s not found", id)
	}
	return t, nil
}

// InviteMember invites a new member to a tenant.
func (s *Service) InviteMember(_ context.Context, tenantID string, req *InviteRequest) (*Member, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	t, ok := s.tenants[tenantID]
	if !ok {
		return nil, fmt.Errorf("tenant %s not found", tenantID)
	}

	m := &Member{
		ID:       uuid.New().String(),
		Email:    req.Email,
		Role:     req.Role,
		Status:   "invited",
		JoinedAt: time.Now().UTC(),
	}
	t.Members[m.ID] = m
	t.UpdatedAt = time.Now().UTC()

	s.logger.Info("member invited",
		zap.String("tenant_id", tenantID),
		zap.String("member_id", m.ID),
	)
	return m, nil
}

// ActivateMember activates an invited member.
func (s *Service) ActivateMember(_ context.Context, tenantID, memberID string) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	t, ok := s.tenants[tenantID]
	if !ok {
		return fmt.Errorf("tenant %s not found", tenantID)
	}
	m, ok := t.Members[memberID]
	if !ok {
		return fmt.Errorf("member %s not found", memberID)
	}
	m.Status = "active"
	t.UpdatedAt = time.Now().UTC()
	return nil
}

// SuspendMember suspends a member.
func (s *Service) SuspendMember(_ context.Context, tenantID, memberID string) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	t, ok := s.tenants[tenantID]
	if !ok {
		return fmt.Errorf("tenant %s not found", tenantID)
	}
	m, ok := t.Members[memberID]
	if !ok {
		return fmt.Errorf("member %s not found", memberID)
	}
	m.Status = "suspended"
	t.UpdatedAt = time.Now().UTC()
	return nil
}

// RemoveMember removes a member from a tenant.
func (s *Service) RemoveMember(_ context.Context, tenantID, memberID string) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	t, ok := s.tenants[tenantID]
	if !ok {
		return fmt.Errorf("tenant %s not found", tenantID)
	}
	delete(t.Members, memberID)
	t.UpdatedAt = time.Now().UTC()
	return nil
}
