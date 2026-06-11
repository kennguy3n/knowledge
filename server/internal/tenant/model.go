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
	// Quota bounds the tenant's resource consumption. Per-tenant
	// overrides are applied by editing this field via the config API.
	Quota Quota `json:"quota"`
}

// DefaultConfig returns the config applied to a freshly created tenant.
func DefaultConfig() Config {
	return Config{
		ConnectorLimit: 10,
		SynthesisTier:  TierStandard,
		RetentionDays:  365,
		Quota:          DefaultQuota(),
	}
}

// Default per-tenant quotas, sized so ~5,000 SME tenants can share the
// gateway without any single tenant exhausting capacity. They are
// deliberately generous for normal SME usage but bound runaway clients;
// operators raise or lower them per tenant via the config API.
const (
	defaultRequestsPerMin      = 1200     // 20 req/s sustained average
	defaultSynthesesPerDay     = 500      // CPU-bound synthesis budget
	defaultStorageSoftCapBytes = 50 << 30 // 50 GiB advisory soft cap
)

// Quota bounds a tenant's resource consumption so a single SME tenant
// cannot exhaust shared capacity. A non-positive field is treated as
// "unset" and replaced by the default for that dimension (see
// [Quota.Normalized]); this is fail-closed — a tenant can never end up
// with an unbounded quota by accident, and tenants persisted before
// quotas existed (all-zero) transparently inherit the safe defaults.
type Quota struct {
	// RequestsPerMin caps total API requests per tenant per minute.
	RequestsPerMin int `json:"requests_per_min"`
	// SynthesesPerDay caps synthesis triggers per tenant per 24h.
	SynthesesPerDay int `json:"syntheses_per_day"`
	// StorageSoftCapBytes is an advisory per-tenant storage ceiling.
	StorageSoftCapBytes int64 `json:"storage_soft_cap_bytes"`
}

// DefaultQuota returns the quota applied to a freshly created tenant.
func DefaultQuota() Quota {
	return Quota{
		RequestsPerMin:      defaultRequestsPerMin,
		SynthesesPerDay:     defaultSynthesesPerDay,
		StorageSoftCapBytes: defaultStorageSoftCapBytes,
	}
}

// Normalized returns q with any non-positive (unset) field replaced by
// its default, guaranteeing a bounded quota in every dimension.
func (q Quota) Normalized() Quota {
	d := DefaultQuota()
	if q.RequestsPerMin <= 0 {
		q.RequestsPerMin = d.RequestsPerMin
	}
	if q.SynthesesPerDay <= 0 {
		q.SynthesesPerDay = d.SynthesesPerDay
	}
	if q.StorageSoftCapBytes <= 0 {
		q.StorageSoftCapBytes = d.StorageSoftCapBytes
	}
	return q
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
