// Package audit provides NATS JetStream-backed audit event persistence
// with PostgreSQL storage and query API.
package audit

import (
	"context"
	"fmt"
	"sync"
	"time"

	"github.com/google/uuid"
	"go.uber.org/zap"
)

// Event represents an audit log entry.
type Event struct {
	ID        string    `json:"id"`
	TenantID  string    `json:"tenant_id"`
	ScopeID   string    `json:"scope_id,omitempty"`
	Action    string    `json:"action"`
	ActorID   string    `json:"actor_id"`
	Details   string    `json:"details,omitempty"`
	CreatedAt time.Time `json:"created_at"`
}

// QueryParams filters audit log queries.
type QueryParams struct {
	TenantID string     `json:"tenant_id,omitempty"`
	ScopeID  string     `json:"scope_id,omitempty"`
	Action   string     `json:"action,omitempty"`
	ActorID  string     `json:"actor_id,omitempty"`
	Since    *time.Time `json:"since,omitempty"`
	Until    *time.Time `json:"until,omitempty"`
	Limit    int        `json:"limit,omitempty"`
}

// RetentionPolicy defines per-tenant retention rules.
type RetentionPolicy struct {
	TenantID      string `json:"tenant_id"`
	RetentionDays int    `json:"retention_days"`
}

// Service manages audit events. In production this would consume
// from NATS JetStream and persist to PostgreSQL. This implementation
// uses an in-memory store.
type Service struct {
	logger *zap.Logger

	mu     sync.RWMutex
	events []*Event

	retentionMu sync.RWMutex
	retention   map[string]*RetentionPolicy

	stopCh  chan struct{}
	stopped chan struct{}
}

// NewService creates an audit service.
func NewService(logger *zap.Logger) *Service {
	return &Service{
		logger:    logger,
		events:    make([]*Event, 0),
		retention: make(map[string]*RetentionPolicy),
		stopCh:    make(chan struct{}),
		stopped:   make(chan struct{}),
	}
}

// Record persists an audit event.
func (s *Service) Record(_ context.Context, evt *Event) error {
	if evt.ID == "" {
		evt.ID = uuid.New().String()
	}
	if evt.CreatedAt.IsZero() {
		evt.CreatedAt = time.Now().UTC()
	}

	s.mu.Lock()
	s.events = append(s.events, evt)
	s.mu.Unlock()

	s.logger.Debug("audit event recorded",
		zap.String("id", evt.ID),
		zap.String("action", evt.Action),
		zap.String("actor_id", evt.ActorID),
	)
	return nil
}

// Query returns audit events matching the given parameters.
func (s *Service) Query(_ context.Context, params *QueryParams) ([]*Event, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()

	limit := params.Limit
	if limit <= 0 {
		limit = 100
	}

	var results []*Event
	for i := len(s.events) - 1; i >= 0 && len(results) < limit; i-- {
		evt := s.events[i]

		if params.TenantID != "" && evt.TenantID != params.TenantID {
			continue
		}
		if params.ScopeID != "" && evt.ScopeID != params.ScopeID {
			continue
		}
		if params.Action != "" && evt.Action != params.Action {
			continue
		}
		if params.ActorID != "" && evt.ActorID != params.ActorID {
			continue
		}
		if params.Since != nil && evt.CreatedAt.Before(*params.Since) {
			continue
		}
		if params.Until != nil && evt.CreatedAt.After(*params.Until) {
			continue
		}

		results = append(results, evt)
	}

	return results, nil
}

// SetRetentionPolicy sets the retention policy for a tenant.
func (s *Service) SetRetentionPolicy(_ context.Context, policy *RetentionPolicy) error {
	if policy.RetentionDays <= 0 {
		return fmt.Errorf("retention_days must be positive")
	}

	s.retentionMu.Lock()
	s.retention[policy.TenantID] = policy
	s.retentionMu.Unlock()

	s.logger.Info("retention policy set",
		zap.String("tenant_id", policy.TenantID),
		zap.Int("retention_days", policy.RetentionDays),
	)
	return nil
}

// StartRetentionEnforcer starts a background goroutine that enforces
// retention policies.
func (s *Service) StartRetentionEnforcer() {
	go func() {
		defer close(s.stopped)
		ticker := time.NewTicker(time.Hour)
		defer ticker.Stop()

		for {
			select {
			case <-s.stopCh:
				return
			case <-ticker.C:
				s.enforceRetention()
			}
		}
	}()
}

// StopRetentionEnforcer stops the background retention enforcer.
func (s *Service) StopRetentionEnforcer() {
	close(s.stopCh)
	<-s.stopped
}

func (s *Service) enforceRetention() {
	s.retentionMu.RLock()
	policies := make(map[string]int)
	for tid, p := range s.retention {
		policies[tid] = p.RetentionDays
	}
	s.retentionMu.RUnlock()

	if len(policies) == 0 {
		return
	}

	now := time.Now().UTC()
	s.mu.Lock()
	defer s.mu.Unlock()

	kept := make([]*Event, 0, len(s.events))
	removed := 0
	for _, evt := range s.events {
		days, ok := policies[evt.TenantID]
		if ok && now.Sub(evt.CreatedAt) > time.Duration(days)*24*time.Hour {
			removed++
			continue
		}
		kept = append(kept, evt)
	}
	s.events = kept

	if removed > 0 {
		s.logger.Info("retention enforcement completed", zap.Int("removed", removed))
	}
}
