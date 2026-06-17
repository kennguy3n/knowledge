package gateway

import (
	"context"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/golang-jwt/jwt/v5"

	"github.com/kennguy3n/knowledge/server/internal/audit"
	"github.com/kennguy3n/knowledge/server/internal/export"
	"github.com/kennguy3n/knowledge/server/internal/middleware"
	"github.com/kennguy3n/knowledge/server/internal/permission"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
	"github.com/kennguy3n/knowledge/server/internal/tenant"
)

const (
	authzAPIKey    = "gateway-authz-service-key"
	authzJWTSecret = "gateway-authz-jwt-secret"
	authzTenantID  = "33333333-3333-3333-3333-333333333333"
)

// fakePermChecker satisfies permission's structural checker; allow
// drives the ReBAC decision for every RequireRelation gate.
type fakePermChecker struct{ allow bool }

func (fakePermChecker) PermissionGrant(context.Context, substrate.RelationTuple) error  { return nil }
func (fakePermChecker) PermissionRevoke(context.Context, substrate.RelationTuple) error { return nil }
func (f fakePermChecker) PermissionCheck(context.Context, substrate.RelationTuple) (bool, error) {
	return f.allow, nil
}

// fakeKeyMinter satisfies tenant's keyMinter.
type fakeKeyMinter struct{}

func (fakeKeyMinter) HybridKeypair(context.Context) (substrate.HybridKeypair, error) {
	return substrate.HybridKeypair{}, nil
}

// fakeExporter satisfies export's exporter.
type fakeExporter struct{}

func (fakeExporter) ExportEvaluate(context.Context, substrate.ExportEvaluateRequest) (substrate.ExportDecision, error) {
	return substrate.ExportDecision{}, nil
}

// authzRouter builds a fully-wired gateway with an authenticator (static
// service key + JWT secret) and a ReBAC checker whose decision is set by
// allow.
func authzRouter(allow bool) http.Handler {
	auditSvc := audit.New(audit.NewMemoryStore())
	return NewRouter(Deps{
		Substrate:   &fakeSub{},
		Permissions: permission.New(fakePermChecker{allow: allow}),
		Tenants:     tenant.New(tenant.NewMemoryStore(), fakeKeyMinter{}),
		Audit:       auditSvc,
		Exports:     export.New(fakeExporter{}, auditSvc),
		Auth:        middleware.NewAuthenticator(authzAPIKey, authzJWTSecret),
	})
}

func signAuthzToken(t *testing.T, subject string) string {
	t.Helper()
	tok := jwt.NewWithClaims(jwt.SigningMethodHS256, jwt.MapClaims{"sub": subject})
	s, err := tok.SignedString([]byte(authzJWTSecret))
	if err != nil {
		t.Fatal(err)
	}
	return s
}

func authzDo(t *testing.T, h http.Handler, method, path, body, bearer string) *httptest.ResponseRecorder {
	t.Helper()
	var br io.Reader
	if body != "" {
		br = strings.NewReader(body)
	}
	r := httptest.NewRequest(method, path, br)
	if bearer != "" {
		r.Header.Set("Authorization", "Bearer "+bearer)
	}
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, r)
	return rec
}

type authzRoute struct {
	name   string
	method string
	path   string
	body   string
}

// TestControlPlaneServiceOnly proves platform-global routes (tenant
// lifecycle, permission graph, SCIM) admit only the service principal:
// a valid tenant-user JWT is rejected with 403 regardless of the ReBAC
// checker, while the service principal reaches the handler.
func TestControlPlaneServiceOnly(t *testing.T) {
	t.Parallel()
	routes := []authzRoute{
		{"create tenant", http.MethodPost, "/api/v1/tenants", `{"name":"acme"}`},
		{"list tenants", http.MethodGet, "/api/v1/tenants", ""},
		{"delete tenant", http.MethodDelete, "/api/v1/tenants/" + authzTenantID, ""},
		{"permission grant", http.MethodPost, "/api/v1/permission/grant", `{}`},
		{"permission revoke", http.MethodPost, "/api/v1/permission/revoke", `{}`},
		{"permission check", http.MethodPost, "/api/v1/permission/check", `{}`},
		{"scim list users", http.MethodGet, "/api/v1/scim/v2/Users", ""},
	}
	// The static API key is independent of the ReBAC checker, so a
	// permissive checker must not relax the service-only guard.
	for _, allow := range []bool{false, true} {
		h := authzRouter(allow)
		for _, rt := range routes {
			rec := authzDo(t, h, rt.method, rt.path, rt.body, signAuthzToken(t, "u1"))
			if rec.Code != http.StatusForbidden {
				t.Fatalf("[allow=%v] %s: tenant-user code = %d, want 403 (body=%s)",
					allow, rt.name, rec.Code, rec.Body.String())
			}
			rec = authzDo(t, h, rt.method, rt.path, rt.body, authzAPIKey)
			if rec.Code == http.StatusForbidden {
				t.Fatalf("[allow=%v] %s: service principal blocked (403, body=%s)",
					allow, rt.name, rec.Body.String())
			}
		}
	}
}

