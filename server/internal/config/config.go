// Package config loads the API gateway and core-service configuration
// from environment variables. Secrets are read from the environment
// and never logged; see [Config.Redacted] for the log-safe view.
package config

import (
	"fmt"
	"os"
	"strconv"
	"strings"
	"time"
)

// Environment variable names. Centralised so tests and docs stay in
// sync with the loader.
const (
	// EnvAPIKey is the static bearer token the gateway accepts for
	// service-to-service / admin calls.
	EnvAPIKey = "KNOWLEDGE_API_KEY"
	// EnvJWTSecret is the HMAC secret used to validate tenant JWTs.
	EnvJWTSecret = "KNOWLEDGE_JWT_SECRET"
	// EnvListenAddr is the gateway's public bind address.
	EnvListenAddr = "KNOWLEDGE_GATEWAY_ADDR"
	// EnvSubstrateAddr is the substrate_server loopback base URL.
	EnvSubstrateAddr = "KNOWLEDGE_SUBSTRATE_URL"
	// EnvSubstrateStandbyAddr is the optional base URL of a standby
	// substrate in an active-passive HA deployment (WS2). When set, the
	// substrate client routes reads across both nodes and fails writes
	// over to whichever node currently reports `role = primary`. Empty
	// keeps the single-substrate behaviour.
	EnvSubstrateStandbyAddr = "KNOWLEDGE_SUBSTRATE_URL_STANDBY"
	// EnvDatabaseURL is the Postgres connection string.
	EnvDatabaseURL = "KNOWLEDGE_DATABASE_URL"
	// EnvNATSURL is the NATS / JetStream connection URL.
	EnvNATSURL = "KNOWLEDGE_NATS_URL"
	// EnvRateIP is the per-IP request-per-second budget.
	EnvRateIP = "KNOWLEDGE_RATE_IP_RPS"
	// EnvRateTenant is the per-tenant request-per-second budget.
	EnvRateTenant = "KNOWLEDGE_RATE_TENANT_RPS"
	// EnvRateBurst is the token-bucket burst allowance.
	EnvRateBurst = "KNOWLEDGE_RATE_BURST"
	// EnvCORSOrigins is a comma-separated allow-list of origins.
	EnvCORSOrigins = "KNOWLEDGE_CORS_ORIGINS"
	// EnvTrustedProxies is a comma-separated list of trusted reverse-proxy
	// CIDRs or IPs. When unset, X-Forwarded-For is ignored and the per-IP
	// rate limiter keys on the transport peer (secure default for a
	// directly-exposed gateway).
	EnvTrustedProxies = "KNOWLEDGE_TRUSTED_PROXIES"
	// EnvSyncInterval is the default per-connector sync cadence.
	EnvSyncInterval = "KNOWLEDGE_SYNC_INTERVAL"
	// EnvPublicBaseURL is the externally reachable base URL, used to
	// build OAuth redirect and webhook callback URLs.
	EnvPublicBaseURL = "KNOWLEDGE_PUBLIC_BASE_URL"
)

// Defaults applied when an environment variable is unset or empty.
const (
	defaultListenAddr    = ":8080"
	defaultSubstrateURL  = "http://127.0.0.1:9090"
	defaultRateIPRPS     = 50.0
	defaultRateTenantRPS = 200.0
	defaultRateBurst     = 100
	defaultSyncInterval  = 15 * time.Minute
	defaultPublicBaseURL = "http://127.0.0.1:8080"
)

// Config is the fully-resolved server configuration.
type Config struct {
	// APIKey is the static bearer token for admin / service calls. May
	// be empty in development, in which case bearer auth is disabled
	// (JWT auth still applies). This is a secret and is never logged.
	APIKey string
	// JWTSecret validates tenant JWTs. Empty disables JWT validation.
	JWTSecret string
	// ListenAddr is the gateway bind address (e.g. ":8080").
	ListenAddr string
	// SubstrateURL is the substrate_server loopback base URL.
	SubstrateURL string
	// SubstrateStandbyURL is the optional standby substrate base URL
	// for active-passive HA. Empty keeps single-substrate routing.
	SubstrateStandbyURL string
	// DatabaseURL is the Postgres DSN. Empty selects the in-memory
	// store (development / unit tests only).
	DatabaseURL string
	// NATSURL is the JetStream URL. Empty disables the audit consumer.
	NATSURL string
	// RateIPRPS is the per-IP token-bucket refill rate.
	RateIPRPS float64
	// RateTenantRPS is the per-tenant token-bucket refill rate.
	RateTenantRPS float64
	// RateBurst is the token-bucket burst size for both limiters.
	RateBurst int
	// CORSOrigins is the parsed origin allow-list. Empty means "*".
	CORSOrigins []string
	// TrustedProxies is the parsed list of trusted reverse-proxy CIDRs/IPs
	// from which X-Forwarded-For is honoured. Empty means trust none.
	TrustedProxies []string
	// SyncInterval is the default connector sync cadence.
	SyncInterval time.Duration
	// PublicBaseURL is the externally reachable base URL.
	PublicBaseURL string
}

