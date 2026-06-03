package tenant

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/google/uuid"

	"github.com/kennguy3n/knowledge/server/internal/substrate"
)

type fakeKeys struct {
	calls int
	err   error
}

func (f *fakeKeys) HybridKeypair(context.Context) (substrate.HybridKeypair, error) {
	f.calls++
	if f.err != nil {
		return substrate.HybridKeypair{}, f.err
	}
	return substrate.HybridKeypair{Algorithm: "x25519-kyber768", PublicKeyHex: "abcd"}, nil
}

func newService() (*Service, *fakeKeys) {
	k := &fakeKeys{}
	return New(NewMemoryStore(), k), k
}

func TestCreateValidation(t *testing.T) {
	t.Parallel()
	s, _ := newService()
	if _, err := s.Create(context.Background(), CreateRequest{Name: ""}); err == nil {
		t.Fatal("expected error on empty name")
	}
	cfg := DefaultConfig()
	cfg.SynthesisTier = SynthesisTier("nope")
	if _, err := s.Create(context.Background(), CreateRequest{Name: "x", Config: &cfg}); err == nil {
		t.Fatal("expected error on invalid tier")
	}
}

func TestCreateGetRotate(t *testing.T) {
	t.Parallel()
	s, keys := newService()
	ctx := context.Background()
	tn, err := s.Create(ctx, CreateRequest{Name: "Acme"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := uuid.Parse(tn.ID); err != nil {
		t.Fatalf("id not uuid: %q", tn.ID)
	}
	if tn.Key.PublicKeyHex != "abcd" {
		t.Fatalf("key not set: %+v", tn.Key)
	}
	got, err := s.Get(ctx, tn.ID)
	if err != nil || got.ID != tn.ID {
		t.Fatalf("get: %+v err=%v", got, err)
	}
	if _, err := s.Get(ctx, "not-a-uuid"); err == nil {
		t.Fatal("expected uuid validation error")
	}
	rotated, err := s.RotateKey(ctx, tn.ID)
	if err != nil {
		t.Fatal(err)
	}
	if keys.calls != 2 {
		t.Fatalf("expected 2 keypair mints, got %d", keys.calls)
	}
	_ = rotated
}

func TestCreateKeyError(t *testing.T) {
	t.Parallel()
	k := &fakeKeys{err: errors.New("kms down")}
	s := New(NewMemoryStore(), k)
	if _, err := s.Create(context.Background(), CreateRequest{Name: "x"}); err == nil {
		t.Fatal("expected keypair error to propagate")
	}
}

func TestMemberLifecycle(t *testing.T) {
	t.Parallel()
	s, _ := newService()
	ctx := context.Background()
	tn, _ := s.Create(ctx, CreateRequest{Name: "Acme"})
	uid := uuid.NewString()
	m, err := s.InviteMember(ctx, tn.ID, InviteRequest{UserID: uid, Email: "u@x.io"})
	if err != nil || m.Status != StatusInvited {
		t.Fatalf("invite: %+v err=%v", m, err)
	}
	if _, err := s.InviteMember(ctx, tn.ID, InviteRequest{UserID: "bad", Email: "u@x.io"}); err == nil {
		t.Fatal("expected uuid validation error")
	}
}

func TestHTTPCreateGet(t *testing.T) {
	t.Parallel()
	s, _ := newService()
	h := s.Routes()

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodPost, "/", strings.NewReader(`{"name":"Acme"}`)))
	if rec.Code != http.StatusCreated {
		t.Fatalf("create code = %d body=%s", rec.Code, rec.Body.String())
	}
	var created Tenant
	if err := json.Unmarshal(rec.Body.Bytes(), &created); err != nil {
		t.Fatal(err)
	}

	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/"+created.ID, nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("get code = %d", rec.Code)
	}

	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/"+uuid.NewString(), nil))
	if rec.Code != http.StatusNotFound {
		t.Fatalf("missing get code = %d", rec.Code)
	}
}
