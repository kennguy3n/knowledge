package middleware

import (
	"net/http"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
)

// RequireService is middleware that admits only the service principal
// (the static admin/service API key, or dev mode when no credentials
// are configured). Tenant-user JWT principals receive 403. It guards
// platform-global operations that have no single-tenant subject —
// tenant lifecycle (create/list-all/delete), SCIM directory
// provisioning, and authorization-graph mutation/inspection
// (/permission/grant|revoke|check).
func RequireService(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		p, ok := PrincipalFrom(r.Context())
		if !ok || !p.Service {
			httpx.WriteError(w, httpx.Forbidden("service credential required"))
			return
		}
		next.ServeHTTP(w, r)
	})
}
