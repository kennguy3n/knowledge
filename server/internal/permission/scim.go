package permission

import (
	"net/http"
	"sync"
	"time"

	"github.com/go-chi/chi/v5"
	"github.com/google/uuid"
	"go.uber.org/zap"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
)

// SCIM v2 schema URNs (RFC 7643).
const (
	schemaUser      = "urn:ietf:params:scim:schemas:core:2.0:User"
	schemaGroup     = "urn:ietf:params:scim:schemas:core:2.0:Group"
	schemaListResp  = "urn:ietf:params:scim:api:messages:2.0:ListResponse"
	schemaErrorResp = "urn:ietf:params:scim:api:messages:2.0:Error"
)

// scimMeta is the common SCIM resource metadata block.
type scimMeta struct {
	ResourceType string    `json:"resourceType"`
	Created      time.Time `json:"created"`
	LastModified time.Time `json:"lastModified"`
}

// User is a SCIM v2 User resource (pragmatic subset).
type User struct {
	Schemas  []string `json:"schemas"`
	ID       string   `json:"id"`
	UserName string   `json:"userName"`
	Active   bool     `json:"active"`
	Emails   []email  `json:"emails,omitempty"`
	Meta     scimMeta `json:"meta"`
}

type email struct {
	Value   string `json:"value"`
	Primary bool   `json:"primary,omitempty"`
}

// Group is a SCIM v2 Group resource (pragmatic subset).
type Group struct {
	Schemas     []string      `json:"schemas"`
	ID          string        `json:"id"`
	DisplayName string        `json:"displayName"`
	Members     []groupMember `json:"members,omitempty"`
	Meta        scimMeta      `json:"meta"`
}

type groupMember struct {
	Value   string `json:"value"`
	Display string `json:"display,omitempty"`
}

// listResponse is the SCIM ListResponse envelope.
type listResponse struct {
	Schemas      []string `json:"schemas"`
	TotalResults int      `json:"totalResults"`
	Resources    []any    `json:"Resources"`
}

// directory is the in-memory SCIM cache. Identity provisioning is kept
// distinct from authorization tuples (which live in the substrate). It is
// kept durable by a [DirectoryStore]: every write handler persists
// through the store before committing this cache (write-through), and the
// cache is rehydrated from the store at startup, so the directory and the
// substrate tuples stay in lock-step across restarts.
type directory struct {
	mu     sync.RWMutex
	users  map[string]User
	groups map[string]Group
}

func newDirectory() *directory {
	return &directory{
		users:  make(map[string]User),
		groups: make(map[string]Group),
	}
}

func (s *Service) scimRoutes() http.Handler {
	r := chi.NewRouter()
	r.Post("/Users", s.scimCreateUser)
	r.Get("/Users", s.scimListUsers)
	r.Get("/Users/{id}", s.scimGetUser)
	r.Put("/Users/{id}", s.scimReplaceUser)
	r.Delete("/Users/{id}", s.scimDeleteUser)

	r.Post("/Groups", s.scimCreateGroup)
	r.Get("/Groups", s.scimListGroups)
	r.Get("/Groups/{id}", s.scimGetGroup)
	r.Put("/Groups/{id}", s.scimReplaceGroup)
	r.Delete("/Groups/{id}", s.scimDeleteGroup)
	return r
}

func scimError(w http.ResponseWriter, status int, detail string) {
	httpx.WriteJSON(w, status, map[string]any{
		"schemas": []string{schemaErrorResp},
		"detail":  detail,
		"status":  status,
	})
}

// ── Users ───────────────────────────────────────────────────────────

