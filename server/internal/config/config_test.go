package config

import (
	"os"
	"testing"
	"time"
)

func TestLoad_RequiresAPIKey(t *testing.T) {
	os.Unsetenv("KNOWLEDGE_API_KEY")
	_, err := Load()
	if err == nil {
		t.Fatal("expected error when KNOWLEDGE_API_KEY is not set")
	}
}

func TestLoad_Defaults(t *testing.T) {
	t.Setenv("KNOWLEDGE_API_KEY", "test-key")
	cfg, err := Load()
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}

	if cfg.ListenAddr != ":8080" {
		t.Errorf("ListenAddr = %q, want %q", cfg.ListenAddr, ":8080")
	}
	if cfg.SubstrateURL != "http://127.0.0.1:9090" {
		t.Errorf("SubstrateURL = %q, want %q", cfg.SubstrateURL, "http://127.0.0.1:9090")
	}
	if cfg.RateLimitPerIP != 100 {
		t.Errorf("RateLimitPerIP = %d, want 100", cfg.RateLimitPerIP)
	}
	if cfg.RateLimitPerTenant != 1000 {
		t.Errorf("RateLimitPerTenant = %d, want 1000", cfg.RateLimitPerTenant)
	}
	if cfg.SubstrateTimeout != 30*time.Second {
		t.Errorf("SubstrateTimeout = %v, want 30s", cfg.SubstrateTimeout)
	}
	if cfg.ShutdownTimeout != 15*time.Second {
		t.Errorf("ShutdownTimeout = %v, want 15s", cfg.ShutdownTimeout)
	}
}

func TestLoad_EnvOverrides(t *testing.T) {
	t.Setenv("KNOWLEDGE_API_KEY", "test-key")
	t.Setenv("LISTEN_ADDR", ":9090")
	t.Setenv("RATE_LIMIT_PER_IP", "50")
	t.Setenv("SUBSTRATE_TIMEOUT", "10s")

	cfg, err := Load()
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}

	if cfg.ListenAddr != ":9090" {
		t.Errorf("ListenAddr = %q, want %q", cfg.ListenAddr, ":9090")
	}
	if cfg.RateLimitPerIP != 50 {
		t.Errorf("RateLimitPerIP = %d, want 50", cfg.RateLimitPerIP)
	}
	if cfg.SubstrateTimeout != 10*time.Second {
		t.Errorf("SubstrateTimeout = %v, want 10s", cfg.SubstrateTimeout)
	}
}

func TestEnvIntOrDefault_InvalidFallback(t *testing.T) {
	t.Setenv("TEST_INT", "not-a-number")
	got := envIntOrDefault("TEST_INT", 42)
	if got != 42 {
		t.Errorf("envIntOrDefault = %d, want 42", got)
	}
}

func TestEnvBoolOrDefault(t *testing.T) {
	tests := []struct {
		name     string
		envVal   string
		fallback bool
		want     bool
	}{
		{"true", "true", false, true},
		{"false", "false", true, false},
		{"empty", "", true, true},
		{"invalid", "maybe", false, false},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if tt.envVal != "" {
				t.Setenv("TEST_BOOL", tt.envVal)
			} else {
				os.Unsetenv("TEST_BOOL")
			}
			got := envBoolOrDefault("TEST_BOOL", tt.fallback)
			if got != tt.want {
				t.Errorf("envBoolOrDefault = %v, want %v", got, tt.want)
			}
		})
	}
}

func TestEnvDurationOrDefault(t *testing.T) {
	t.Setenv("TEST_DUR", "5s")
	got := envDurationOrDefault("TEST_DUR", time.Minute)
	if got != 5*time.Second {
		t.Errorf("envDurationOrDefault = %v, want 5s", got)
	}

	t.Setenv("TEST_DUR", "invalid")
	got = envDurationOrDefault("TEST_DUR", time.Minute)
	if got != time.Minute {
		t.Errorf("envDurationOrDefault = %v, want 1m0s", got)
	}
}
