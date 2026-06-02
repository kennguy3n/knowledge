package logging

import (
	"testing"
)

func TestNew(t *testing.T) {
	logger, err := New()
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if logger == nil {
		t.Fatal("expected non-nil logger")
	}
}

func TestIsPII(t *testing.T) {
	tests := []struct {
		key  string
		want bool
	}{
		{"email", true},
		{"password", true},
		{"token", true},
		{"secret", true},
		{"api_key", true},
		{"master_key", true},
		{"private_key", true},
		{"access_token", true},
		{"body", true},
		{"user_id", false},
		{"scope_id", false},
		{"action", false},
	}
	for _, tt := range tests {
		t.Run(tt.key, func(t *testing.T) {
			got := isPII(tt.key)
			if got != tt.want {
				t.Errorf("isPII(%q) = %v, want %v", tt.key, got, tt.want)
			}
		})
	}
}
