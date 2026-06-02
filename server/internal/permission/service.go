// Package permission provides a Zanzibar-style permission service
// wrapping the substrate's permission_service crate via HTTP.
package permission

import (
	"context"
	"fmt"
	"net/http"
	"sync"

	"go.uber.org/zap"
)

// Tuple represents a (user, relation, object) permission tuple.
type Tuple struct {
	User     string `json:"user"`
	Relation string `json:"relation"`
	Object   string `json:"object"`
}

// CheckRequest asks whether a tuple exists.
type CheckRequest struct {
	User     string `json:"user"`
	Relation string `json:"relation"`
	Object   string `json:"object"`
}

// CheckResponse is the result of a permission check.
type CheckResponse struct {
	Allowed bool `json:"allowed"`
}

// SCIMUser represents a SCIM v2 user resource.
type SCIMUser struct {
	ID          string `json:"id"`
	UserName    string `json:"userName"`
	DisplayName string `json:"displayName"`
	Active      bool   `json:"active"`
	ExternalID  string `json:"externalId,omitempty"`
}

// SCIMGroup represents a SCIM v2 group resource.
type SCIMGroup struct {
	ID          string   `json:"id"`
	DisplayName string   `json:"displayName"`
	Members     []string `json:"members,omitempty"`
	ExternalID  string   `json:"externalId,omitempty"`
}

// Service manages permission tuples and SCIM provisioning.
type Service struct {
	logger *zap.Logger

	mu     sync.RWMutex
	tuples map[string]map[string]map[string]bool // object -> relation -> user -> exists

	userMu sync.RWMutex
	users  map[string]*SCIMUser

	groupMu sync.RWMutex
	groups  map[string]*SCIMGroup
}

// NewService creates a permission service.
func NewService(logger *zap.Logger) *Service {
	return &Service{
		logger: logger,
		tuples: make(map[string]map[string]map[string]bool),
		users:  make(map[string]*SCIMUser),
		groups: make(map[string]*SCIMGroup),
	}
}

// Grant adds a permission tuple.
func (s *Service) Grant(_ context.Context, t *Tuple) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	if _, ok := s.tuples[t.Object]; !ok {
		s.tuples[t.Object] = make(map[string]map[string]bool)
	}
	if _, ok := s.tuples[t.Object][t.Relation]; !ok {
		s.tuples[t.Object][t.Relation] = make(map[string]bool)
	}
	s.tuples[t.Object][t.Relation][t.User] = true

	s.logger.Debug("tuple granted",
		zap.String("user", t.User),
		zap.String("relation", t.Relation),
		zap.String("object", t.Object),
	)
	return nil
}

// Revoke removes a permission tuple.
func (s *Service) Revoke(_ context.Context, t *Tuple) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	if rels, ok := s.tuples[t.Object]; ok {
		if users, ok := rels[t.Relation]; ok {
			delete(users, t.User)
		}
	}

	s.logger.Debug("tuple revoked",
		zap.String("user", t.User),
		zap.String("relation", t.Relation),
		zap.String("object", t.Object),
	)
	return nil
}

// Check verifies whether a tuple exists.
func (s *Service) Check(_ context.Context, req *CheckRequest) *CheckResponse {
	s.mu.RLock()
	defer s.mu.RUnlock()

	if rels, ok := s.tuples[req.Object]; ok {
		if users, ok := rels[req.Relation]; ok {
			if users[req.User] {
				return &CheckResponse{Allowed: true}
			}
		}
	}
	return &CheckResponse{Allowed: false}
}

// Middleware returns an HTTP middleware that checks (actor, relation, object)
// tuples. The object is derived from the request path and the relation
// is configurable.
func (s *Service) Middleware(relation string) func(http.Handler) http.Handler {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			// Permission middleware is opt-in; skip if no actor context.
			actor := r.Context().Value("actor_id")
			if actor == nil {
				next.ServeHTTP(w, r)
				return
			}

			actorStr, _ := actor.(string)
			object := r.URL.Path

			resp := s.Check(r.Context(), &CheckRequest{
				User:     actorStr,
				Relation: relation,
				Object:   object,
			})

			if !resp.Allowed {
				http.Error(w, `{"error":"forbidden"}`, http.StatusForbidden)
				return
			}
			next.ServeHTTP(w, r)
		})
	}
}

// ---------------------------------------------------------------------------
// SCIM v2 user operations
// ---------------------------------------------------------------------------

// CreateUser provisions a SCIM user.
func (s *Service) CreateUser(_ context.Context, u *SCIMUser) (*SCIMUser, error) {
	s.userMu.Lock()
	defer s.userMu.Unlock()

	if u.ID == "" {
		u.ID = fmt.Sprintf("scim-user-%d", len(s.users)+1)
	}
	s.users[u.ID] = u
	return u, nil
}

// GetUser retrieves a SCIM user by ID.
func (s *Service) GetUser(_ context.Context, id string) (*SCIMUser, error) {
	s.userMu.RLock()
	defer s.userMu.RUnlock()

	u, ok := s.users[id]
	if !ok {
		return nil, fmt.Errorf("SCIM user %s not found", id)
	}
	return u, nil
}

// ListUsers returns all SCIM users.
func (s *Service) ListUsers(_ context.Context) []*SCIMUser {
	s.userMu.RLock()
	defer s.userMu.RUnlock()

	var result []*SCIMUser
	for _, u := range s.users {
		result = append(result, u)
	}
	return result
}

// ---------------------------------------------------------------------------
// SCIM v2 group operations
// ---------------------------------------------------------------------------

// CreateGroup provisions a SCIM group.
func (s *Service) CreateGroup(_ context.Context, g *SCIMGroup) (*SCIMGroup, error) {
	s.groupMu.Lock()
	defer s.groupMu.Unlock()

	if g.ID == "" {
		g.ID = fmt.Sprintf("scim-group-%d", len(s.groups)+1)
	}
	s.groups[g.ID] = g
	return g, nil
}

// GetGroup retrieves a SCIM group by ID.
func (s *Service) GetGroup(_ context.Context, id string) (*SCIMGroup, error) {
	s.groupMu.RLock()
	defer s.groupMu.RUnlock()

	g, ok := s.groups[id]
	if !ok {
		return nil, fmt.Errorf("SCIM group %s not found", id)
	}
	return g, nil
}

// ListGroups returns all SCIM groups.
func (s *Service) ListGroups(_ context.Context) []*SCIMGroup {
	s.groupMu.RLock()
	defer s.groupMu.RUnlock()

	var result []*SCIMGroup
	for _, g := range s.groups {
		result = append(result, g)
	}
	return result
}
