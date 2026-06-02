// Package middleware provides HTTP middleware for the API gateway.
package middleware

import (
	"context"
	"crypto/subtle"
	"net/http"
	"strings"

	"github.com/golang-jwt/jwt/v5"
	"go.uber.org/zap"
)

type contextKey string

const (
	// TenantIDKey is the context key for the authenticated tenant ID.
	TenantIDKey contextKey = "tenant_id"
	// ActorIDKey is the context key for the authenticated actor (user) ID.
	ActorIDKey contextKey = "actor_id"
)

// TenantID extracts the tenant ID from request context.
func TenantID(ctx context.Context) string {
	v, _ := ctx.Value(TenantIDKey).(string)
	return v
}

// ActorID extracts the actor ID from request context.
func ActorID(ctx context.Context) string {
	v, _ := ctx.Value(ActorIDKey).(string)
	return v
}

// Auth returns middleware that validates Bearer tokens.
// It supports both static API keys and JWT tenant tokens.
func Auth(apiKey, jwtSecret string, logger *zap.Logger) func(http.Handler) http.Handler {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			auth := r.Header.Get("Authorization")
			if auth == "" {
				http.Error(w, `{"error":"missing authorization header"}`, http.StatusUnauthorized)
				return
			}

			if !strings.HasPrefix(auth, "Bearer ") {
				http.Error(w, `{"error":"invalid authorization scheme"}`, http.StatusUnauthorized)
				return
			}

			token := strings.TrimPrefix(auth, "Bearer ")

			// Try static API key first.
			if subtle.ConstantTimeCompare([]byte(token), []byte(apiKey)) == 1 {
				ctx := context.WithValue(r.Context(), TenantIDKey, "system")
				ctx = context.WithValue(ctx, ActorIDKey, "api-key")
				next.ServeHTTP(w, r.WithContext(ctx))
				return
			}

			// Try JWT.
			if jwtSecret != "" {
				claims, err := validateJWT(token, jwtSecret)
				if err == nil {
					ctx := context.WithValue(r.Context(), TenantIDKey, claims.TenantID)
					ctx = context.WithValue(ctx, ActorIDKey, claims.Subject)
					next.ServeHTTP(w, r.WithContext(ctx))
					return
				}
				logger.Debug("JWT validation failed", zap.Error(err))
			}

			http.Error(w, `{"error":"invalid or expired token"}`, http.StatusUnauthorized)
		})
	}
}

// TenantClaims are custom JWT claims for tenant tokens.
type TenantClaims struct {
	jwt.RegisteredClaims
	TenantID string `json:"tenant_id"`
}

func validateJWT(tokenStr, secret string) (*TenantClaims, error) {
	claims := &TenantClaims{}
	_, err := jwt.ParseWithClaims(tokenStr, claims, func(_ *jwt.Token) (interface{}, error) {
		return []byte(secret), nil
	}, jwt.WithValidMethods([]string{"HS256", "HS384", "HS512"}))
	if err != nil {
		return nil, err
	}
	return claims, nil
}