// TestControlPlaneReBAC proves per-tenant routes are ReBAC-authorized
// against the tenant resolved from the request: a tenant-user without
// the relation is denied, one holding it passes the gate, and the
// service principal bypasses it.
func TestControlPlaneReBAC(t *testing.T) {
	t.Parallel()
	exportBody := `{"scope_id":"` + scopeUUID + `","tenant_id":"` + authzTenantID +
		`","format":"json","profile":{"k":"v"}}`
	routes := []authzRoute{
		{"get tenant (viewer)", http.MethodGet, "/api/v1/tenants/" + authzTenantID, ""},
		{"list members (viewer)", http.MethodGet, "/api/v1/tenants/" + authzTenantID + "/members", ""},
		{"audit query (viewer)", http.MethodGet, "/api/v1/audit?tenant_id=" + authzTenantID, ""},
		{"update config (admin)", http.MethodPut, "/api/v1/tenants/" + authzTenantID + "/config", `{"config":{}}`},
		{"export profile (admin)", http.MethodPost, "/api/v1/export/profile", exportBody},
	}

	deny := authzRouter(false)
	for _, rt := range routes {
		rec := authzDo(t, deny, rt.method, rt.path, rt.body, signAuthzToken(t, "u1"))
		if rec.Code != http.StatusForbidden {
			t.Fatalf("%s: unauthorized caller code = %d, want 403 (body=%s)",
				rt.name, rec.Code, rec.Body.String())
		}
	}

	allow := authzRouter(true)
	for _, rt := range routes {
		rec := authzDo(t, allow, rt.method, rt.path, rt.body, signAuthzToken(t, "u1"))
		if rec.Code == http.StatusForbidden {
			t.Fatalf("%s: authorized caller blocked (403, body=%s)", rt.name, rec.Body.String())
		}
		rec = authzDo(t, allow, rt.method, rt.path, rt.body, authzAPIKey)
		if rec.Code == http.StatusForbidden {
			t.Fatalf("%s: service principal blocked (403, body=%s)", rt.name, rec.Body.String())
		}
	}
}

// TestAuditReadAllRequiresTenantScope proves a tenant-user cannot omit
// tenant_id to read every tenant's audit log: the protected object is
// unresolvable so the viewer gate denies even a holder of the relation,
// while the service principal retains cross-tenant visibility.
func TestAuditReadAllRequiresTenantScope(t *testing.T) {
	t.Parallel()
	h := authzRouter(true)

	rec := authzDo(t, h, http.MethodGet, "/api/v1/audit", "", signAuthzToken(t, "u1"))
	if rec.Code != http.StatusForbidden {
		t.Fatalf("audit read-all not closed: code = %d, want 403 (body=%s)", rec.Code, rec.Body.String())
	}

	rec = authzDo(t, h, http.MethodGet, "/api/v1/audit", "", authzAPIKey)
	if rec.Code != http.StatusOK {
		t.Fatalf("service audit read-all code = %d, want 200 (body=%s)", rec.Code, rec.Body.String())
	}
}

// TestExportBodyReplayedIntact proves the export tenant extractor replays
// the full request body to the handler. A valid export payload larger
// than any in-extractor peek window, authorized via a tenant-user JWT,
// must reach handleProfile intact and succeed; if the extractor only
// buffered a prefix it would either fail to parse tenant_id (403) or
// feed the handler a truncated body (400).
func TestExportBodyReplayedIntact(t *testing.T) {
	t.Parallel()
	h := authzRouter(true)

	// 2 MiB profile blob: larger than a 1 MiB peek, well under the
	// 10 MiB BodyLimit, so a correct extractor replays it whole.
	blob := strings.Repeat("a", 2<<20)
	body := `{"scope_id":"` + scopeUUID + `","tenant_id":"` + authzTenantID +
		`","format":"json","profile":{"blob":"` + blob + `"}}`

	rec := authzDo(t, h, http.MethodPost, "/api/v1/export/profile", body, signAuthzToken(t, "u1"))
	if rec.Code != http.StatusOK {
		t.Fatalf("large export body truncated: code = %d, want 200 (body=%s)", rec.Code, rec.Body.String())
	}
}
