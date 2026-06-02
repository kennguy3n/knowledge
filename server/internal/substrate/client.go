// Package substrate provides an HTTP client for the Rust substrate
// server running on 127.0.0.1:9090.
package substrate

import (
	"bytes"
	"context"
	"crypto/tls"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"time"
)

// Client talks to the Rust substrate server over HTTP loopback.
type Client struct {
	base   string
	http   *http.Client
}

// NewClient creates a substrate client pointing at the given base URL.
func NewClient(baseURL string, timeout time.Duration) *Client {
	return &Client{
		base: baseURL,
		http: &http.Client{
			Timeout: timeout,
			Transport: &http.Transport{
				TLSClientConfig: &tls.Config{
					MinVersion: tls.VersionTLS13,
				},
				MaxIdleConnsPerHost: 64,
				IdleConnTimeout:     90 * time.Second,
			},
		},
	}
}

// ---------------------------------------------------------------------------
// Request / response types (mirror substrate_server DTOs)
// ---------------------------------------------------------------------------

// IngestRequest is the payload for POST /substrate/ingest.
type IngestRequest struct {
	ScopeID    string `json:"scope_id"`
	Body       string `json:"body"`
	Source     string `json:"source"`
	Importance string `json:"importance"`
}

// IngestResponse is returned by POST /substrate/ingest.
type IngestResponse struct {
	EvidenceID string `json:"evidence_id"`
}

// QueryRequest is the payload for POST /substrate/query.
type QueryRequest struct {
	ScopeID   string `json:"scope_id"`
	QueryText string `json:"query_text"`
	Limit     int    `json:"limit"`
}

// QueryResult mirrors the Rust QueryResult struct.
type QueryResult struct {
	EvidenceID   string  `json:"evidence_id"`
	Score        float64 `json:"score"`
	FTSScore     float64 `json:"fts_score"`
	RecencyScore float64 `json:"recency_score"`
	VectorScore  float64 `json:"vector_score"`
	Snippet      string  `json:"snippet"`
}

// QueryResponse is returned by POST /substrate/query.
type QueryResponse struct {
	Results []QueryResult `json:"results"`
}

// EvidenceRecord mirrors the Rust EvidenceRecord struct.
type EvidenceRecord struct {
	ID          string  `json:"id"`
	ScopeID     string  `json:"scope_id"`
	Body        string  `json:"body"`
	Source      string  `json:"source"`
	CreatedAt   int64   `json:"created_at"`
	LanguageTag *string `json:"language_tag,omitempty"`
}

// MemoryRecord mirrors the Rust MemoryRecord struct.
type MemoryRecord struct {
	ID               string  `json:"id"`
	ScopeID          string  `json:"scope_id"`
	Summary          string  `json:"summary"`
	State            string  `json:"state"`
	RetentionScore   float64 `json:"retention_score"`
	CreatedAt        int64   `json:"created_at"`
	LastReinforcedAt int64   `json:"last_reinforced_at"`
}

// SynthesisTriggerRequest is the payload for POST /substrate/synthesis/trigger.
type SynthesisTriggerRequest struct {
	ScopeID string `json:"scope_id"`
	Trigger string `json:"trigger"`
}

// SynthesisTriggerResponse is returned by synthesis trigger.
type SynthesisTriggerResponse struct {
	WindowID string `json:"window_id"`
}

// EncryptRequest is the payload for POST /substrate/encrypt.
type EncryptRequest struct {
	ScopeID      string `json:"scope_id"`
	PlaintextB64 string `json:"plaintext_b64"`
}

// EncryptResponse is returned by POST /substrate/encrypt.
type EncryptResponse struct {
	CiphertextB64 string `json:"ciphertext_b64"`
}

// DecryptRequest is the payload for POST /substrate/decrypt.
type DecryptRequest struct {
	ScopeID       string `json:"scope_id"`
	CiphertextB64 string `json:"ciphertext_b64"`
}

// DecryptResponse is returned by POST /substrate/decrypt.
type DecryptResponse struct {
	PlaintextB64 string `json:"plaintext_b64"`
}

