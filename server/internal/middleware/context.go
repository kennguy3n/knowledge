// Package middleware holds the gateway's HTTP middleware: request-id
// injection, panic recovery, CORS, body-size limiting, bearer/JWT
// authentication, and per-IP/per-tenant token-bucket rate limiting.
package middleware

import "context"

// ctxKey is an unexported context key type to avoid collisions.
type ctxKey int

const (
	keyRequestID ctxKey = iota
	keyPrincipal
	keyTenant
)

// Principal describes the authenticated caller.
type Principal struct {
	// Subject is the caller identity ("service" for the static API
	// key, or the JWT `sub` claim for tenant tokens).
	Subject string
	// TenantID is the tenant the caller acts within (empty for the
	// service principal).
	TenantID string
	// Service is true when the caller authenticated with the static
	// admin/service API key.
	Service bool
}

// RequestID returns the request id stored in ctx, or "" if absent.
func RequestID(ctx context.Context) string {
	if v, ok := ctx.Value(keyRequestID).(string); ok {
		return v
	}
	return ""
}

// PrincipalFrom returns the authenticated principal stored in ctx.
func PrincipalFrom(ctx context.Context) (Principal, bool) {
	p, ok := ctx.Value(keyPrincipal).(Principal)
	return p, ok
}

// TenantID returns the tenant id stored in ctx, or "" if absent.
func TenantID(ctx context.Context) string {
	if v, ok := ctx.Value(keyTenant).(string); ok {
		return v
	}
	return ""
}

func withRequestID(ctx context.Context, id string) context.Context {
	return context.WithValue(ctx, keyRequestID, id)
}

func withPrincipal(ctx context.Context, p Principal) context.Context {
	ctx = context.WithValue(ctx, keyPrincipal, p)
	if p.TenantID != "" {
		ctx = context.WithValue(ctx, keyTenant, p.TenantID)
	}
	return ctx
}