func (s *Service) scimCreateUser(w http.ResponseWriter, r *http.Request) {
	var in User
	if err := httpx.DecodeJSON(r, &in); err != nil {
		scimError(w, http.StatusBadRequest, "invalid SCIM User payload")
		return
	}
	if in.UserName == "" {
		scimError(w, http.StatusBadRequest, "userName is required")
		return
	}
	now := time.Now().UTC()
	u := User{
		Schemas:  []string{schemaUser},
		ID:       uuid.NewString(),
		UserName: in.UserName,
		Active:   true,
		Emails:   in.Emails,
		Meta:     scimMeta{ResourceType: "User", Created: now, LastModified: now},
	}
	s.dir.mu.Lock()
	for _, existing := range s.dir.users {
		if existing.UserName == in.UserName {
			s.dir.mu.Unlock()
			scimError(w, http.StatusConflict, "userName already exists")
			return
		}
	}
	// Persist through the durable store before committing the cache, so a
	// successful create survives a restart.
	if err := s.dirStore.SaveUser(r.Context(), u); err != nil {
		s.dir.mu.Unlock()
		s.log.Error("scim: persist user on create", zap.Error(err))
		scimError(w, http.StatusInternalServerError, "failed to persist user")
		return
	}
	s.dir.users[u.ID] = u
	s.dir.mu.Unlock()
	httpx.WriteJSON(w, http.StatusCreated, u)
}

func (s *Service) scimListUsers(w http.ResponseWriter, _ *http.Request) {
	s.dir.mu.RLock()
	res := make([]any, 0, len(s.dir.users))
	for _, u := range s.dir.users {
		res = append(res, u)
	}
	s.dir.mu.RUnlock()
	httpx.WriteJSON(w, http.StatusOK, listResponse{
		Schemas:      []string{schemaListResp},
		TotalResults: len(res),
		Resources:    res,
	})
}

func (s *Service) scimGetUser(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "id")
	s.dir.mu.RLock()
	u, ok := s.dir.users[id]
	s.dir.mu.RUnlock()
	if !ok {
		scimError(w, http.StatusNotFound, "user not found")
		return
	}
	httpx.WriteJSON(w, http.StatusOK, u)
}

func (s *Service) scimReplaceUser(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "id")
	var in User
	if err := httpx.DecodeJSON(r, &in); err != nil {
		scimError(w, http.StatusBadRequest, "invalid SCIM User payload")
		return
	}
	s.dir.mu.RLock()
	prev, ok := s.dir.users[id]
	s.dir.mu.RUnlock()
	if !ok {
		scimError(w, http.StatusNotFound, "user not found")
		return
	}
	next := prev
	if in.UserName != "" {
		next.UserName = in.UserName
	}
	next.Active = in.Active
	next.Emails = in.Emails
	next.Meta.LastModified = time.Now().UTC()
	// An active-state flip adds or removes this user's membership tuples
	// across every group it belongs to. Reconcile the substrate before
	// committing the directory change so the two never diverge.
	var toggleOps []tupleOp
	if prev.Active != next.Active {
		toggleOps = s.userActiveToggleOps(id, next.Active)
		if err := s.applyTupleOps(r.Context(), toggleOps); err != nil {
			scimError(w, http.StatusInternalServerError, "failed to reconcile membership tuples")
			return
		}
	}
	s.dir.mu.Lock()
	// Re-check existence under the write lock: a concurrent delete may have
	// removed the user while we reconciled tuples outside the lock. Writing
	// unconditionally would resurrect a deprovisioned user.
	if _, ok := s.dir.users[id]; !ok {
		s.dir.mu.Unlock()
		s.compensateGrants(r.Context(), toggleOps)
		scimError(w, http.StatusConflict, "user was deleted concurrently")
		return
	}
	if err := s.dirStore.SaveUser(r.Context(), next); err != nil {
		s.dir.mu.Unlock()
		// Persist failed: fully reverse the applied toggle so the substrate
		// matches the unchanged directory. rollbackTupleOps (not
		// compensateGrants) is required because a deactivation toggle is a
		// set of revokes, which compensateGrants would leave gone.
		s.rollbackTupleOps(r.Context(), toggleOps)
		s.log.Error("scim: persist user on replace", zap.Error(err))
		scimError(w, http.StatusInternalServerError, "failed to persist user")
		return
	}
	s.dir.users[id] = next
	s.dir.mu.Unlock()
	httpx.WriteJSON(w, http.StatusOK, next)
}

