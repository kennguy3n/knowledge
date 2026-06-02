package middleware

import (
	"context"
	"net/http"

	"github.com/google/uuid"
)

const requestIDKey contextKey = "request_id"

// RequestID extracts the request ID from context.
func RequestID(ctx context.Context) string {
	v, _ := ctx.Value(requestIDKey).(string)
	return v
}

// InjectRequestID injects or propagates X-Request-Id on every request.
func InjectRequestID(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		id := r.Header.Get("X-Request-Id")
		if id == "" {
			id = uuid.New().String()
		}
		w.Header().Set("X-Request-Id", id)
		ctx := context.WithValue(r.Context(), requestIDKey, id)
		next.ServeHTTP(w, r.WithContext(ctx))
	})
}
