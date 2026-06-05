package config

import (
	"testing"
	"time"
)

func TestLoadDefaults(t *testing.T) {
	// No env vars set: clear the ones we care about.
	for _, k := range []string{
		EnvAPIKey, EnvJWTSecret, EnvListenAddr, EnvSubstrateAddr, EnvSubstrateStandbyAddr,
		EnvDatabaseURL, EnvNATSURL, EnvRateIP, EnvRateTenant, EnvRateBurst, EnvCORSOrigins,
		EnvSyncInterval, EnvPublicBaseURL,
	} {
		t.Setenv(k, "")
	}
	c, err := Load()
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if c.ListenAddr != ":8080" {
		t.Errorf("ListenAddr = %q", c.ListenAddr)
	}
	if c.SubstrateURL != "http://127.0.0.1:9090" {
		t.Errorf("SubstrateURL = %q", c.SubstrateURL)
	}
	if c.SubstrateStandbyURL != "" {
		t.Errorf("SubstrateStandbyURL should default empty, got %q", c.SubstrateStandbyURL)
	}
	if c.RateIPRPS != defaultRateIPRPS || c.RateTenantRPS != defaultRateTenantRPS {
		t.Errorf("rate defaults wrong: %v %v", c.RateIPRPS, c.RateTenantRPS)
	}
	if c.SyncInterval != 15*time.Minute {
		t.Errorf("SyncInterval = %v", c.SyncInterval)
	}
}

func TestLoadOverrides(t *testing.T) {
	t.Setenv(EnvListenAddr, ":9999")
	t.Setenv(EnvSubstrateAddr, "http://127.0.0.1:7000/")
	t.Setenv(EnvSubstrateStandbyAddr, "http://127.0.0.1:7001/")
	t.Setenv(EnvRateIP, "10")
	t.Setenv(EnvRateBurst, "5")
	t.Setenv(EnvSyncInterval, "30s")
	t.Setenv(EnvCORSOrigins, "https://a.example, https://b.example")
	c, err := Load()
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if c.ListenAddr != ":9999" {
		t.Errorf("ListenAddr = %q", c.ListenAddr)
	}
	if c.SubstrateURL != "http://127.0.0.1:7000" {
		t.Errorf("SubstrateURL trailing slash not trimmed: %q", c.SubstrateURL)
	}
	if c.SubstrateStandbyURL != "http://127.0.0.1:7001" {
		t.Errorf("SubstrateStandbyURL trailing slash not trimmed: %q", c.SubstrateStandbyURL)
	}
	if c.RateIPRPS != 10 || c.RateBurst != 5 {
		t.Errorf("override rates wrong: %v %v", c.RateIPRPS, c.RateBurst)
	}
	if c.SyncInterval != 30*time.Second {
		t.Errorf("SyncInterval = %v", c.SyncInterval)
	}
	if len(c.CORSOrigins) != 2 {
		t.Errorf("CORSOrigins = %v", c.CORSOrigins)
	}
}

func TestLoadInvalid(t *testing.T) {
	t.Setenv(EnvRateIP, "not-a-number")
	if _, err := Load(); err == nil {
		t.Fatal("expected error for invalid rate")
	}
}

func TestRedactedHidesSecrets(t *testing.T) {
	c := &Config{APIKey: "supersecret", JWTSecret: "jwtsecret", DatabaseURL: "postgres://x"}
	r := c.Redacted()
	if r["api_key"] == "supersecret" || r["jwt_secret"] == "jwtsecret" {
		t.Fatal("secret leaked in Redacted()")
	}
	if r["api_key"] != "<redacted>" {
		t.Errorf("api_key = %v", r["api_key"])
	}
}
