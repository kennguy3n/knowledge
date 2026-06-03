// Package tenant is the Go tenant service: tenant CRUD with Postgres
// persistence (parameterised queries only), per-tenant configuration,
// member provisioning, and per-tenant encryption-key management backed
// by the Rust crypto crate via substrate_server.
package tenant

import "time"

// SynthesisTier selects the synthesis quality/cost tier for a tenant.
type SynthesisTier string

// Synthesis tiers.
const (
	TierBasic    SynthesisTier = "basic"
	TierStandard SynthesisTier = "standard"
	TierPremium  SynthesisTier = "premium"
)

// Valid reports whether t is a recognised tier.
func (t SynthesisTier) Valid() bool {
	switch t {
	case TierBasic, TierStandard, TierPremium:
		return true
	default:
		return false
	}
}

// Config holds per-tenant policy knobs.
type Config struct {
	// ConnectorLimit caps the number of connector instances.
	ConnectorLimit int `json:"connector_limit"`
	// SynthesisTier selects the synthesis tier.
	SynthesisTier SynthesisTier `json:"synthesis_tier"`
	// RetentionDays is the audit/evidence retention window in days.
	RetentionDays int `json:"retention_days"`
}

// DefaultConfig returns the config applied to a freshly created tenant.
func DefaultConfig() Config {
	return Config{ConnectorLimit: 10, SynthesisTier: TierStandard, RetentionDays: 365}
}

// CryptoKey is a tenant's public encryption-key material. Only the
// public half is persisted; the secret half never leaves the crypto
// boundary and is never stored or logged here.
type CryptoKey struct {
	// Algorithm is the KEM algorithm tag (e.g. "x25519-ml-kem-768").
	Algorithm string `json:"algorithm"`
	// PublicKeyHex is the hex-encoded public key.
	PublicKeyHex string `json:"public_key_hex"`
}

// Tenant is a top-level B2B account.
type Tenant struct {
	ID        string    `json:"id"`
	Name      string    `json:"name"`
	Config    Config    `json:"config"`
	Key       CryptoKey `json:"key"`
	CreatedAt time.Time `json:"created_at"`
}

// MemberStatus is a member's lifecycle state.
type MemberStatus string

// Member lifecycle states.
const (
	StatusInvited   MemberStatus = "invited"
	StatusActive    MemberStatus = "active"
	StatusSuspended MemberStatus = "suspended"
)

// Member is a user provisioned into a tenant.
type Member struct {
	TenantID  string       `json:"tenant_id"`
	UserID    string       `json:"user_id"`
	Email     string       `json:"email"`
	Status    MemberStatus `json:"status"`
	UpdatedAt time.Time    `json:"updated_at"`
}
