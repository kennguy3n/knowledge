package tenant

import (
	"context"
	"errors"
	"net/http"
	"time"

	"github.com/go-chi/chi/v5"
	"github.com/google/uuid"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
	"github.com/kennguy3n/knowledge/server/internal/validate"
)

// keyMinter mints per-tenant hybrid KEM keypairs. Satisfied by
// [substrate.Client]; narrowed so tests can inject a fake.
type keyMinter interface {
	HybridKeypair(ctx context.Context) (substrate.HybridKeypair, error)
}

// Service implements tenant CRUD, configuration, member provisioning,
// and per-tenant key management.
type Service struct {
	store Store
	keys  keyMinter
}

// New constructs a tenant Service.
func New(store Store, keys keyMinter) *Service {
	return &Service{store: store, keys: keys}
}

// Routes returns a chi router for the tenant surface.
func (s *Service) Routes() http.Handler {
	r := chi.NewRouter()
	r.Post("/", s.handleCreate)
	r.Get("/", s.handleList)
	r.Get("/{id}", s.handleGet)
	r.Delete("/{id}", s.handleDelete)
	r.Put("/{id}/config", s.handleUpdateConfig)
	r.Post("/{id}/key/rotate", s.handleRotateKey)
	r.Get("/{id}/members", s.handleListMembers)
	r.Post("/{id}/members", s.handleInviteMember)
	r.Post("/{id}/members/{userID}/activate", s.handleActivateMember)
	r.Post("/{id}/members/{userID}/suspend", s.handleSuspendMember)
	r.Delete("/{id}/members/{userID}", s.handleRemoveMember)
	return r
}

// CreateRequest is the body of POST /tenants.
type CreateRequest struct {
	Name   string  `json:"name"`
	Config *Config `json:"config,omitempty"`
}

// Create provisions a new tenant, minting its hybrid KEM keypair.
func (s *Service) Create(ctx context.Context, req CreateRequest) (Tenant, error) {
	if err := validate.NonEmptyUTF8(req.Name); err != nil {
		return Tenant{}, httpx.BadRequest("name is required and must be valid UTF-8")
	}
	cfg := DefaultConfig()
	if req.Config != nil {
		cfg = *req.Config
		if !cfg.SynthesisTier.Valid() {
			return Tenant{}, httpx.BadRequest("invalid synthesis_tier")
		}
		if cfg.ConnectorLimit < 0 || cfg.RetentionDays < 0 {
			return Tenant{}, httpx.BadRequest("connector_limit and retention_days must be non-negative")
		}
	}
	kp, err := s.keys.HybridKeypair(ctx)
	if err != nil {
		return Tenant{}, err
	}
	t := Tenant{
		ID:        uuid.NewString(),
		Name:      req.Name,
		Config:    cfg,
		Key:       CryptoKey{Algorithm: kp.Algorithm, PublicKeyHex: kp.PublicKeyHex},
		CreatedAt: time.Now().UTC(),
	}
	if err := s.store.CreateTenant(ctx, t); err != nil {
		return Tenant{}, mapStoreErr(err)
	}
	return t, nil
}

// Get loads a tenant by id.
func (s *Service) Get(ctx context.Context, id string) (Tenant, error) {
	if _, err := uuid.Parse(id); err != nil {
		return Tenant{}, httpx.BadRequest("tenant id must be a UUID")
	}
	t, err := s.store.GetTenant(ctx, id)
	if err != nil {
		return Tenant{}, mapStoreErr(err)
	}
	return t, nil
}

// RotateKey mints a fresh keypair and replaces the tenant's public key.
func (s *Service) RotateKey(ctx context.Context, id string) (Tenant, error) {
	t, err := s.Get(ctx, id)
	if err != nil {
		return Tenant{}, err
	}
	kp, err := s.keys.HybridKeypair(ctx)
	if err != nil {
		return Tenant{}, err
	}
	t.Key = CryptoKey{Algorithm: kp.Algorithm, PublicKeyHex: kp.PublicKeyHex}
	if err := s.store.UpdateTenant(ctx, t); err != nil {
		return Tenant{}, mapStoreErr(err)
	}
	return t, nil
}

// InviteRequest is the body of POST /tenants/{id}/members.
type InviteRequest struct {
	UserID string `json:"user_id"`
	Email  string `json:"email"`
}

// InviteMember provisions a member in the invited state.
func (s *Service) InviteMember(ctx context.Context, tenantID string, req InviteRequest) (Member, error) {
	if _, err := s.Get(ctx, tenantID); err != nil {
		return Member{}, err
	}
	if _, err := validate.ScopeID(req.UserID); err != nil {
		return Member{}, httpx.BadRequest("user_id must be a UUID")
	}
	if err := validate.NonEmptyUTF8(req.Email); err != nil {
		return Member{}, httpx.BadRequest("email is required")
	}
	m := Member{
		TenantID:  tenantID,
		UserID:    req.UserID,
		Email:     req.Email,
		Status:    StatusInvited,
		UpdatedAt: time.Now().UTC(),
	}
	if err := s.store.UpsertMember(ctx, m); err != nil {
		return Member{}, mapStoreErr(err)
	}
	return m, nil
}

