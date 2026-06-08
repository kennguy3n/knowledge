// Package substrate is a typed Go client for the Rust substrate_server
// HTTP loopback (default http://127.0.0.1:9090). It wraps every
// endpoint the Go server tier needs: evidence, query, memories,
// synthesis, connectors, permissions, crypto and export.
package substrate

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
)

// Client talks to the substrate_server loopback.
//
// In the default single-node deployment it wraps one base URL. Under
// active-passive HA (WS2) it holds the primary plus one or more standby
// URLs and routes per request: writes go to the node currently believed
// to be primary (failing over to another node when it is unreachable or
// reports itself a read-only standby), while reads prefer a standby to
// offload the primary and fall back to it on error. The believed
// primary is learned reactively from successful writes, so it tracks
// leadership changes (promotion after a primary failure) without a
// separate health-polling loop.
type Client struct {
	// nodes are the substrate base URLs (no trailing slash). Index 0 is
	// the initial primary guess; len == 1 for a non-HA deployment.
	nodes []string
	http  *http.Client

	mu sync.RWMutex
	// primary indexes nodes for the node believed to currently accept
	// writes. Guarded by mu.
	primary int
}

// New constructs a single-node Client. If hc is nil a hardened default
// client is used. baseURL should not carry a trailing slash.
func New(baseURL string, hc *http.Client) *Client {
	if hc == nil {
		hc = httpx.NewClient(30 * time.Second)
	}
	return &Client{nodes: []string{baseURL}, http: hc}
}

// NewHA constructs a Client for an active-passive HA deployment. The
// primary URL is the initial write target; standby URLs are additional
// nodes used for read offload and write failover. Empty standby URLs
// are ignored, so passing none is equivalent to [New]. URLs should not
// carry a trailing slash.
func NewHA(primaryURL string, standbyURLs []string, hc *http.Client) *Client {
	if hc == nil {
		hc = httpx.NewClient(30 * time.Second)
	}
	nodes := make([]string, 0, 1+len(standbyURLs))
	nodes = append(nodes, primaryURL)
	for _, u := range standbyURLs {
		if s := strings.TrimRight(strings.TrimSpace(u), "/"); s != "" {
			nodes = append(nodes, s)
		}
	}
	return &Client{nodes: nodes, http: hc}
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
	// Marshal once; the body may be replayed against several nodes on
	// failover so we cannot reuse a one-shot io.Reader.
	var payload []byte
	if body != nil {
		buf, err := json.Marshal(body)
		if err != nil {
			return httpx.Internal("substrate: marshal request: " + err.Error())
		}
		payload = buf
	}
	return c.route(method, path, func(base string) error {
		return c.doOne(ctx, base, method, path, payload, out)
	})
}