// Load reads configuration from the process environment, applying
// defaults for anything unset. It returns an error only for values
// that are present but malformed (e.g. a non-numeric rate limit).
func Load() (*Config, error) {
	c := &Config{
		APIKey:              os.Getenv(EnvAPIKey),
		JWTSecret:           os.Getenv(EnvJWTSecret),
		ListenAddr:          envOr(EnvListenAddr, defaultListenAddr),
		SubstrateURL:        strings.TrimRight(envOr(EnvSubstrateAddr, defaultSubstrateURL), "/"),
		SubstrateStandbyURL: strings.TrimRight(os.Getenv(EnvSubstrateStandbyAddr), "/"),
		DatabaseURL:         os.Getenv(EnvDatabaseURL),
		NATSURL:             os.Getenv(EnvNATSURL),
		RateIPRPS:           defaultRateIPRPS,
		RateTenantRPS:       defaultRateTenantRPS,
		RateBurst:           defaultRateBurst,
		SyncInterval:        defaultSyncInterval,
		PublicBaseURL:       strings.TrimRight(envOr(EnvPublicBaseURL, defaultPublicBaseURL), "/"),
	}

	var err error
	if c.RateIPRPS, err = floatOr(EnvRateIP, defaultRateIPRPS); err != nil {
		return nil, err
	}
	if c.RateTenantRPS, err = floatOr(EnvRateTenant, defaultRateTenantRPS); err != nil {
		return nil, err
	}
	if c.RateBurst, err = intOr(EnvRateBurst, defaultRateBurst); err != nil {
		return nil, err
	}
	if c.SyncInterval, err = durationOr(EnvSyncInterval, defaultSyncInterval); err != nil {
		return nil, err
	}
	if origins := os.Getenv(EnvCORSOrigins); origins != "" {
		c.CORSOrigins = splitTrim(origins)
	}
	if proxies := os.Getenv(EnvTrustedProxies); proxies != "" {
		c.TrustedProxies = splitTrim(proxies)
	}
	return c, nil
}

// Redacted returns a copy safe for structured logging: every secret
// field is replaced with a fixed sentinel so accidental logging can
// never leak a credential.
func (c *Config) Redacted() map[string]any {
	redact := func(s string) string {
		if s == "" {
			return "<unset>"
		}
		return "<redacted>"
	}
	return map[string]any{
		"listen_addr":           c.ListenAddr,
		"substrate_url":         c.SubstrateURL,
		"substrate_standby_url": c.SubstrateStandbyURL,
		"database_url":          redact(c.DatabaseURL),
		"nats_url":              redact(c.NATSURL),
		"api_key":               redact(c.APIKey),
		"jwt_secret":            redact(c.JWTSecret),
		"rate_ip_rps":           c.RateIPRPS,
		"rate_tenant_rps":       c.RateTenantRPS,
		"rate_burst":            c.RateBurst,
		"cors_origins":          c.CORSOrigins,
		"trusted_proxies":       c.TrustedProxies,
		"sync_interval":         c.SyncInterval.String(),
		"public_base_url":       c.PublicBaseURL,
	}
}

func envOr(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

func floatOr(key string, def float64) (float64, error) {
	v := os.Getenv(key)
	if v == "" {
		return def, nil
	}
	f, err := strconv.ParseFloat(v, 64)
	if err != nil {
		return 0, fmt.Errorf("config: %s=%q is not a valid float: %w", key, v, err)
	}
	if f <= 0 {
		return 0, fmt.Errorf("config: %s must be positive, got %v", key, f)
	}
	return f, nil
}

func intOr(key string, def int) (int, error) {
	v := os.Getenv(key)
	if v == "" {
		return def, nil
	}
	n, err := strconv.Atoi(v)
	if err != nil {
		return 0, fmt.Errorf("config: %s=%q is not a valid integer: %w", key, v, err)
	}
	if n <= 0 {
		return 0, fmt.Errorf("config: %s must be positive, got %d", key, n)
	}
	return n, nil
}

func durationOr(key string, def time.Duration) (time.Duration, error) {
	v := os.Getenv(key)
	if v == "" {
		return def, nil
	}
	d, err := time.ParseDuration(v)
	if err != nil {
		return 0, fmt.Errorf("config: %s=%q is not a valid duration: %w", key, v, err)
	}
	if d <= 0 {
		return 0, fmt.Errorf("config: %s must be positive, got %s", key, d)
	}
	return d, nil
}

func splitTrim(s string) []string {
	parts := strings.Split(s, ",")
	out := make([]string, 0, len(parts))
	for _, p := range parts {
		if t := strings.TrimSpace(p); t != "" {
			out = append(out, t)
		}
	}
	return out
}
