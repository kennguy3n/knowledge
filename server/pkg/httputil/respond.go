// Package httputil provides helpers for HTTP JSON responses.
package httputil

import (
	"encoding/json"
	"net/http"
)

// JSON writes a JSON response with the given status code.
func JSON(w http.ResponseWriter, status int, v interface{}) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(status)
	if v != nil {
		_ = json.NewEncoder(w).Encode(v)
	}
}

// Error writes a JSON error response.
func Error(w http.ResponseWriter, status int, msg string) {
	JSON(w, status, map[string]string{"error": msg})
}
