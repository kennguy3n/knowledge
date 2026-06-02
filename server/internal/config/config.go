// Package config provides environment-variable-based configuration
// following the RouterConfig pattern from the Rust crates.
package config

import (
	"fmt"
	"os"
	"strconv"
	"time"
)

// Config holds all server configuration loaded from environment variables.
type Config struct {
	// Gateway
	ListenAddr string
	APIKey     string

	// JWT
	JWTSecret string

	// Rate limiting
	RateLimitPerIP     int
	RateLimitPerTenant int

	// Substrate server
	SubstrateURL string

	// PostgreSQL
	DatabaseURL string

	// NATS
	NatsURL string

	// MinIO / S3
	S3Endpoint  string
	S3AccessKey string
	S3SecretKey string
	S3Bucket    string
	S3UseSSL    bool

	// Timeouts
	SubstrateTimeout time.Duration
	ShutdownTimeout  time.Duration
}

// Load reads configuration from environment variables with sensible defaults.
func Load() (*Config, error) {
	c := &Config{
		ListenAddr:         envOrDefault("LISTEN_ADDR", ":8080"),
		APIKey:             os.Getenv("KNOWLEDGE_API_KEY"),
		JWTSecret:          os.Getenv("JWT_SECRET"),
		RateLimitPerIP:     envIntOrDefault("RATE_LIMIT_PER_IP", 100),
		RateLimitPerTenant: envIntOrDefault("RATE_LIMIT_PER_TENANT", 1000),
		SubstrateURL:       envOrDefault("SUBSTRATE_URL", "http://127.0.0.1:9090"),
		DatabaseURL:        os.Getenv("DATABASE_URL"),
		NatsURL:            envOrDefault("NATS_URL", "nats://127.0.0.1:4222"),
		S3Endpoint:         envOrDefault("S3_ENDPOINT", "127.0.0.1:9000"),
		S3AccessKey:        os.Getenv("S3_ACCESS_KEY"),
		S3SecretKey:        os.Getenv("S3_SECRET_KEY"),
		S3Bucket:           envOrDefault("S3_BUCKET", "knowledge"),
		S3UseSSL:           envBoolOrDefault("S3_USE_SSL", false),
		SubstrateTimeout:   envDurationOrDefault("SUBSTRATE_TIMEOUT", 30*time.Second),
		ShutdownTimeout:    envDurationOrDefault("SHUTDOWN_TIMEOUT", 15*time.Second),
	}

	if c.APIKey == "" {
		return nil, fmt.Errorf("KNOWLEDGE_API_KEY is required")
	}

	return c, nil
}

func envOrDefault(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
}

func envIntOrDefault(key string, fallback int) int {
	if v := os.Getenv(key); v != "" {
		if n, err := strconv.Atoi(v); err == nil {
			return n
		}
	}
	return fallback
}

func envBoolOrDefault(key string, fallback bool) bool {
	if v := os.Getenv(key); v != "" {
		if b, err := strconv.ParseBool(v); err == nil {
			return b
		}
	}
	return fallback
}

func envDurationOrDefault(key string, fallback time.Duration) time.Duration {
	if v := os.Getenv(key); v != "" {
		if d, err := time.ParseDuration(v); err == nil {
			return d
		}
	}
	return fallback
}
