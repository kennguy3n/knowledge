package middleware

import (
	"crypto/subtle"
	"net"
	"net/http"
	"strings"

	"github.com/golang-jwt/jwt/v5"
	"github.com/google/uuid"
	"go.uber.org/zap"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
	"github.com/kennguy3n/knowledge/server/internal/validate"
)

// RequestIDHeader is the canonical request-id header name.
const RequestIDHeader = "X-Request-Id"

// InjectRequestID ensures every request carries an X-Request-Id: it
// reuses a valid inbound id or mints a new UUID, stores it in the
// context, and echoes it on the response so it can be propagated
// downstream.
func InjectRequestID(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		id := strings.TrimSpace(r.Header.Get(RequestIDHeader))
		if id == "" || len(id) > 200 {
			id = uuid.NewString()
		}
		w.Header().Set(RequestIDHeader, id)
		ctx := withRequestID(r.Context(), id)
		next.ServeHTTP(w, r.WithContext(ctx))
	})
}

// Recover converts a panic in any downstream handler into a logged
// 500 response instead of crashing the server.
func Recover(log *zap.Logger) func(http.Handler) http.Handler {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			defer func() {
				if rec := recover(); rec != nil {
					log.Error("panic recovered",
						zap.Any("panic", rec),
						zap.String("request_id", RequestID(r.Context())),
						zap.String("path", r.URL.Path),
					)
					httpx.WriteError(w, httpx.Internal("internal server error"))
				}
			}()
			next.ServeHTTP(w, r)
		})
	}
}

// BodyLimit caps request bodies at [validate.MaxBodyBytes] using
// http.MaxBytesReader, so oversized payloads are rejected during read.
func BodyLimit(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Body != nil {
			r.Body = http.MaxBytesReader(w, r.Body, validate.MaxBodyBytes)
		}
		next.ServeHTTP(w, r)
	})
}

// CORS applies a configurable cross-origin policy. An empty allow-list
// permits any origin ("*"); otherwise only listed origins are echoed.
func CORS(allowed []string) func(http.Handler) http.Handler {
	allowAll := len(allowed) == 0
	set := make(map[string]struct{}, len(allowed))
	for _, o := range allowed {
		set[o] = struct{}{}
	}
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			origin := r.Header.Get("Origin")
			if origin != "" {
				_, ok := set[origin]
				if allowAll || ok {
					w.Header().Set("Access-Control-Allow-Origin", origin)
					w.Header().Set("Vary", "Origin")
					w.Header().Set("Access-Control-Allow-Methods", "GET, POST, PUT, DELETE, OPTIONS")
					w.Header().Set("Access-Control-Allow-Headers", "Authorization, Content-Type, X-Request-Id")
					w.Header().Set("Access-Control-Max-Age", "600")
				}
			}
			if r.Method == http.MethodOptions {
				w.WriteHeader(http.StatusNoContent)
				return
			}
			next.ServeHTTP(w, r)
		})
	}
}

// Authenticator validates bearer credentials. It accepts either the
// static service API key or a tenant JWT signed with the HMAC secret.
type Authenticator struct {
	apiKey    string
	jwtSecret []byte
}

// NewAuthenticator builds an Authenticator. An empty apiKey disables
// static-key auth; an empty jwtSecret disables JWT auth. If both are
// empty, all requests are treated as the service principal (intended
// for local development only).
func NewAuthenticator(apiKey, jwtSecret string) *Authenticator {
	return &Authenticator{apiKey: apiKey, jwtSecret: []byte(jwtSecret)}
}

// Middleware authenticates the request, attaching a [Principal] to the
// context, or responds 401.
func (a *Authenticator) Middleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		// Dev mode: no credentials configured at all.
		if a.apiKey == "" && len(a.jwtSecret) == 0 {
			ctx := withPrincipal(r.Context(), Principal{Subject: "service", Service: true})
			next.ServeHTTP(w, r.WithContext(ctx))
			return
		}

		token := bearerToken(r)
		if token == "" {
			httpx.WriteError(w, httpx.Unauthorized("missing bearer token"))
			return
		}

		if a.apiKey != "" && subtle.ConstantTimeCompare([]byte(token), []byte(a.apiKey)) == 1 {
			ctx := withPrincipal(r.Context(), Principal{Subject: "service", Service: true})
			next.ServeHTTP(w, r.WithContext(ctx))
			return
		}

		if len(a.jwtSecret) > 0 {
			if principal, err := a.parseJWT(token); err == nil {
				next.ServeHTTP(w, r.WithContext(withPrincipal(r.Context(), principal)))
				return
			}
		}
		httpx.WriteError(w, httpx.Unauthorized("invalid credentials"))
	})
}

// parseJWT validates a tenant JWT and extracts the principal.
func (a *Authenticator) parseJWT(token string) (Principal, error) {
	claims := jwt.MapClaims{}
	_, err := jwt.ParseWithClaims(token, claims, func(t *jwt.Token) (any, error) {
		if _, ok := t.Method.(*jwt.SigningMethodHMAC); !ok {
			return nil, jwt.ErrTokenSignatureInvalid
		}
		return a.jwtSecret, nil
	}, jwt.WithValidMethods([]string{"HS256", "HS384", "HS512"}))
	if err != nil {
		return Principal{}, err
	}
	sub, _ := claims["sub"].(string)
	tenant, _ := claims["tenant_id"].(string)
	if sub == "" {
		return Principal{}, jwt.ErrTokenInvalidClaims
	}
	return Principal{Subject: sub, TenantID: tenant}, nil
}

// bearerToken extracts the token from an "Authorization: Bearer …"
// header, returning "" when absent or malformed.
func bearerToken(r *http.Request) string {
	h := r.Header.Get("Authorization")
	const prefix = "Bearer "
	if len(h) <= len(prefix) || !strings.EqualFold(h[:len(prefix)], prefix) {
		return ""
	}
	return strings.TrimSpace(h[len(prefix):])
}

// clientIP extracts the client IP, honouring X-Forwarded-For (first
// hop) then falling back to the remote address.
func clientIP(r *http.Request) string {
	if xff := r.Header.Get("X-Forwarded-For"); xff != "" {
		if comma := strings.IndexByte(xff, ','); comma >= 0 {
			return strings.TrimSpace(xff[:comma])
		}
		return strings.TrimSpace(xff)
	}
	host, _, err := net.SplitHostPort(r.RemoteAddr)
	if err != nil {
		return r.RemoteAddr
	}
	return host
}
