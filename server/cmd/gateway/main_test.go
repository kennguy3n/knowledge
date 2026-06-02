package main

import (
	"testing"

	"github.com/google/uuid"
)

func TestIsValidUUID(t *testing.T) {
	tests := []struct {
		name  string
		input string
		want  bool
	}{
		{"valid v4", uuid.New().String(), true},
		{"valid nil", "00000000-0000-0000-0000-000000000000", true},
		{"empty", "", false},
		{"garbage", "not-a-uuid", false},
		{"short", "abc123", false},
		{"no hyphens", "550e8400e29b41d4a716446655440000", true},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := isValidUUID(tt.input)
			if got != tt.want {
				t.Errorf("isValidUUID(%q) = %v, want %v", tt.input, got, tt.want)
			}
		})
	}
}
