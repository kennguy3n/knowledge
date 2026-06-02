// Package connector manages connector lifecycle, OAuth2 flows,
// sync scheduling, and content pipeline via the substrate server.
package connector

import (
	"context"
	"fmt"
	"sync"
	"time"

	"github.com/google/uuid"
	"go.uber.org/zap"

	"github.com/kennguy3n/knowledge/server/internal/substrate"
)

// ConnectorKind enumerates supported connector types.
type ConnectorKind string

const (
	KindSlack        ConnectorKind = "Slack"
	KindEmail        ConnectorKind = "Email"
	KindNotion       ConnectorKind = "Notion"
	KindGoogleDrive  ConnectorKind = "GoogleDrive"
	KindOneDrive     ConnectorKind = "OneDrive"
	KindJira         ConnectorKind = "Jira"
	KindConfluence   ConnectorKind = "Confluence"
	KindFigma        ConnectorKind = "Figma"
	KindHubSpot      ConnectorKind = "HubSpot"
	KindGitHub       ConnectorKind = "GitHub"
)

// Instance represents a configured connector instance.
type Instance struct {
	ID           string        `json:"id"`
	TenantID     string        `json:"tenant_id"`
	ScopeID      string        `json:"scope_id"`
	Kind         ConnectorKind `json:"kind"`
	Name         string        `json:"name"`
	Authenticated bool         `json:"authenticated"`
	SyncInterval time.Duration `json:"sync_interval"`
	LastSyncAt   *time.Time    `json:"last_sync_at,omitempty"`
	Status       string        `json:"status"`
	CreatedAt    time.Time     `json:"created_at"`
}

// CreateRequest is the payload for creating a connector.
type CreateRequest struct {
	TenantID     string        `json:"tenant_id"`
	ScopeID      string        `json:"scope_id"`
	Kind         ConnectorKind `json:"kind"`
	Name         string        `json:"name"`
	SyncInterval *string       `json:"sync_interval,omitempty"` // e.g. "15m"
}

// OAuthStartResponse contains the redirect URL for OAuth2 flow initiation.
type OAuthStartResponse struct {
	RedirectURL string `json:"redirect_url"`
}

// SyncResponse contains the result of a sync operation.
type SyncResponse struct {
	ItemsSynced int    `json:"items_synced"`
	Status      string `json:"status"`
}

// StatusResponse contains connector health information.
type StatusResponse struct {
	ID            string     `json:"id"`
	Status        string     `json:"status"`
	Authenticated bool       `json:"authenticated"`
	LastSyncAt    *time.Time `json:"last_sync_at,omitempty"`
	ErrorMessage  *string    `json:"error_message,omitempty"`
}

// Service manages connector instances.
type Service struct {
	substrate *substrate.Client
	logger    *zap.Logger

	mu        sync.RWMutex
	instances map[string]*Instance

	// Sync scheduler
	stopCh    chan struct{}
	stopped   chan struct{}
}

// NewService creates a connector service.
func NewService(sub *substrate.Client, logger *zap.Logger) *Service {
	return &Service{
		substrate: sub,
		logger:    logger,
		instances: make(map[string]*Instance),
		stopCh:    make(chan struct{}),
		stopped:   make(chan struct{}),
	}
}

// Create creates a new connector instance.
func (s *Service) Create(ctx context.Context, req *CreateRequest) (*Instance, error) {
	interval := 15 * time.Minute
	if req.SyncInterval != nil {
		d, err := time.ParseDuration(*req.SyncInterval)
		if err != nil {
			return nil, fmt.Errorf("invalid sync_interval: %w", err)
		}
		interval = d
	}

	inst := &Instance{
		ID:           uuid.New().String(),
		TenantID:     req.TenantID,
		ScopeID:      req.ScopeID,
		Kind:         req.Kind,
		Name:         req.Name,
		SyncInterval: interval,
		Status:       "created",
		CreatedAt:    time.Now().UTC(),
	}

	s.mu.Lock()
	s.instances[inst.ID] = inst
	s.mu.Unlock()

	s.logger.Info("connector created",
		zap.String("id", inst.ID),
		zap.String("kind", string(inst.Kind)),
	)
	return inst, nil
}