// Keypair mirrors the Rust FfiKeypair struct.
type Keypair struct {
	Algorithm  string `json:"algorithm"`
	PublicKey  []byte `json:"public_key"`
	PrivateKey []byte `json:"private_key"`
}

// HealthStatus mirrors the Rust HealthStatus struct.
type HealthStatus struct {
	CoreVersion        string            `json:"core_version"`
	UptimeSecs         uint64            `json:"uptime_secs"`
	TracingInitialized bool              `json:"tracing_initialized"`
	Subsystems         []SubsystemHealth `json:"subsystems"`
}

// SubsystemHealth mirrors the Rust SubsystemHealth struct.
type SubsystemHealth struct {
	Name   string  `json:"name"`
	Status string  `json:"status"`
	Detail *string `json:"detail,omitempty"`
}

// SubstrateError is returned when the substrate server responds with an error.
type SubstrateError struct {
	StatusCode int
	Message    string
}

func (e *SubstrateError) Error() string {
	return fmt.Sprintf("substrate %d: %s", e.StatusCode, e.Message)
}

// ---------------------------------------------------------------------------
// Client methods
// ---------------------------------------------------------------------------

// Ingest calls POST /substrate/ingest.
func (c *Client) Ingest(ctx context.Context, req *IngestRequest) (*IngestResponse, error) {
	var resp IngestResponse
	if err := c.post(ctx, "/substrate/ingest", req, &resp); err != nil {
		return nil, err
	}
	return &resp, nil
}

// Query calls POST /substrate/query.
func (c *Client) Query(ctx context.Context, req *QueryRequest) (*QueryResponse, error) {
	var resp QueryResponse
	if err := c.post(ctx, "/substrate/query", req, &resp); err != nil {
		return nil, err
	}
	return &resp, nil
}

// GetEvidence calls GET /substrate/evidence/:id.
func (c *Client) GetEvidence(ctx context.Context, id string) (*EvidenceRecord, error) {
	var resp EvidenceRecord
	if err := c.get(ctx, "/substrate/evidence/"+url.PathEscape(id), nil, &resp); err != nil {
		return nil, err
	}
	return &resp, nil
}

// Forget calls POST /substrate/forget/:id.
func (c *Client) Forget(ctx context.Context, id string) error {
	return c.postNoBody(ctx, "/substrate/forget/"+url.PathEscape(id))
}

// ForgetScope calls POST /substrate/forget-scope/:scope_id.
func (c *Client) ForgetScope(ctx context.Context, scopeID string) error {
	return c.postNoBody(ctx, "/substrate/forget-scope/"+url.PathEscape(scopeID))
}

// ListMemories calls GET /substrate/memories with query params.
func (c *Client) ListMemories(ctx context.Context, scopeID string, state *string, pinnedOnly bool) ([]MemoryRecord, error) {
	params := url.Values{}
	params.Set("scope_id", scopeID)
	if state != nil {
		params.Set("state", *state)
	}
	if pinnedOnly {
		params.Set("pinned_only", "true")
	}
	var resp []MemoryRecord
	if err := c.get(ctx, "/substrate/memories", params, &resp); err != nil {
		return nil, err
	}
	return resp, nil
}

// DecaySweep calls POST /substrate/decay-sweep/:scope_id.
func (c *Client) DecaySweep(ctx context.Context, scopeID string) (int, error) {
	var resp struct {
		ArchivedCount int `json:"archived_count"`
	}
	if err := c.post(ctx, "/substrate/decay-sweep/"+url.PathEscape(scopeID), nil, &resp); err != nil {
		return 0, err
	}
	return resp.ArchivedCount, nil
}

// GetChannelMemory calls GET /substrate/channel-memory/:scope_id.
func (c *Client) GetChannelMemory(ctx context.Context, scopeID string) (*MemoryRecord, error) {
	var resp *MemoryRecord
	if err := c.get(ctx, "/substrate/channel-memory/"+url.PathEscape(scopeID), nil, &resp); err != nil {
		return nil, err
	}
	return resp, nil
}

