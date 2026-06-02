// Package substrate is a typed Go client for the Rust substrate_server
// HTTP loopback (default http://127.0.0.1:9090). It wraps every
// endpoint the Go server tier needs: evidence, query, memories,
// synthesis, connectors, permissions, crypto and export.
package substrate

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"net/http"
	"time"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
)

// Client talks to the substrate_server loopback.
type Client struct {
	baseURL string
	http    *http.Client
}

// New constructs a Client. If hc is nil a hardened default client is
// used. baseURL should not carry a trailing slash.
func New(baseURL string, hc *http.Client) *Client {
	if hc == nil {
		hc = httpx.NewClient(30 * time.Second)
	}
	return &Client{baseURL: baseURL, http: hc}
}

// substrateError mirrors the `{ "kind", "detail" }` body produced by
// substrate_server's `ApiError` (serde tag="kind", content="detail").
type substrateError struct {
	Kind   string          `json:"kind"`
	Detail json.RawMessage `json:"detail"`
}

// do issues an HTTP request with an optional JSON body and decodes a
// 2xx JSON response into out (which may be nil to discard the body).
// Non-2xx responses are converted into an [*httpx.Error] preserving
// the upstream status code and error kind.
func (c *Client) do(ctx context.Context, method, path string, body, out any) error {
	var reader io.Reader
	if body != nil {
		buf, err := json.Marshal(body)
		if err != nil {
			return httpx.Internal("substrate: marshal request: " + err.Error())
		}
		reader = bytes.NewReader(buf)
	}
	req, err := http.NewRequestWithContext(ctx, method, c.baseURL+path, reader)
	if err != nil {
		return httpx.Internal("substrate: build request: " + err.Error())
	}
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	if rid, ok := ctx.Value(requestIDKey{}).(string); ok && rid != "" {
		req.Header.Set("X-Request-Id", rid)
	}

	resp, err := c.http.Do(req)
	if err != nil {
		return httpx.NewError(http.StatusBadGateway, "SubstrateUnavailable",
			"substrate loopback unreachable: "+err.Error())
	}
	defer func() { _ = resp.Body.Close() }()

	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 16<<20))
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return decodeSubstrateError(resp.StatusCode, raw)
	}
	if out != nil && len(raw) > 0 {
		if err := json.Unmarshal(raw, out); err != nil {
			return httpx.Internal("substrate: decode response: " + err.Error())
		}
	}
	return nil
}

// decodeSubstrateError converts an upstream error body into an
// [*httpx.Error] that preserves the status code and kind.
func decodeSubstrateError(status int, raw []byte) error {
	var se substrateError
	if err := json.Unmarshal(raw, &se); err != nil || se.Kind == "" {
		return httpx.NewError(status, "Substrate", string(raw))
	}
	msg := se.Kind
	if len(se.Detail) > 0 {
		msg = se.Kind + ": " + string(se.Detail)
	}
	return httpx.NewError(status, se.Kind, msg)
}

// requestIDKey is the context key under which the gateway stores the
// inbound X-Request-Id so it can be propagated to the loopback.
type requestIDKey struct{}

// WithRequestID returns a context carrying the request id, which the
// client forwards as an X-Request-Id header on every call.
func WithRequestID(ctx context.Context, id string) context.Context {
	return context.WithValue(ctx, requestIDKey{}, id)
}

// ── Evidence ────────────────────────────────────────────────────────

// Ingest persists a message and returns its new evidence id.
func (c *Client) Ingest(ctx context.Context, req IngestRequest) (IDResponse, error) {
	var out IDResponse
	err := c.do(ctx, http.MethodPost, "/ingest", req, &out)
	return out, err
}

// Query runs a hybrid FTS query and returns the raw result array.
func (c *Client) Query(ctx context.Context, req QueryRequest) (json.RawMessage, error) {
	return c.raw(ctx, http.MethodPost, "/query", req)
}

// GetEvidence fetches a single decrypted evidence row by id.
func (c *Client) GetEvidence(ctx context.Context, id string) (json.RawMessage, error) {
	return c.raw(ctx, http.MethodGet, "/evidence/"+id, nil)
}

// ListMemories returns per-user memories for a scope.
func (c *Client) ListMemories(ctx context.Context, req ListMemoriesRequest) (json.RawMessage, error) {
	return c.raw(ctx, http.MethodPost, "/memories", req)
}

// Pin marks a memory decay-immune.
func (c *Client) Pin(ctx context.Context, id string) error {
	return c.do(ctx, http.MethodPost, "/pin", map[string]string{"id": id}, nil)
}

// Unpin releases a pin.
func (c *Client) Unpin(ctx context.Context, id string) error {
	return c.do(ctx, http.MethodPost, "/unpin", map[string]string{"id": id}, nil)
}

// Forget cryptographically forgets a single evidence row.
func (c *Client) Forget(ctx context.Context, id string) error {
	return c.do(ctx, http.MethodPost, "/forget", map[string]string{"id": id}, nil)
}

// ForgetScope cryptographically forgets an entire scope.
func (c *Client) ForgetScope(ctx context.Context, scopeID string) error {
	return c.do(ctx, http.MethodPost, "/forget_scope", map[string]string{"scope_id": scopeID}, nil)
}

// ── Synthesis ───────────────────────────────────────────────────────

// TriggerSynthesis kicks off a synthesis cycle.
func (c *Client) TriggerSynthesis(ctx context.Context, req SynthesisTriggerRequest) (json.RawMessage, error) {
	return c.raw(ctx, http.MethodPost, "/synthesis/trigger", req)
}

