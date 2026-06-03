// Package permission is the Go permission service: a thin wrapper over
// the Rust permission_service (reached via substrate_server) plus a
// gateway middleware hook and SCIM v2 user/group provisioning.
package permission

import (
	"context"
	"net/http"

	"github.com/go-chi/chi/v5"
	"go.uber.org/zap"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
	"github.com/kennguy3n/knowledge/server/internal/middleware"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
	"github.com/kennguy3n/knowledge/server/internal/validate"
)

// checker is the subset of [substrate.Client] the service needs;
// narrowing it lets unit tests inject a fake without a live loopback.
type checker interface {
	PermissionGrant(ctx context.Context, t substrate.RelationTuple) error
	PermissionRevoke(ctx context.Context, t substrate.RelationTuple) error
	PermissionCheck(ctx context.Context, t substrate.RelationTuple) (bool, error)
}

// Service implements tuple grant/revoke/check and SCIM provisioning.
type Service struct {
	sub      checker
	dir      *directory
	dirStore DirectoryStore
	log      *zap.Logger
}

// New constructs a permission Service over the given substrate client.
// The SCIM directory defaults to the non-durable noop store; supply a
// durable backend with [Service.WithDirectoryStore] to survive restarts.
func New(sub checker) *Service {
	return &Service{sub: sub, dir: newDirectory(), dirStore: NewNoopDirectoryStore(), log: zap.NewNop()}
}

// WithDirectoryStore sets the durable SCIM directory backend and returns
// the service for chaining. A nil store is ignored (the noop store is
// kept). Pair with [Service.Rehydrate] at startup to restore the
// directory persisted by a prior process.
func (s *Service) WithDirectoryStore(ds DirectoryStore) *Service {
	if ds != nil {
		s.dirStore = ds
	}
	return s
}

// Rehydrate reloads the SCIM directory from the durable store into the
// in-memory cache. It is called once at startup, before serving traffic,
// so users and groups provisioned by a prior process survive a restart
// and stay in lock-step with the substrate membership tuples. The cache
// is cleared first so the reload is a faithful replica of the store
// (and so a repeat call cannot leave behind entries deleted upstream).
func (s *Service) Rehydrate(ctx context.Context) error {
	users, err := s.dirStore.ListUsers(ctx)
	if err != nil {
		return err
	}
	groups, err := s.dirStore.ListGroups(ctx)
	if err != nil {
		return err
	}
	s.dir.mu.Lock()
	clear(s.dir.users)
	clear(s.dir.groups)
	for _, u := range users {
		s.dir.users[u.ID] = u
	}
	for _, g := range groups {
		s.dir.groups[g.ID] = g
	}
	s.dir.mu.Unlock()
	s.log.Info("scim directory rehydrated",
		zap.Int("users", len(users)), zap.Int("groups", len(groups)))
	return nil
}

// WithLogger sets the logger used for best-effort diagnostics (e.g.
// SCIM tuple-reconciliation rollback failures) and returns the service
// for chaining. A nil logger is ignored.
func (s *Service) WithLogger(l *zap.Logger) *Service {
	if l != nil {
		s.log = l
	}
	return s
}

// Routes returns a chi router exposing the tuple grant/revoke/check
// surface.
func (s *Service) Routes() http.Handler {
	r := chi.NewRouter()
	r.Post("/grant", s.handleGrant)
	r.Post("/revoke", s.handleRevoke)
	r.Post("/check", s.handleCheck)
	return r
}

// SCIMRoutes returns the SCIM v2 user/group provisioning router,
// intended to be mounted at /scim/v2.
func (s *Service) SCIMRoutes() http.Handler {
	return s.scimRoutes()
}

// Grant idempotently inserts a relation tuple.
func (s *Service) Grant(ctx context.Context, t substrate.RelationTuple) error {
	if err := validateTuple(t); err != nil {
		return err
	}
	return s.sub.PermissionGrant(ctx, t)
}