func (s *Service) scimDeleteUser(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "id")
	s.dir.mu.RLock()
	_, ok := s.dir.users[id]
	s.dir.mu.RUnlock()
	if !ok {
		scimError(w, http.StatusNotFound, "user not found")
		return
	}
	// Drop the user's membership tuples before deleting it, so a removed
	// user leaves no dangling group-derived authorization behind.
	removalOps := s.userRemovalOps(id)
	if err := s.applyTupleOps(r.Context(), removalOps); err != nil {
		scimError(w, http.StatusInternalServerError, "failed to reconcile membership tuples")
		return
	}
	now := time.Now().UTC()
	s.dir.mu.Lock()
	// Compute the groups the user is stripped from without mutating the
	// cache yet, so the durable delete + member-list rewrite commits as one
	// transaction before the cache is updated. Bump last_modified so the
	// timestamp reflects the membership change.
	var updated []Group
	for _, g := range s.dir.groups {
		if members, changed := removeMemberValue(g.Members, id); changed {
			g.Members = members
			g.Meta.LastModified = now
			updated = append(updated, g)
		}
	}
	if err := s.dirStore.DeleteUser(r.Context(), id, updated); err != nil {
		s.dir.mu.Unlock()
		// Persist failed: re-grant the membership tuples revoked above so the
		// substrate stays consistent with the still-present user.
		s.rollbackTupleOps(r.Context(), removalOps)
		s.log.Error("scim: persist user delete", zap.Error(err))
		scimError(w, http.StatusInternalServerError, "failed to persist user deletion")
		return
	}
	delete(s.dir.users, id)
	// Strip the deleted user from every group's member list to keep the
	// directory consistent with the now-removed tuples.
	for _, g := range updated {
		s.dir.groups[g.ID] = g
	}
	s.dir.mu.Unlock()
	w.WriteHeader(http.StatusNoContent)
}

// ── Groups ──────────────────────────────────────────────────────────

func (s *Service) scimCreateGroup(w http.ResponseWriter, r *http.Request) {
	var in Group
	if err := httpx.DecodeJSON(r, &in); err != nil {
		scimError(w, http.StatusBadRequest, "invalid SCIM Group payload")
		return
	}
	if in.DisplayName == "" {
		scimError(w, http.StatusBadRequest, "displayName is required")
		return
	}
	now := time.Now().UTC()
	g := Group{
		Schemas:     []string{schemaGroup},
		ID:          uuid.NewString(),
		DisplayName: in.DisplayName,
		Members:     in.Members,
		Meta:        scimMeta{ResourceType: "Group", Created: now, LastModified: now},
	}
	// Join the new membership to the tuple store before recording the
	// group, so a group is only persisted once its tuples exist. A
	// DisplayName matching the role convention additionally grants the
	// tenant role binding in the same atomic reconcile.
	ops := s.groupReconcileOps(g.ID, nil, g.Members)
	ops = append(ops, groupRoleReconcileOps("", g.DisplayName, g.ID)...)
	if err := s.applyTupleOps(r.Context(), ops); err != nil {
		scimError(w, http.StatusInternalServerError, "failed to sync group membership tuples")
		return
	}
	s.dir.mu.Lock()
	if err := s.dirStore.SaveGroup(r.Context(), g); err != nil {
		s.dir.mu.Unlock()
		// Persist failed: fully reverse the membership grants applied above.
		s.rollbackTupleOps(r.Context(), ops)
		s.log.Error("scim: persist group on create", zap.Error(err))
		scimError(w, http.StatusInternalServerError, "failed to persist group")
		return
	}
	s.dir.groups[g.ID] = g
	s.dir.mu.Unlock()
	httpx.WriteJSON(w, http.StatusCreated, g)
}

func (s *Service) scimListGroups(w http.ResponseWriter, _ *http.Request) {
	s.dir.mu.RLock()
	res := make([]any, 0, len(s.dir.groups))
	for _, g := range s.dir.groups {
		res = append(res, g)
	}
	s.dir.mu.RUnlock()
	httpx.WriteJSON(w, http.StatusOK, listResponse{
		Schemas:      []string{schemaListResp},
		TotalResults: len(res),
		Resources:    res,
	})
}

