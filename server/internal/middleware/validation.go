package middleware

import (
	"net/http"
	"unicode/utf8"

	"go.uber.org/zap"
)

const maxBodySize = 10 * 1024 * 1024 // 10 MB

// MaxBodySize limits request body size to 10 MB.
func MaxBodySize(logger *zap.Logger) func(http.Handler) http.Handler {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			if r.ContentLength > maxBodySize {
				logger.Warn("request body too large", zap.Int64("size", r.ContentLength))
				http.Error(w, `{"error":"request body too large (max 10 MB)"}`, http.StatusRequestEntityTooLarge)
				return
			}
			r.Body = http.MaxBytesReader(w, r.Body, maxBodySize)
			next.ServeHTTP(w, r)
		})
	}
}

// ValidateUTF8Body checks a string is valid UTF-8.
func ValidateUTF8Body(s string) bool {
	return utf8.ValidString(s)
}
