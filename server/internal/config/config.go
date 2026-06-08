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
	// EnvConnectorWebhookSecret is the HMAC-SHA256 key inbound connector
	// webhooks must sign their body with. Empty disables verification
	// (dev mode / upstream-terminated auth). This is a secret and is
	// never logged.
	EnvConnectorWebhookSecret = "KNOWLEDGE_CONNECTOR_WEBHOOK_SECRET"
	// EnvConnectorRateRPS overrides the default per-provider connector-call
	// rate (calls/second). Unset keeps the connector package default.
	EnvConnectorRateRPS = "KNOWLEDGE_CONNECTOR_RATE_RPS"
	// EnvConnectorRateBurst overrides the default per-provider connector-call
	// burst allowance. Unset keeps the connector package default.
	EnvConnectorRateBurst = "KNOWLEDGE_CONNECTOR_RATE_BURST"
	// EnvConnectorRateOverrides sets per-provider rate-limit overrides as a
	// comma-separated list of "kind=rps:burst" entries (e.g.
	// "github=10:20,slack=5:10"). Each kind is the on-the-wire snake_case
	// connector kind; rps and burst must both be positive.
	EnvConnectorRateOverrides = "KNOWLEDGE_CONNECTOR_RATE_OVERRIDES"
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
	// ConnectorWebhookSecret is the HMAC-SHA256 signing key inbound
	// connector webhooks must sign their body with. Empty disables
	// verification. This is a secret and is never logged.
	ConnectorWebhookSecret string
	// ConnectorRateRPS is the default per-provider connector-call rate.
	// Zero means "use the connector package default".
	ConnectorRateRPS float64
	// ConnectorRateBurst is the default per-provider connector-call burst.
	// Zero means "use the connector package default".
	ConnectorRateBurst int
	// ConnectorRateOverrides holds per-provider rate-limit overrides keyed
	// by connector kind. Empty means every provider uses the default.
	ConnectorRateOverrides []ProviderRateOverride
}

// ProviderRateOverride is a per-provider connector-call rate-limit
// override parsed from [EnvConnectorRateOverrides].
type ProviderRateOverride struct {
	// Kind is the on-the-wire snake_case connector kind (e.g. "github").
	Kind string
	// RPS is the sustained calls/second for this provider.
	RPS float64
	// Burst is the instantaneous burst allowance for this provider.
	Burst int
}

// Load reads configuration from the process environment, applying
// defaults for anything unset. It returns an error only for values
// that are present but malformed (e.g. a non-numeric rate limit).
func Load() (*Config, error) {
	c := &Config{
		APIKey:                 os.Getenv(EnvAPIKey),
		JWTSecret:              os.Getenv(EnvJWTSecret),
		ListenAddr:             envOr(EnvListenAddr, defaultListenAddr),
		SubstrateURL:           strings.TrimRight(envOr(EnvSubstrateAddr, defaultSubstrateURL), "/"),
		SubstrateStandbyURL:    strings.TrimRight(os.Getenv(EnvSubstrateStandbyAddr), "/"),
		DatabaseURL:            os.Getenv(EnvDatabaseURL),
		NATSURL:                os.Getenv(EnvNATSURL),
		RateIPRPS:              defaultRateIPRPS,
		RateTenantRPS:          defaultRateTenantRPS,
		RateBurst:              defaultRateBurst,
		SyncInterval:           defaultSyncInterval,
		PublicBaseURL:          strings.TrimRight(envOr(EnvPublicBaseURL, defaultPublicBaseURL), "/"),
		ConnectorWebhookSecret: os.Getenv(EnvConnectorWebhookSecret),
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
	if c.ConnectorRateRPS, err = optionalPositiveFloat(EnvConnectorRateRPS); err != nil {
		return nil, err
	}
	if c.ConnectorRateBurst, err = optionalPositiveInt(EnvConnectorRateBurst); err != nil {
		return nil, err
	}
	if c.ConnectorRateOverrides, err = parseRateOverrides(os.Getenv(EnvConnectorRateOverrides)); err != nil {
		return nil, err
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
		"listen_addr":              c.ListenAddr,
		"substrate_url":            c.SubstrateURL,
		"substrate_standby_url":    c.SubstrateStandbyURL,
		"database_url":             redact(c.DatabaseURL),
		"nats_url":                 redact(c.NATSURL),
		"api_key":                  redact(c.APIKey),
		"jwt_secret":               redact(c.JWTSecret),
		"rate_ip_rps":              c.RateIPRPS,
		"rate_tenant_rps":          c.RateTenantRPS,
		"rate_burst":               c.RateBurst,
		"cors_origins":             c.CORSOrigins,
		"trusted_proxies":          c.TrustedProxies,
		"sync_interval":            c.SyncInterval.String(),
		"public_base_url":          c.PublicBaseURL,
		"connector_webhook_secret": redact(c.ConnectorWebhookSecret),
		"connector_rate_rps":       c.ConnectorRateRPS,
		"connector_rate_burst":     c.ConnectorRateBurst,
		"connector_rate_overrides": len(c.ConnectorRateOverrides),
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

// optionalPositiveFloat parses an optional float env var. Unset returns
// 0 (caller treats it as "use default"); a present value must be a valid
// positive float.
func optionalPositiveFloat(key string) (float64, error) {
	v := os.Getenv(key)
	if v == "" {
		return 0, nil
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

// optionalPositiveInt parses an optional integer env var. Unset returns
// 0 (caller treats it as "use default"); a present value must be a valid
// positive integer.
func optionalPositiveInt(key string) (int, error) {
	v := os.Getenv(key)
	if v == "" {
		return 0, nil
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

// parseRateOverrides parses a comma-separated list of "kind=rps:burst"
// per-provider rate-limit overrides. An empty string yields no overrides.
func parseRateOverrides(s string) ([]ProviderRateOverride, error) {
	if strings.TrimSpace(s) == "" {
		return nil, nil
	}
	entries := splitTrim(s)
	out := make([]ProviderRateOverride, 0, len(entries))
	seen := make(map[string]struct{}, len(entries))
	for _, e := range entries {
		kind, spec, ok := strings.Cut(e, "=")
		kind = strings.TrimSpace(kind)
		if !ok || kind == "" {
			return nil, fmt.Errorf("config: %s entry %q must be \"kind=rps:burst\"", EnvConnectorRateOverrides, e)
		}
		if _, dup := seen[kind]; dup {
			return nil, fmt.Errorf("config: %s has duplicate kind %q", EnvConnectorRateOverrides, kind)
		}
		rpsStr, burstStr, ok := strings.Cut(spec, ":")
		if !ok {
			return nil, fmt.Errorf("config: %s entry %q must be \"kind=rps:burst\"", EnvConnectorRateOverrides, e)
		}
		rps, err := strconv.ParseFloat(strings.TrimSpace(rpsStr), 64)
		if err != nil || rps <= 0 {
			return nil, fmt.Errorf("config: %s entry %q has invalid rps (must be a positive float)", EnvConnectorRateOverrides, e)
		}
		burst, err := strconv.Atoi(strings.TrimSpace(burstStr))
		if err != nil || burst <= 0 {
			return nil, fmt.Errorf("config: %s entry %q has invalid burst (must be a positive integer)", EnvConnectorRateOverrides, e)
		}
		seen[kind] = struct{}{}
		out = append(out, ProviderRateOverride{Kind: kind, RPS: rps, Burst: burst})
	}
	return out, nil
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