func (s *Service) scimGetGroup(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "id")
	s.dir.mu.RLock()
	g, ok := s.dir.groups[id]
	s.dir.mu.RUnlock()
	if !ok {
		scimError(w, http.StatusNotFound, "group not found")
		return
	}
	httpx.WriteJSON(w, http.StatusOK, g)
}

func (s *Service) scimReplaceGroup(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "id")
	var in Group
	if err := httpx.DecodeJSON(r, &in); err != nil {
		scimError(w, http.StatusBadRequest, "invalid SCIM Group payload")
		return
	}
	s.dir.mu.RLock()
	g, ok := s.dir.groups[id]
	s.dir.mu.RUnlock()
	if !ok {
		scimError(w, http.StatusNotFound, "group not found")
		return
	}
	next := g
	if in.DisplayName != "" {
		next.DisplayName = in.DisplayName
	}
	next.Members = in.Members
	next.Meta.LastModified = time.Now().UTC()
	// Reconcile the membership delta (grant added, revoke removed) and the
	// role binding delta (a DisplayName change re-points or drops it)
	// before committing, so the directory and tuple store stay in lock-step.
	ops := s.groupReconcileOps(id, g.Members, next.Members)
	ops = append(ops, groupRoleReconcileOps(g.DisplayName, next.DisplayName, id)...)
	if err := s.applyTupleOps(r.Context(), ops); err != nil {
		scimError(w, http.StatusInternalServerError, "failed to sync group membership tuples")
		return
	}
	s.dir.mu.Lock()
	// Re-check existence under the write lock: a concurrent delete may have
	// removed the group while we reconciled tuples outside the lock.
	// Writing unconditionally would resurrect a deleted group.
	if _, ok := s.dir.groups[id]; !ok {
		s.dir.mu.Unlock()
		s.compensateGrants(r.Context(), ops)
		scimError(w, http.StatusConflict, "group was deleted concurrently")
		return
	}
	if err := s.dirStore.SaveGroup(r.Context(), next); err != nil {
		s.dir.mu.Unlock()
		// Persist failed: fully reverse the reconciliation so the substrate
		// matches the unchanged group. rollbackTupleOps (not compensateGrants)
		// is required because removed members produce revokes that
		// compensateGrants would leave gone.
		s.rollbackTupleOps(r.Context(), ops)
		s.log.Error("scim: persist group on replace", zap.Error(err))
		scimError(w, http.StatusInternalServerError, "failed to persist group")
		return
	}
	s.dir.groups[id] = next
	s.dir.mu.Unlock()
	httpx.WriteJSON(w, http.StatusOK, next)
}

func (s *Service) scimDeleteGroup(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "id")
	s.dir.mu.RLock()
	g, ok := s.dir.groups[id]
	s.dir.mu.RUnlock()
	if !ok {
		scimError(w, http.StatusNotFound, "group not found")
		return
	}
	// Revoke all of the group's membership tuples, and its role binding if
	// any, before removing it.
	ops := s.groupReconcileOps(id, g.Members, nil)
	ops = append(ops, groupRoleReconcileOps(g.DisplayName, "", id)...)
	if err := s.applyTupleOps(r.Context(), ops); err != nil {
		scimError(w, http.StatusInternalServerError, "failed to sync group membership tuples")
		return
	}
	s.dir.mu.Lock()
	if err := s.dirStore.DeleteGroup(r.Context(), id); err != nil {
		s.dir.mu.Unlock()
		// Persist failed: re-grant the membership tuples revoked above so the
		// substrate stays consistent with the still-present group.
		s.rollbackTupleOps(r.Context(), ops)
		s.log.Error("scim: persist group delete", zap.Error(err))
		scimError(w, http.StatusInternalServerError, "failed to persist group deletion")
		return
	}
	delete(s.dir.groups, id)
	s.dir.mu.Unlock()
	w.WriteHeader(http.StatusNoContent)
}
