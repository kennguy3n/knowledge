package tenant

import (
	"context"
	"errors"
	"net/http"
	"testing"

	"github.com/google/uuid"
)

func TestHandlerErrorPaths(t *testing.T) {
	t.Parallel()
	s, _ := newService()
	h := s.Routes(Authz{})
	missing := uuid.NewString()

	cases := []struct {
		method, path, body string
		want               int
	}{
		{http.MethodPost, "/", `{bad json`, http.StatusBadRequest},
		{http.MethodGet, "/" + missing, "", http.StatusNotFound},
		{http.MethodGet, "/not-a-uuid", "", http.StatusBadRequest},
		{http.MethodPut, "/" + missing + "/config", `{"synthesis_tier":"basic"}`, http.StatusNotFound},
		{http.MethodPost, "/" + missing + "/key/rotate", "", http.StatusNotFound},
		{http.MethodGet, "/" + missing + "/members", "", http.StatusNotFound},
		{http.MethodPost, "/" + missing + "/members", `{"user_id":"` + uuid.NewString() + `","email":"u@x.io"}`, http.StatusNotFound},
		{http.MethodDelete, "/" + missing + "/members/" + uuid.NewString(), "", http.StatusNotFound},
	}
	for _, c := range cases {
		if rec := req(h, c.method, c.path, c.body); rec.Code != c.want {
			t.Errorf("%s %s = %d, want %d (%s)", c.method, c.path, rec.Code, c.want, rec.Body.String())
		}
	}
}

func TestRotateKeyError(t *testing.T) {
	t.Parallel()
	k := &fakeKeys{}
	s := New(NewMemoryStore(), k)
	tn, _ := s.Create(context.Background(), CreateRequest{Name: "x"})
	k.err = errors.New("kms down")
	if _, err := s.RotateKey(context.Background(), tn.ID); err == nil {
		t.Fatal("expected rotate key error")
	}
}

func TestMapStoreErr(t *testing.T) {
	t.Parallel()
	if mapStoreErr(nil) != nil {
		t.Fatal("nil should map to nil")
	}
	if err := mapStoreErr(ErrConflict); err == nil {
		t.Fatal("conflict should map to error")
	}
	if err := mapStoreErr(errors.New("other")); err == nil {
		t.Fatal("generic should map to error")
	}
}
