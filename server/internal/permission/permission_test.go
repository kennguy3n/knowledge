package permission

import (
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/golang-jwt/jwt/v5"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
	"github.com/kennguy3n/knowledge/server/internal/middleware"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
)

const testJWTSecret = "permission-test-secret"

// userRequest returns a request authenticated as the given user via a
// signed tenant JWT, chained through the real authenticator so the
// principal lands in the context exactly as in production.
func userRequest(t *testing.T, next http.Handler) http.Handler {
	t.Helper()
	return middleware.NewAuthenticator("", testJWTSecret).Middleware(next)
}

func signUserToken(t *testing.T, subject string) string {
	t.Helper()
	tok := jwt.NewWithClaims(jwt.SigningMethodHS256, jwt.MapClaims{"sub": subject})
	s, err := tok.SignedString([]byte(testJWTSecret))
	if err != nil {
		t.Fatal(err)
	}
	return s
}

const (
	objUUID  = "11111111-1111-1111-1111-111111111111"
	subUUID  = "22222222-2222-2222-2222-222222222222"
	relOwner = "owner"
)

type fakeChecker struct {
	granted, revoked []substrate.RelationTuple
	allow            bool
	err              error
}

func (f *fakeChecker) PermissionGrant(_ context.Context, t substrate.RelationTuple) error {
	f.granted = append(f.granted, t)
	return f.err
}

func (f *fakeChecker) PermissionRevoke(_ context.Context, t substrate.RelationTuple) error {
	f.revoked = append(f.revoked, t)
	return f.err
}

func (f *fakeChecker) PermissionCheck(_ context.Context, _ substrate.RelationTuple) (bool, error) {
	return f.allow, f.err
}

func validTuple() substrate.RelationTuple {
	return substrate.RelationTuple{
		Object:   substrate.ObjectRef{ObjectType: "tenant", ObjectID: objUUID},
		Relation: relOwner,
		Subject:  substrate.SubjectRef{SubjectType: "user", SubjectID: subUUID},
	}
}

func TestGrantValidation(t *testing.T) {
	t.Parallel()
	s := New(&fakeChecker{})
	err := s.Grant(context.Background(), substrate.RelationTuple{})
	var apiErr *httpx.Error
	if !errors.As(err, &apiErr) || apiErr.Status != http.StatusBadRequest {
		t.Fatalf("expected 400, got %v", err)
	}
}

func TestGrantRevokeCheck(t *testing.T) {
	t.Parallel()
	fc := &fakeChecker{allow: true}
	s := New(fc)
	ctx := context.Background()
	if err := s.Grant(ctx, validTuple()); err != nil {
		t.Fatalf("grant: %v", err)
	}
	if len(fc.granted) != 1 {
		t.Fatalf("grant not forwarded")
	}
	if err := s.Revoke(ctx, validTuple()); err != nil {
		t.Fatalf("revoke: %v", err)
	}
	allowed, err := s.Check(ctx, validTuple())
	if err != nil || !allowed {
		t.Fatalf("check: allowed=%v err=%v", allowed, err)
	}
}

func TestHandleGrantHTTP(t *testing.T) {
	t.Parallel()
	s := New(&fakeChecker{})
	body := `{"object":{"object_type":"tenant","object_id":"` + objUUID +
		`"},"relation":"owner","subject":{"subject_type":"user","subject_id":"` + subUUID + `"}}`
	req := httptest.NewRequest(http.MethodPost, "/grant", strings.NewReader(body))
	rec := httptest.NewRecorder()
	s.Routes().ServeHTTP(rec, req)
	if rec.Code != http.StatusCreated {
		t.Fatalf("code = %d body=%s", rec.Code, rec.Body.String())
	}
}

func TestRequireRelation(t *testing.T) {
	t.Parallel()
	extract := func(*http.Request) (string, string, bool) { return "tenant", objUUID, true }
	okHandler := http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) { w.WriteHeader(http.StatusOK) })

	t.Run("service principal bypasses", func(t *testing.T) {
		s := New(&fakeChecker{allow: false})
		// Dev-mode authenticator (no creds) yields a service principal.
		h := middleware.NewAuthenticator("", "").Middleware(
			s.RequireRelation(relOwner, extract)(okHandler))
		rec := httptest.NewRecorder()
		h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/", nil))
		if rec.Code != http.StatusOK {
			t.Fatalf("service principal blocked: %d", rec.Code)
		}
	})

	t.Run("denied without grant", func(t *testing.T) {
		s := New(&fakeChecker{allow: false})
		h := userRequest(t, s.RequireRelation(relOwner, extract)(okHandler))
		req := httptest.NewRequest(http.MethodGet, "/", nil)
		req.Header.Set("Authorization", "Bearer "+signUserToken(t, "u1"))
		rec := httptest.NewRecorder()
		h.ServeHTTP(rec, req)
		if rec.Code != http.StatusForbidden {
			t.Fatalf("expected 403, got %d", rec.Code)
		}
	})

	t.Run("allowed with grant", func(t *testing.T) {
		s := New(&fakeChecker{allow: true})
		h := userRequest(t, s.RequireRelation(relOwner, extract)(okHandler))
		req := httptest.NewRequest(http.MethodGet, "/", nil)
		req.Header.Set("Authorization", "Bearer "+signUserToken(t, "u1"))
		rec := httptest.NewRecorder()
		h.ServeHTTP(rec, req)
		if rec.Code != http.StatusOK {
			t.Fatalf("allowed principal blocked: %d", rec.Code)
		}
	})
}