// SynthesisStatus fetches the status of a synthesis run by id.
func (c *Client) SynthesisStatus(ctx context.Context, id string) (json.RawMessage, error) {
	return c.raw(ctx, http.MethodGet, "/synthesis/"+id+"/status", nil)
}

// RecentSyntheses lists recent synthesis runs for a scope.
func (c *Client) RecentSyntheses(ctx context.Context, req RecentSynthesisRequest) (json.RawMessage, error) {
	return c.raw(ctx, http.MethodPost, "/synthesis/recent", req)
}

// ── Connectors ──────────────────────────────────────────────────────

// CreateConnector provisions a connector instance and returns its id.
func (c *Client) CreateConnector(ctx context.Context, req CreateConnectorRequest) (IDResponse, error) {
	var out IDResponse
	err := c.do(ctx, http.MethodPost, "/connectors", req, &out)
	return out, err
}

// ListConnectors returns all connector instances.
func (c *Client) ListConnectors(ctx context.Context) (json.RawMessage, error) {
	return c.raw(ctx, http.MethodGet, "/connectors", nil)
}

// AuthenticateConnector completes an OAuth2 code exchange.
func (c *Client) AuthenticateConnector(ctx context.Context, id string, req AuthenticateRequest) (json.RawMessage, error) {
	return c.raw(ctx, http.MethodPost, "/connectors/"+id+"/authenticate", req)
}

// SyncConnector runs an incremental sync and returns its report.
func (c *Client) SyncConnector(ctx context.Context, id string) (json.RawMessage, error) {
	return c.raw(ctx, http.MethodPost, "/connectors/"+id+"/sync", nil)
}

// RemoveConnector deletes a connector instance.
func (c *Client) RemoveConnector(ctx context.Context, id string) error {
	return c.do(ctx, http.MethodDelete, "/connectors/"+id, nil, nil)
}

// ConnectorStatus returns a connector instance's health record.
func (c *Client) ConnectorStatus(ctx context.Context, id string) (json.RawMessage, error) {
	return c.raw(ctx, http.MethodGet, "/connectors/"+id+"/status", nil)
}

// FetchContent calls the (Session-B-owned) content-fetch endpoint. A
// 501 is surfaced as an [*httpx.Error] with status 501 so callers can
// treat the feature as "not yet available".
func (c *Client) FetchContent(ctx context.Context, req FetchContentRequest) (json.RawMessage, error) {
	return c.raw(ctx, http.MethodPost, "/connector/fetch_content", req)
}

// ── Permissions ─────────────────────────────────────────────────────

// PermissionGrant idempotently inserts a relation tuple.
func (c *Client) PermissionGrant(ctx context.Context, t RelationTuple) error {
	return c.do(ctx, http.MethodPost, "/permission/grant", t, nil)
}

// PermissionRevoke removes a relation tuple (404 if absent).
func (c *Client) PermissionRevoke(ctx context.Context, t RelationTuple) error {
	return c.do(ctx, http.MethodPost, "/permission/revoke", t, nil)
}

// PermissionCheck evaluates a (subject, relation, object) query.
func (c *Client) PermissionCheck(ctx context.Context, t RelationTuple) (bool, error) {
	var out PermissionCheckResponse
	err := c.do(ctx, http.MethodPost, "/permission/check", t, &out)
	return out.Allowed, err
}

// ── Crypto ──────────────────────────────────────────────────────────

// HybridKeypair generates a fresh X25519+ML-KEM-768 hybrid keypair.
func (c *Client) HybridKeypair(ctx context.Context) (HybridKeypair, error) {
	var out HybridKeypair
	err := c.do(ctx, http.MethodPost, "/crypto/hybrid_keypair", nil, &out)
	return out, err
}

// SigningKeypair generates a fresh ML-DSA-65 signing keypair.
func (c *Client) SigningKeypair(ctx context.Context) (SigningKeypair, error) {
	var out SigningKeypair
	err := c.do(ctx, http.MethodPost, "/crypto/signing_keypair", nil, &out)
	return out, err
}

// ── Export ──────────────────────────────────────────────────────────

// ExportEvaluate runs a portable concept profile through the export
// policy engine and returns the approve/reject decision.
func (c *Client) ExportEvaluate(ctx context.Context, req ExportEvaluateRequest) (ExportDecision, error) {
	var out ExportDecision
	err := c.do(ctx, http.MethodPost, "/export/evaluate", req, &out)
	return out, err
}

// ── Health ──────────────────────────────────────────────────────────

// Health probes every subsystem reachable through the loopback.
func (c *Client) Health(ctx context.Context) (json.RawMessage, error) {
	return c.raw(ctx, http.MethodGet, "/health", nil)
}

// Metrics fetches the Prometheus text exposition from the loopback.
func (c *Client) Metrics(ctx context.Context) (string, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, c.baseURL+"/internal/metrics", nil)
	if err != nil {
		return "", httpx.Internal("substrate: build metrics request: " + err.Error())
	}
	resp, err := c.http.Do(req)
	if err != nil {
		return "", httpx.NewError(http.StatusBadGateway, "SubstrateUnavailable", err.Error())
	}
	defer func() { _ = resp.Body.Close() }()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 16<<20))
	if resp.StatusCode != http.StatusOK {
		return "", httpx.NewError(resp.StatusCode, "Substrate", string(raw))
	}
	return string(raw), nil
}

// raw issues a request and returns the response body verbatim.
func (c *Client) raw(ctx context.Context, method, path string, body any) (json.RawMessage, error) {
	var out json.RawMessage
	err := c.do(ctx, method, path, body, &out)
	return out, err
}
