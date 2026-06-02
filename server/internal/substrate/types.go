package substrate

import "encoding/json"

// IngestRequest mirrors substrate_server's `POST /ingest` body. Source
// and Importance are the FFI enum string tags (PascalCase), e.g.
// "Manual"/"Slack" and "Critical"/"Important"/"Useful"/"Noise".
type IngestRequest struct {
	ScopeID    string `json:"scope_id"`
	Body       string `json:"body"`
	Source     string `json:"source"`
	Importance string `json:"importance"`
}

// IDResponse is the `{ "id": "<uuid>" }` reply from create-style routes.
type IDResponse struct {
	ID string `json:"id"`
}

// QueryRequest mirrors `POST /query`.
type QueryRequest struct {
	ScopeID   string `json:"scope_id"`
	QueryText string `json:"query_text"`
	Limit     uint32 `json:"limit"`
}

// MemoryFilter mirrors `ffi::MemoryFilter`. State is an optional memory
// state tag; nil means "any state".
type MemoryFilter struct {
	State      *string `json:"state"`
	PinnedOnly bool    `json:"pinned_only"`
}

// ListMemoriesRequest mirrors `POST /memories`.
type ListMemoriesRequest struct {
	ScopeID string       `json:"scope_id"`
	Filter  MemoryFilter `json:"filter"`
}

// SynthesisTriggerRequest mirrors `POST /synthesis/trigger`. Trigger is
// the PascalCase `SynthesisTrigger` tag (e.g. "ManualUserAction").
type SynthesisTriggerRequest struct {
	ScopeID string `json:"scope_id"`
	Trigger string `json:"trigger"`
}

// RecentSynthesisRequest mirrors `POST /synthesis/recent`.
type RecentSynthesisRequest struct {
	ScopeID string `json:"scope_id"`
}

// CreateConnectorRequest mirrors `POST /connectors`. Kind is the
// PascalCase `ConnectorKindTag` (e.g. "GoogleDrive", "Slack").
type CreateConnectorRequest struct {
	Kind       string `json:"kind"`
	ScopeID    string `json:"scope_id"`
	ConfigJSON string `json:"config_json"`
}

// AuthenticateRequest mirrors `POST /connectors/{id}/authenticate`.
type AuthenticateRequest struct {
	AuthCode string `json:"auth_code"`
}

// FetchContentRequest mirrors `POST /connector/fetch_content`.
type FetchContentRequest struct {
	InstanceID string `json:"instance_id"`
	ContentRef string `json:"content_ref"`
}

// ObjectRef is the object side of a Zanzibar relation tuple. ObjectType
// is snake_case (e.g. "tenant", "channel", "user").
type ObjectRef struct {
	ObjectType string `json:"object_type"`
	ObjectID   string `json:"object_id"`
}

// SubjectRef is the subject side of a relation tuple. SubjectRelation
// is the optional userset rewrite (nil for a direct subject).
type SubjectRef struct {
	SubjectType     string  `json:"subject_type"`
	SubjectID       string  `json:"subject_id"`
	SubjectRelation *string `json:"subject_relation"`
}

// RelationTuple mirrors `permission_service::RelationTuple`. Relation is
// snake_case (e.g. "owner", "admin", "member", "viewer").
type RelationTuple struct {
	Object   ObjectRef  `json:"object"`
	Relation string     `json:"relation"`
	Subject  SubjectRef `json:"subject"`
}

// PermissionCheckResponse is the reply from `POST /permission/check`.
type PermissionCheckResponse struct {
	Allowed bool `json:"allowed"`
}

// HybridKeypair is the reply from `POST /crypto/hybrid_keypair`. The
// secret key is loopback-only material and must never be logged.
type HybridKeypair struct {
	Algorithm    string `json:"algorithm"`
	PublicKeyHex string `json:"public_key_hex"`
	SecretKeyHex string `json:"secret_key_hex"`
}

// SigningKeypair is the reply from `POST /crypto/signing_keypair`
// (`ffi::FfiKeypair`).
type SigningKeypair struct {
	Algorithm  string `json:"algorithm"`
	PublicKey  []byte `json:"public_key"`
	PrivateKey []byte `json:"private_key"`
}

// ApprovedConcept is the subset of an export-approved concept the
// server inspects when rendering summaries. Unmodelled fields
// (provenance, scope_id, …) are ignored on decode.
type ApprovedConcept struct {
	ConceptID        string `json:"concept_id"`
	Label            string `json:"label"`
	Definition       string `json:"definition"`
	SensitivityClass string `json:"sensitivity_class"`
}

// ExportRejection is one rejected concept with its reason.
type ExportRejection struct {
	ConceptID string          `json:"concept_id"`
	Reason    json.RawMessage `json:"reason"`
}

// ExportDecision mirrors `export_plane::ExportDecision`.
type ExportDecision struct {
	Approved         []ApprovedConcept `json:"approved"`
	Rejected         []ExportRejection `json:"rejected"`
	Warnings         []string          `json:"warnings"`
	AllowRawEvidence bool              `json:"allow_raw_evidence"`
}

// ExportEvaluateRequest mirrors `POST /export/evaluate`. Profile and
// Policy are forwarded verbatim; the server does not need to interpret
// their internal structure.
type ExportEvaluateRequest struct {
	Policy  json.RawMessage `json:"policy,omitempty"`
	Profile json.RawMessage `json:"profile"`
}