// List returns all connector instances for a tenant.
func (s *Service) List(_ context.Context, tenantID string) []*Instance {
	s.mu.RLock()
	defer s.mu.RUnlock()

	var result []*Instance
	for _, inst := range s.instances {
		if tenantID == "" || inst.TenantID == tenantID {
			result = append(result, inst)
		}
	}
	return result
}

// Get returns a single connector instance.
func (s *Service) Get(id string) (*Instance, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()

	inst, ok := s.instances[id]
	if !ok {
		return nil, fmt.Errorf("connector %s not found", id)
	}
	return inst, nil
}

// Authenticate initiates OAuth2 flow for a connector.
func (s *Service) Authenticate(_ context.Context, id string) (*OAuthStartResponse, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	inst, ok := s.instances[id]
	if !ok {
		return nil, fmt.Errorf("connector %s not found", id)
	}

	// In production this would generate a real OAuth2 redirect URL
	// using the connector_framework's OAuth2Client.
	redirectURL := fmt.Sprintf(
		"https://auth.example.com/oauth2/authorize?client_id=%s&state=%s",
		string(inst.Kind), inst.ID,
	)

	inst.Authenticated = true
	inst.Status = "authenticated"

	return &OAuthStartResponse{RedirectURL: redirectURL}, nil
}

// Sync triggers a sync operation for a connector.
func (s *Service) Sync(ctx context.Context, id string) (*SyncResponse, error) {
	s.mu.RLock()
	inst, ok := s.instances[id]
	s.mu.RUnlock()

	if !ok {
		return nil, fmt.Errorf("connector %s not found", id)
	}

	if !inst.Authenticated {
		return nil, fmt.Errorf("connector %s not authenticated", id)
	}

	// Ingest a sync marker via substrate.
	_, err := s.substrate.Ingest(ctx, &substrate.IngestRequest{
		ScopeID:    inst.ScopeID,
		Body:       fmt.Sprintf("[sync] %s connector %s synced at %s", inst.Kind, inst.ID, time.Now().UTC().Format(time.RFC3339)),
		Source:     string(inst.Kind),
		Importance: "Important",
	})
	if err != nil {
		s.logger.Error("sync ingest failed", zap.String("id", id), zap.Error(err))
	}

	now := time.Now().UTC()
	s.mu.Lock()
	inst.LastSyncAt = &now
	inst.Status = "synced"
	s.mu.Unlock()

	return &SyncResponse{
		ItemsSynced: 1,
		Status:      "completed",
	}, nil
}

// Remove deletes a connector instance.
func (s *Service) Remove(_ context.Context, id string) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	if _, ok := s.instances[id]; !ok {
		return fmt.Errorf("connector %s not found", id)
	}
	delete(s.instances, id)

	s.logger.Info("connector removed", zap.String("id", id))
	return nil
}

// GetStatus returns health information for a connector.
func (s *Service) GetStatus(id string) (*StatusResponse, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()

	inst, ok := s.instances[id]
	if !ok {
		return nil, fmt.Errorf("connector %s not found", id)
	}

	return &StatusResponse{
		ID:            inst.ID,
		Status:        inst.Status,
		Authenticated: inst.Authenticated,
		LastSyncAt:    inst.LastSyncAt,
	}, nil
}

// StartScheduler starts the background sync scheduler.
func (s *Service) StartScheduler() {
	go func() {
		defer close(s.stopped)
		ticker := time.NewTicker(time.Minute)
		defer ticker.Stop()

		for {
			select {
			case <-s.stopCh:
				return
			case <-ticker.C:
				s.runScheduledSyncs()
			}
		}
	}()
}

// StopScheduler stops the background sync scheduler.
func (s *Service) StopScheduler() {
	close(s.stopCh)
	<-s.stopped
}

func (s *Service) runScheduledSyncs() {
	s.mu.RLock()
	var due []*Instance
	now := time.Now()
	for _, inst := range s.instances {
		if !inst.Authenticated {
			continue
		}
		if inst.LastSyncAt == nil || now.Sub(*inst.LastSyncAt) >= inst.SyncInterval {
			due = append(due, inst)
		}
	}
	s.mu.RUnlock()

	for _, inst := range due {
		ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
		if _, err := s.Sync(ctx, inst.ID); err != nil {
			s.logger.Error("scheduled sync failed",
				zap.String("id", inst.ID),
				zap.Error(err),
			)
		}
		cancel()
	}
}
