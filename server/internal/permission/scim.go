package permission

import (
	"net/http"
	"sync"
	"time"

	"github.com/go-chi/chi/v5"
	"github.com/google/uuid"

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

// directory is the in-memory SCIM store. Identity provisioning is kept
// distinct from authorization tuples (which live in the substrate).
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
	s.dir.mu.Lock()
	defer s.dir.mu.Unlock()
	u, ok := s.dir.users[id]
	if !ok {
		scimError(w, http.StatusNotFound, "user not found")
		return
	}
	if in.UserName != "" {
		u.UserName = in.UserName
	}
	u.Active = in.Active
	u.Emails = in.Emails
	u.Meta.LastModified = time.Now().UTC()
	s.dir.users[id] = u
	httpx.WriteJSON(w, http.StatusOK, u)
}

func (s *Service) scimDeleteUser(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "id")
	s.dir.mu.Lock()
	_, ok := s.dir.users[id]
	delete(s.dir.users, id)
	s.dir.mu.Unlock()
	if !ok {
		scimError(w, http.StatusNotFound, "user not found")
		return
	}
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
	s.dir.mu.Lock()
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
	s.dir.mu.Lock()
	defer s.dir.mu.Unlock()
	g, ok := s.dir.groups[id]
	if !ok {
		scimError(w, http.StatusNotFound, "group not found")
		return
	}
	if in.DisplayName != "" {
		g.DisplayName = in.DisplayName
	}
	g.Members = in.Members
	g.Meta.LastModified = time.Now().UTC()
	s.dir.groups[id] = g
	httpx.WriteJSON(w, http.StatusOK, g)
}

func (s *Service) scimDeleteGroup(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "id")
	s.dir.mu.Lock()
	_, ok := s.dir.groups[id]
	delete(s.dir.groups, id)
	s.dir.mu.Unlock()
	if !ok {
		scimError(w, http.StatusNotFound, "group not found")
		return
	}
	w.WriteHeader(http.StatusNoContent)
}