// TriggerSynthesis calls POST /substrate/synthesis/trigger.
func (c *Client) TriggerSynthesis(ctx context.Context, req *SynthesisTriggerRequest) (*SynthesisTriggerResponse, error) {
	var resp SynthesisTriggerResponse
	if err := c.post(ctx, "/substrate/synthesis/trigger", req, &resp); err != nil {
		return nil, err
	}
	return &resp, nil
}

// GenerateKeypair calls POST /substrate/keypair.
func (c *Client) GenerateKeypair(ctx context.Context) (*Keypair, error) {
	var resp Keypair
	if err := c.post(ctx, "/substrate/keypair", nil, &resp); err != nil {
		return nil, err
	}
	return &resp, nil
}

// Encrypt calls POST /substrate/encrypt.
func (c *Client) Encrypt(ctx context.Context, req *EncryptRequest) (*EncryptResponse, error) {
	var resp EncryptResponse
	if err := c.post(ctx, "/substrate/encrypt", req, &resp); err != nil {
		return nil, err
	}
	return &resp, nil
}

// Decrypt calls POST /substrate/decrypt.
func (c *Client) Decrypt(ctx context.Context, req *DecryptRequest) (*DecryptResponse, error) {
	var resp DecryptResponse
	if err := c.post(ctx, "/substrate/decrypt", req, &resp); err != nil {
		return nil, err
	}
	return &resp, nil
}

// Metrics calls GET /substrate/metrics (returns raw Prometheus text).
func (c *Client) Metrics(ctx context.Context) (string, error) {
	reqURL := c.base + "/substrate/metrics"
	httpReq, err := http.NewRequestWithContext(ctx, http.MethodGet, reqURL, nil)
	if err != nil {
		return "", err
	}
	resp, err := c.http.Do(httpReq)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return "", err
	}
	if resp.StatusCode != http.StatusOK {
		return "", &SubstrateError{StatusCode: resp.StatusCode, Message: string(body)}
	}
	return string(body), nil
}

// Health calls GET /substrate/health.
func (c *Client) Health(ctx context.Context) (*HealthStatus, error) {
	var resp HealthStatus
	if err := c.get(ctx, "/substrate/health", nil, &resp); err != nil {
		return nil, err
	}
	return &resp, nil
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

func (c *Client) get(ctx context.Context, path string, params url.Values, out interface{}) error {
	reqURL := c.base + path
	if params != nil {
		reqURL += "?" + params.Encode()
	}
	httpReq, err := http.NewRequestWithContext(ctx, http.MethodGet, reqURL, nil)
	if err != nil {
		return err
	}
	return c.do(httpReq, out)
}

func (c *Client) post(ctx context.Context, path string, body interface{}, out interface{}) error {
	reqURL := c.base + path
	var bodyReader io.Reader
	if body != nil {
		data, err := json.Marshal(body)
		if err != nil {
			return err
		}
		bodyReader = bytes.NewReader(data)
	}
	httpReq, err := http.NewRequestWithContext(ctx, http.MethodPost, reqURL, bodyReader)
	if err != nil {
		return err
	}
	if body != nil {
		httpReq.Header.Set("Content-Type", "application/json")
	}
	return c.do(httpReq, out)
}

func (c *Client) postNoBody(ctx context.Context, path string) error {
	reqURL := c.base + path
	httpReq, err := http.NewRequestWithContext(ctx, http.MethodPost, reqURL, nil)
	if err != nil {
		return err
	}
	resp, err := c.http.Do(httpReq)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode >= 400 {
		body, _ := io.ReadAll(resp.Body)
		return &SubstrateError{StatusCode: resp.StatusCode, Message: string(body)}
	}
	return nil
}

func (c *Client) do(req *http.Request, out interface{}) error {
	resp, err := c.http.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()

	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return err
	}

	if resp.StatusCode >= 400 {
		return &SubstrateError{StatusCode: resp.StatusCode, Message: string(body)}
	}

	if out != nil && len(body) > 0 {
		return json.Unmarshal(body, out)
	}
	return nil
}