// transitionMember moves a member to the target status.
func (s *Service) transitionMember(ctx context.Context, tenantID, userID string, to MemberStatus) (Member, error) {
	m, err := s.store.GetMember(ctx, tenantID, userID)
	if err != nil {
		return Member{}, mapStoreErr(err)
	}
	m.Status = to
	m.UpdatedAt = time.Now().UTC()
	if err := s.store.UpsertMember(ctx, m); err != nil {
		return Member{}, mapStoreErr(err)
	}
	return m, nil
}

func (s *Service) handleCreate(w http.ResponseWriter, r *http.Request) {
	var req CreateRequest
	if err := httpx.DecodeJSON(r, &req); err != nil {
		httpx.WriteError(w, err)
		return
	}
	t, err := s.Create(r.Context(), req)
	if err != nil {
		httpx.WriteError(w, err)
		return
	}
	httpx.WriteJSON(w, http.StatusCreated, t)
}

func (s *Service) handleList(w http.ResponseWriter, r *http.Request) {
	ts, err := s.store.ListTenants(r.Context())
	if err != nil {
		httpx.WriteError(w, mapStoreErr(err))
		return
	}
	if ts == nil {
		ts = []Tenant{}
	}
	httpx.WriteJSON(w, http.StatusOK, ts)
}

func (s *Service) handleGet(w http.ResponseWriter, r *http.Request) {
	t, err := s.Get(r.Context(), chi.URLParam(r, "id"))
	if err != nil {
		httpx.WriteError(w, err)
		return
	}
	httpx.WriteJSON(w, http.StatusOK, t)
}

func (s *Service) handleDelete(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "id")
	if _, err := uuid.Parse(id); err != nil {
		httpx.WriteError(w, httpx.BadRequest("tenant id must be a UUID"))
		return
	}
	if err := s.store.DeleteTenant(r.Context(), id); err != nil {
		httpx.WriteError(w, mapStoreErr(err))
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

func (s *Service) handleUpdateConfig(w http.ResponseWriter, r *http.Request) {
	var cfg Config
	if err := httpx.DecodeJSON(r, &cfg); err != nil {
		httpx.WriteError(w, err)
		return
	}
	if !cfg.SynthesisTier.Valid() {
		httpx.WriteError(w, httpx.BadRequest("invalid synthesis_tier"))
		return
	}
	t, err := s.Get(r.Context(), chi.URLParam(r, "id"))
	if err != nil {
		httpx.WriteError(w, err)
		return
	}
	t.Config = cfg
	if err := s.store.UpdateTenant(r.Context(), t); err != nil {
		httpx.WriteError(w, mapStoreErr(err))
		return
	}
	httpx.WriteJSON(w, http.StatusOK, t)
}

func (s *Service) handleRotateKey(w http.ResponseWriter, r *http.Request) {
	t, err := s.RotateKey(r.Context(), chi.URLParam(r, "id"))
	if err != nil {
		httpx.WriteError(w, err)
		return
	}
	httpx.WriteJSON(w, http.StatusOK, t)
}

func (s *Service) handleListMembers(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "id")
	if _, err := s.Get(r.Context(), id); err != nil {
		httpx.WriteError(w, err)
		return
	}
	ms, err := s.store.ListMembers(r.Context(), id)
	if err != nil {
		httpx.WriteError(w, mapStoreErr(err))
		return
	}
	if ms == nil {
		ms = []Member{}
	}
	httpx.WriteJSON(w, http.StatusOK, ms)
}

func (s *Service) handleInviteMember(w http.ResponseWriter, r *http.Request) {
	var req InviteRequest
	if err := httpx.DecodeJSON(r, &req); err != nil {
		httpx.WriteError(w, err)
		return
	}
	m, err := s.InviteMember(r.Context(), chi.URLParam(r, "id"), req)
	if err != nil {
		httpx.WriteError(w, err)
		return
	}
	httpx.WriteJSON(w, http.StatusCreated, m)
}

func (s *Service) handleActivateMember(w http.ResponseWriter, r *http.Request) {
	s.transitionHandler(w, r, StatusActive)
}

func (s *Service) handleSuspendMember(w http.ResponseWriter, r *http.Request) {
	s.transitionHandler(w, r, StatusSuspended)
}

func (s *Service) transitionHandler(w http.ResponseWriter, r *http.Request, to MemberStatus) {
	m, err := s.transitionMember(r.Context(), chi.URLParam(r, "id"), chi.URLParam(r, "userID"), to)
	if err != nil {
		httpx.WriteError(w, err)
		return
	}
	httpx.WriteJSON(w, http.StatusOK, m)
}

func (s *Service) handleRemoveMember(w http.ResponseWriter, r *http.Request) {
	if err := s.store.DeleteMember(r.Context(), chi.URLParam(r, "id"), chi.URLParam(r, "userID")); err != nil {
		httpx.WriteError(w, mapStoreErr(err))
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

// mapStoreErr converts store sentinels into HTTP errors.
func mapStoreErr(err error) error {
	switch {
	case err == nil:
		return nil
	case errors.Is(err, ErrNotFound):
		return httpx.NotFound("tenant or member not found")
	case errors.Is(err, ErrConflict):
		return httpx.NewError(http.StatusConflict, "Conflict", "resource already exists")
	default:
		return httpx.Internal("tenant store error")
	}
}