// doOne issues a single request against one substrate base URL.
func (c *Client) doOne(ctx context.Context, base, method, path string, payload []byte, out any) error {
	var reader io.Reader
	if payload != nil {
		reader = bytes.NewReader(payload)
	}
	req, err := http.NewRequestWithContext(ctx, method, base+path, reader)
	if err != nil {
		return httpx.Internal("substrate: build request: " + err.Error())
	}
	if payload != nil {
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

// route runs attempt against the substrate nodes in priority order
// (write vs read), retrying on the next node when the current one is
// unreachable or reports itself a read-only standby. A successful write
// updates the believed-primary so subsequent writes target it directly.
//
// Reads are ordered standby-first with the primary last (see
// [Client.nodeOrder]). Because a standby applies the primary's WAL
// asynchronously it can briefly miss a freshly written row, so a 404
// from a non-primary read is treated as a miss and falls through to the
// next node — ultimately the authoritative primary — preserving
// read-after-write consistency (e.g. GET /evidence/{id} right after
// POST /ingest). A 404 from the primary itself is genuine and returned.
func (c *Client) route(method, path string, attempt func(base string) error) error {
	write := !isReadRoute(method, path)
	order := c.nodeOrder(write)
	var lastErr error
	for i, idx := range order {
		err := attempt(c.nodes[idx])
		if err == nil {
			if write {
				c.setPrimary(idx)
			}
			return nil
		}
		last := i == len(order)-1
		if !last && (isFailoverErr(err) || (!write && isNotFound(err))) {
			lastErr = err
			continue
		}
		return err
	}
	return lastErr
}

// nodeOrder returns the indices of nodes to try, in priority order.
// Writes start at the believed primary; reads prefer standbys (to
// offload the primary) and fall back to the primary last.
func (c *Client) nodeOrder(write bool) []int {
	c.mu.RLock()
	primary := c.primary
	c.mu.RUnlock()
	order := make([]int, 0, len(c.nodes))
	if write {
		order = append(order, primary)
		for i := range c.nodes {
			if i != primary {
				order = append(order, i)
			}
		}
		return order
	}
	for i := range c.nodes {
		if i != primary {
			order = append(order, i)
		}
	}
	order = append(order, primary)
	return order
}

// setPrimary records the node that last accepted a write.
func (c *Client) setPrimary(idx int) {
	c.mu.Lock()
	c.primary = idx
	c.mu.Unlock()
}

// isReadRoute reports whether (method, path) is a read-only endpoint
// that may be served by a standby. Every GET is a read; the handful of
// read-only POSTs are listed explicitly. Anything else (mutating POSTs,
// DELETEs, unknown routes) is treated as a write and pinned to the
// primary — misrouting a read to the primary is harmless, whereas
// misrouting a write to a standby would be rejected.
func isReadRoute(method, path string) bool {
	switch method {
	case http.MethodGet:
		return true
	case http.MethodPost:
		switch path {
		case "/query", "/memories", "/synthesis/recent", "/permission/check":
			return true
		}
	}
	return false
}

// isFailoverErr reports whether an error from one node should trigger a
// retry against another. Only a transport-level unreachable error
// (502 SubstrateUnavailable, raised by this client when the connection
// fails) or a 503 (a standby rejecting a write, or a transiently
// unavailable subsystem) are retriable. Application errors — including
// a 502 surfaced from the substrate's own upstream connector/inference
// failures — are returned to the caller unchanged.
func isFailoverErr(err error) bool {
	var he *httpx.Error
	if !errors.As(err, &he) {
		return false
	}
	switch {
	case he.Status == http.StatusBadGateway && he.Kind == "SubstrateUnavailable":
		return true
	case he.Status == http.StatusServiceUnavailable:
		return true
	default:
		return false
	}
}

// isNotFound reports whether err is a 404 from a substrate node. Used
// only for reads, where a miss on a not-yet-caught-up standby should
// fall through to the authoritative primary rather than surface to the
// caller (see [Client.route]).
func isNotFound(err error) bool {
	var he *httpx.Error
	return errors.As(err, &he) && he.Status == http.StatusNotFound
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

// ChannelMemory returns the latest synthesised channel recap for a
// scope. The substrate replies 404 when synthesis has never produced a
// recap for the scope, which the gateway surfaces verbatim.
func (c *Client) ChannelMemory(ctx context.Context, scopeID string) (json.RawMessage, error) {
	return c.raw(ctx, http.MethodGet, "/channel_memory/"+scopeID, nil)
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
	const path = "/internal/metrics"
	var body string
	err := c.route(http.MethodGet, path, func(base string) error {
		req, err := http.NewRequestWithContext(ctx, http.MethodGet, base+path, nil)
		if err != nil {
			return httpx.Internal("substrate: build metrics request: " + err.Error())
		}
		resp, err := c.http.Do(req)
		if err != nil {
			return httpx.NewError(http.StatusBadGateway, "SubstrateUnavailable", err.Error())
		}
		defer func() { _ = resp.Body.Close() }()
		raw, _ := io.ReadAll(io.LimitReader(resp.Body, 16<<20))
		if resp.StatusCode != http.StatusOK {
			return httpx.NewError(resp.StatusCode, "Substrate", string(raw))
		}
		body = string(raw)
		return nil
	})
	if err != nil {
		return "", err
	}
	return body, nil
}

// raw issues a request and returns the response body verbatim.
func (c *Client) raw(ctx context.Context, method, path string, body any) (json.RawMessage, error) {
	var out json.RawMessage
	err := c.do(ctx, method, path, body, &out)
	return out, err
}