// Revoke removes a relation tuple.
func (s *Service) Revoke(ctx context.Context, t substrate.RelationTuple) error {
	if err := validateTuple(t); err != nil {
		return err
	}
	return s.sub.PermissionRevoke(ctx, t)
}

// Check evaluates whether the subject has the relation on the object.
func (s *Service) Check(ctx context.Context, t substrate.RelationTuple) (bool, error) {
	if err := validateTuple(t); err != nil {
		return false, err
	}
	return s.sub.PermissionCheck(ctx, t)
}

func (s *Service) handleGrant(w http.ResponseWriter, r *http.Request) {
	var t substrate.RelationTuple
	if err := httpx.DecodeJSON(r, &t); err != nil {
		httpx.WriteError(w, err)
		return
	}
	if err := s.Grant(r.Context(), t); err != nil {
		httpx.WriteError(w, err)
		return
	}
	httpx.WriteJSON(w, http.StatusCreated, map[string]bool{"granted": true})
}

func (s *Service) handleRevoke(w http.ResponseWriter, r *http.Request) {
	var t substrate.RelationTuple
	if err := httpx.DecodeJSON(r, &t); err != nil {
		httpx.WriteError(w, err)
		return
	}
	if err := s.Revoke(r.Context(), t); err != nil {
		httpx.WriteError(w, err)
		return
	}
	httpx.WriteJSON(w, http.StatusOK, map[string]bool{"revoked": true})
}

func (s *Service) handleCheck(w http.ResponseWriter, r *http.Request) {
	var t substrate.RelationTuple
	if err := httpx.DecodeJSON(r, &t); err != nil {
		httpx.WriteError(w, err)
		return
	}
	allowed, err := s.Check(r.Context(), t)
	if err != nil {
		httpx.WriteError(w, err)
		return
	}
	httpx.WriteJSON(w, http.StatusOK, substrate.PermissionCheckResponse{Allowed: allowed})
}

// ObjectExtractor derives the protected object's id from a request
// (e.g. a URL param or the authenticated tenant).
type ObjectExtractor func(r *http.Request) (objectType, objectID string, ok bool)

// RequireRelation returns middleware that authorises the request by
// checking that the authenticated principal holds relation on the
// object produced by extract. The service principal bypasses the
// check. A missing object or principal yields 403.
func (s *Service) RequireRelation(relation string, extract ObjectExtractor) func(http.Handler) http.Handler {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			p, ok := middleware.PrincipalFrom(r.Context())
			if ok && p.Service {
				next.ServeHTTP(w, r)
				return
			}
			if !ok || p.Subject == "" {
				httpx.WriteError(w, httpx.Forbidden("no authenticated principal"))
				return
			}
			objType, objID, ok := extract(r)
			if !ok {
				httpx.WriteError(w, httpx.Forbidden("could not resolve protected object"))
				return
			}
			tuple := substrate.RelationTuple{
				Object:   substrate.ObjectRef{ObjectType: objType, ObjectID: objID},
				Relation: relation,
				Subject:  substrate.SubjectRef{SubjectType: "user", SubjectID: p.Subject},
			}
			allowed, err := s.sub.PermissionCheck(r.Context(), tuple)
			if err != nil {
				httpx.WriteError(w, err)
				return
			}
			if !allowed {
				httpx.WriteError(w, httpx.Forbidden("permission denied"))
				return
			}
			next.ServeHTTP(w, r)
		})
	}
}

// validateTuple enforces UUID object/subject ids and non-empty
// relation/type tags before any loopback round-trip.
func validateTuple(t substrate.RelationTuple) error {
	if _, err := validate.ScopeID(t.Object.ObjectID); err != nil {
		return httpx.BadRequest("object_id must be a UUID")
	}
	if _, err := validate.ScopeID(t.Subject.SubjectID); err != nil {
		return httpx.BadRequest("subject_id must be a UUID")
	}
	if t.Object.ObjectType == "" || t.Subject.SubjectType == "" || t.Relation == "" {
		return httpx.BadRequest("object_type, subject_type and relation are required")
	}
	return nil
}
