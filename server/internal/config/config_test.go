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
		EnvConnectorWebhookSecret, EnvConnectorRateRPS, EnvConnectorRateBurst, EnvConnectorRateOverrides,
	} {
		t.Setenv(k, "")
	}
	c, err := Load()
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if c.ConnectorWebhookSecret != "" {
		t.Errorf("ConnectorWebhookSecret should default empty, got %q", c.ConnectorWebhookSecret)
	}
	if c.ConnectorRateRPS != 0 || c.ConnectorRateBurst != 0 {
		t.Errorf("connector rate defaults should be zero (defer to package), got %v %v", c.ConnectorRateRPS, c.ConnectorRateBurst)
	}
	if len(c.ConnectorRateOverrides) != 0 {
		t.Errorf("ConnectorRateOverrides should default empty, got %v", c.ConnectorRateOverrides)
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

func TestLoadConnectorRateLimiting(t *testing.T) {
	t.Setenv(EnvConnectorWebhookSecret, "s3cr3t")
	t.Setenv(EnvConnectorRateRPS, "7.5")
	t.Setenv(EnvConnectorRateBurst, "15")
	t.Setenv(EnvConnectorRateOverrides, "github=10:20, slack=5:8")
	c, err := Load()
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if c.ConnectorWebhookSecret != "s3cr3t" {
		t.Errorf("ConnectorWebhookSecret = %q", c.ConnectorWebhookSecret)
	}
	if c.ConnectorRateRPS != 7.5 || c.ConnectorRateBurst != 15 {
		t.Errorf("connector default rate wrong: %v %v", c.ConnectorRateRPS, c.ConnectorRateBurst)
	}
	want := map[string]ProviderRateOverride{
		"github": {Kind: "github", RPS: 10, Burst: 20},
		"slack":  {Kind: "slack", RPS: 5, Burst: 8},
	}
	if len(c.ConnectorRateOverrides) != len(want) {
		t.Fatalf("overrides = %v, want %d entries", c.ConnectorRateOverrides, len(want))
	}
	for _, got := range c.ConnectorRateOverrides {
		if w, ok := want[got.Kind]; !ok || got != w {
			t.Errorf("override %q = %+v, want %+v", got.Kind, got, w)
		}
	}
}

func TestLoadConnectorRateOverridesInvalid(t *testing.T) {
	for _, spec := range []string{
		"github",                  // missing =rps:burst
		"github=10",               // missing :burst
		"github=abc:20",           // non-numeric rps
		"github=10:xyz",           // non-numeric burst
		"github=0:20",             // non-positive rps
		"github=10:0",             // non-positive burst
		"=10:20",                  // empty kind
		"github=10:20,github=5:5", // duplicate kind
	} {
		t.Setenv(EnvConnectorRateOverrides, spec)
		if _, err := Load(); err == nil {
			t.Errorf("expected error for overrides %q", spec)
		}
	}
}

func TestRedactedHidesSecrets(t *testing.T) {
	c := &Config{APIKey: "supersecret", JWTSecret: "jwtsecret", DatabaseURL: "postgres://x", ConnectorWebhookSecret: "whsecret"}
	r := c.Redacted()
	if r["api_key"] == "supersecret" || r["jwt_secret"] == "jwtsecret" {
		t.Fatal("secret leaked in Redacted()")
	}
	if r["api_key"] != "<redacted>" {
		t.Errorf("api_key = %v", r["api_key"])
	}
	if r["connector_webhook_secret"] != "<redacted>" {
		t.Errorf("connector_webhook_secret leaked: %v", r["connector_webhook_secret"])
	}
}
