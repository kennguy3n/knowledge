package gateway

import (
	"bytes"
	"encoding/json"
	"io"
	"net/http"

	"github.com/go-chi/chi/v5"

	"github.com/kennguy3n/knowledge/server/internal/middleware"
	"github.com/kennguy3n/knowledge/server/internal/permission"
	"github.com/kennguy3n/knowledge/server/internal/tenant"
)

// maxAuthzBodyPeek bounds how much of a request body the export tenant
// extractor reads to recover tenant_id before restoring the body for
// the handler. The body is already size-limited upstream by
// middleware.BodyLimit; this is a defensive second bound.
const maxAuthzBodyPeek = 1 << 20 // 1 MiB

// tenantObjectFromURL resolves the tenant object guarded by the
// per-tenant routes from the {id} URL parameter.
func tenantObjectFromURL(r *http.Request) (objectType, objectID string, ok bool) {
	id := chi.URLParam(r, "id")
	if id == "" {
		return "", "", false
	}
	return "tenant", id, true
}

// auditTenantFromQuery resolves the tenant whose audit log is being read
// from the required tenant_id query parameter. A missing tenant_id is
// unresolvable, so a non-service caller can neither read across tenants
// nor omit the filter to read every tenant's log.
func auditTenantFromQuery(r *http.Request) (objectType, objectID string, ok bool) {
	tid := r.URL.Query().Get("tenant_id")
	if tid == "" {
		return "", "", false
	}
	return "tenant", tid, true
}

// exportTenantFromBody resolves the tenant being exported from the
// request body's tenant_id, restoring the body so the handler can
// decode it again.
func exportTenantFromBody(r *http.Request) (objectType, objectID string, ok bool) {
	if r.Body == nil {
		return "", "", false
	}
	body, err := io.ReadAll(io.LimitReader(r.Body, maxAuthzBodyPeek))
	if err != nil {
		return "", "", false
	}
	r.Body = io.NopCloser(bytes.NewReader(body))
	var probe struct {
		TenantID string `json:"tenant_id"`
	}
	if json.Unmarshal(body, &probe) != nil || probe.TenantID == "" {
		return "", "", false
	}
	return "tenant", probe.TenantID, true
}

// tenantAuthz builds the per-route authorization for the tenant router:
// lifecycle (create/list-all/delete) is platform-global and service-only;
// per-tenant reads and mutations are ReBAC-authorized against the {id}
// tenant. With no permission service wired the per-tenant routes fail
// closed to service-only rather than running ungated.
func tenantAuthz(p *permission.Service) tenant.Authz {
	if p == nil {
		return tenant.Authz{
			Service: middleware.RequireService,
			Admin:   middleware.RequireService,
			Viewer:  middleware.RequireService,
		}
	}
	return tenant.Authz{
		Service: middleware.RequireService,
		Admin:   p.RequireRelation("admin", tenantObjectFromURL),
		Viewer:  p.RequireRelation("viewer", tenantObjectFromURL),
	}
}

// controlGuard returns the authorization middleware for a per-tenant
// control-plane mount (audit, export): ReBAC against the tenant resolved
// by extract, failing closed to service-only when no permission service
// is wired.
func controlGuard(p *permission.Service, relation string, extract permission.ObjectExtractor) func(http.Handler) http.Handler {
	if p == nil {
		return middleware.RequireService
	}
	return p.RequireRelation(relation, extract)
}
