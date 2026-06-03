// Package validate holds reusable input-validation helpers shared by
// the gateway middleware and the individual services.
package validate

import (
	"errors"
	"unicode/utf8"

	"github.com/google/uuid"
)

// MaxBodyBytes is the hard ceiling on request body size enforced by
// the gateway (10 MiB).
const MaxBodyBytes int64 = 10 << 20

// Validation errors. Callers compare with [errors.Is].
var (
	// ErrNotUTF8 is returned when a field is not valid UTF-8.
	ErrNotUTF8 = errors.New("value is not valid UTF-8")
	// ErrBadScopeID is returned when a scope id is not a UUID.
	ErrBadScopeID = errors.New("scope_id is not a valid UUID")
	// ErrEmpty is returned when a required field is empty.
	ErrEmpty = errors.New("value must not be empty")
)

// ScopeID validates that s is a syntactically valid UUID and returns
// its canonical lower-case form. The substrate enforces UUID scope
// ids; rejecting malformed ids at the edge yields a clean 400 instead
// of a round-trip to Rust.
func ScopeID(s string) (string, error) {
	if s == "" {
		return "", ErrEmpty
	}
	id, err := uuid.Parse(s)
	if err != nil {
		return "", ErrBadScopeID
	}
	return id.String(), nil
}

// UTF8 verifies that s is valid UTF-8, returning [ErrNotUTF8]
// otherwise. Empty strings are valid.
func UTF8(s string) error {
	if !utf8.ValidString(s) {
		return ErrNotUTF8
	}
	return nil
}

// NonEmptyUTF8 combines an emptiness check with a UTF-8 check.
func NonEmptyUTF8(s string) error {
	if s == "" {
		return ErrEmpty
	}
	return UTF8(s)
}
