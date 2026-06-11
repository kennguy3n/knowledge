package substrate

import (
	"context"
	"net/http"
	"net/http/httptest"
	"testing"
)

// muxClient returns a client backed by a mux that echoes a small JSON
// body for every endpoint the client touches, so each wrapper method is
// exercised end-to-end.
func muxClient(t *testing.T) *Client {
	t.Helper()
	mux := http.NewServeMux()
	json := func(body string) http.HandlerFunc {
		return func(w http.ResponseWriter, _ *http.Request) { _, _ = w.Write([]byte(body)) }
	}
	mux.HandleFunc("/ingest", json(`{"id":"ev"}`))
	mux.HandleFunc("/query", json(`[]`))
	mux.HandleFunc("/memories", json(`[]`))
	mux.HandleFunc("/channel_memory/", json(`{"summary":"recap"}`))
	mux.HandleFunc("/concept_graph/", json(`{"nodes":[],"edges":[]}`))
	mux.HandleFunc("/pin", json(`{}`))
	mux.HandleFunc("/unpin", json(`{}`))
	mux.HandleFunc("/forget", json(`{}`))
	mux.HandleFunc("/forget_scope", json(`{}`))
	mux.HandleFunc("/synthesis/trigger", json(`{}`))
	mux.HandleFunc("/synthesis/recent", json(`[]`))
	mux.HandleFunc("/connectors", json(`{"id":"c1"}`))
	mux.HandleFunc("/crypto/hybrid_keypair", json(`{"algorithm":"hybrid","public_key_hex":"ab"}`))
	mux.HandleFunc("/crypto/signing_keypair", json(`{"algorithm":"ml-dsa-65","public_key_hex":"cd"}`))
	mux.HandleFunc("/export/evaluate", json(`{"approved":[],"rejected":[],"warnings":[],"allow_raw_evidence":true}`))
	mux.HandleFunc("/health", json(`{"ok":true}`))
	mux.HandleFunc("/internal/metrics", json("gateway_up 1\n"))
	// Path-param endpoints.
	mux.HandleFunc("/evidence/", json(`{"id":"ev"}`))
	mux.HandleFunc("/synthesis/", json(`{"status":"complete"}`))
	mux.HandleFunc("/connectors/", json(`{"state":"idle"}`))
	mux.HandleFunc("/permission/grant", json(`{}`))
	mux.HandleFunc("/permission/revoke", json(`{}`))
	mux.HandleFunc("/permission/check", json(`{"allowed":true}`))

	srv := httptest.NewServer(mux)
	t.Cleanup(srv.Close)
	return New(srv.URL, srv.Client())
}

func TestAllEndpoints(t *testing.T) {
	t.Parallel()
	c := muxClient(t)
	ctx := context.Background()

	if _, err := c.Ingest(ctx, IngestRequest{ScopeID: "s", Body: "b"}); err != nil {
		t.Errorf("Ingest: %v", err)
	}
	if _, err := c.Query(ctx, QueryRequest{ScopeID: "s", QueryText: "q"}); err != nil {
		t.Errorf("Query: %v", err)
	}
	if _, err := c.GetEvidence(ctx, "id"); err != nil {
		t.Errorf("GetEvidence: %v", err)
	}
	if _, err := c.ListMemories(ctx, ListMemoriesRequest{ScopeID: "s"}); err != nil {
		t.Errorf("ListMemories: %v", err)
	}
	if _, err := c.ChannelMemory(ctx, "s"); err != nil {
		t.Errorf("ChannelMemory: %v", err)
	}
	if _, err := c.ConceptGraph(ctx, "s"); err != nil {
		t.Errorf("ConceptGraph: %v", err)
	}
	if err := c.Pin(ctx, "id"); err != nil {
		t.Errorf("Pin: %v", err)
	}
	if err := c.Unpin(ctx, "id"); err != nil {
		t.Errorf("Unpin: %v", err)
	}
	if err := c.Forget(ctx, "id"); err != nil {
		t.Errorf("Forget: %v", err)
	}
	if err := c.ForgetScope(ctx, "s"); err != nil {
		t.Errorf("ForgetScope: %v", err)
	}
	if _, err := c.TriggerSynthesis(ctx, SynthesisTriggerRequest{ScopeID: "s"}); err != nil {
		t.Errorf("TriggerSynthesis: %v", err)
	}
	if _, err := c.SynthesisStatus(ctx, "id"); err != nil {
		t.Errorf("SynthesisStatus: %v", err)
	}
	if _, err := c.RecentSyntheses(ctx, RecentSynthesisRequest{ScopeID: "s"}); err != nil {
		t.Errorf("RecentSyntheses: %v", err)
	}
	if _, err := c.CreateConnector(ctx, CreateConnectorRequest{Kind: "k", ScopeID: "s"}); err != nil {
		t.Errorf("CreateConnector: %v", err)
	}
	if _, err := c.ListConnectors(ctx); err != nil {
		t.Errorf("ListConnectors: %v", err)
	}
	if _, err := c.AuthenticateConnector(ctx, "id", AuthenticateRequest{AuthCode: "x"}); err != nil {
		t.Errorf("AuthenticateConnector: %v", err)
	}
	if _, err := c.SyncConnector(ctx, "id"); err != nil {
		t.Errorf("SyncConnector: %v", err)
	}
	if err := c.RemoveConnector(ctx, "id"); err != nil {
		t.Errorf("RemoveConnector: %v", err)
	}
	if _, err := c.ConnectorStatus(ctx, "id"); err != nil {
		t.Errorf("ConnectorStatus: %v", err)
	}
	if err := c.PermissionGrant(ctx, RelationTuple{Relation: "r"}); err != nil {
		t.Errorf("PermissionGrant: %v", err)
	}
	if err := c.PermissionRevoke(ctx, RelationTuple{Relation: "r"}); err != nil {
		t.Errorf("PermissionRevoke: %v", err)
	}
	if _, err := c.HybridKeypair(ctx); err != nil {
		t.Errorf("HybridKeypair: %v", err)
	}
	if _, err := c.SigningKeypair(ctx); err != nil {
		t.Errorf("SigningKeypair: %v", err)
	}
	if _, err := c.ExportEvaluate(ctx, ExportEvaluateRequest{Profile: []byte(`{}`)}); err != nil {
		t.Errorf("ExportEvaluate: %v", err)
	}
	if _, err := c.Health(ctx); err != nil {
		t.Errorf("Health: %v", err)
	}
	m, err := c.Metrics(ctx)
	if err != nil || m == "" {
		t.Errorf("Metrics: %q err=%v", m, err)
	}
}

func TestMetricsErrorStatus(t *testing.T) {
	t.Parallel()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusInternalServerError)
		_, _ = w.Write([]byte("boom"))
	}))
	t.Cleanup(srv.Close)
	c := New(srv.URL, srv.Client())
	if _, err := c.Metrics(context.Background()); err == nil {
		t.Fatal("expected error on non-200 metrics")
	}
}
