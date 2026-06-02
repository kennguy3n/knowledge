package substrate

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"
)

func TestNewClient(t *testing.T) {
	c := NewClient("http://localhost:9090", 5*time.Second)
	if c.base != "http://localhost:9090" {
		t.Errorf("base = %q, want %q", c.base, "http://localhost:9090")
	}
}

func TestIngest(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/substrate/ingest" {
			t.Errorf("path = %q, want /substrate/ingest", r.URL.Path)
		}
		if r.Method != http.MethodPost {
			t.Errorf("method = %q, want POST", r.Method)
		}
		var req IngestRequest
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			t.Fatalf("decode error: %v", err)
		}
		if req.ScopeID != "scope-1" {
			t.Errorf("scope_id = %q, want scope-1", req.ScopeID)
		}
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(IngestResponse{EvidenceID: "ev-123"})
	}))
	defer srv.Close()

	c := NewClient(srv.URL, 5*time.Second)
	resp, err := c.Ingest(context.Background(), &IngestRequest{
		ScopeID:    "scope-1",
		Body:       "test body",
		Source:     "Manual",
		Importance: "Important",
	})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if resp.EvidenceID != "ev-123" {
		t.Errorf("evidence_id = %q, want ev-123", resp.EvidenceID)
	}
}

func TestQuery(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(QueryResponse{
			Results: []QueryResult{{EvidenceID: "ev-1", Score: 0.95, Snippet: "test"}},
		})
	}))
	defer srv.Close()

	c := NewClient(srv.URL, 5*time.Second)
	resp, err := c.Query(context.Background(), &QueryRequest{
		ScopeID:   "scope-1",
		QueryText: "test",
		Limit:     10,
	})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if len(resp.Results) != 1 {
		t.Fatalf("results = %d, want 1", len(resp.Results))
	}
	if resp.Results[0].EvidenceID != "ev-1" {
		t.Errorf("evidence_id = %q, want ev-1", resp.Results[0].EvidenceID)
	}
}

func TestGetEvidence(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/substrate/evidence/ev-123" {
			t.Errorf("path = %q", r.URL.Path)
		}
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(EvidenceRecord{ID: "ev-123", ScopeID: "s1", Body: "hello"})
	}))
	defer srv.Close()

	c := NewClient(srv.URL, 5*time.Second)
	resp, err := c.GetEvidence(context.Background(), "ev-123")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if resp.ID != "ev-123" {
		t.Errorf("id = %q, want ev-123", resp.ID)
	}
}

func TestForget(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusNoContent)
	}))
	defer srv.Close()

	c := NewClient(srv.URL, 5*time.Second)
	err := c.Forget(context.Background(), "ev-123")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestForgetScope(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusNoContent)
	}))
	defer srv.Close()

	c := NewClient(srv.URL, 5*time.Second)
	err := c.ForgetScope(context.Background(), "scope-1")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestHealth(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(HealthStatus{
			CoreVersion: "0.1.0",
			UptimeSecs:  42,
		})
	}))
	defer srv.Close()

	c := NewClient(srv.URL, 5*time.Second)
	resp, err := c.Health(context.Background())
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if resp.CoreVersion != "0.1.0" {
		t.Errorf("version = %q, want 0.1.0", resp.CoreVersion)
	}
}

func TestMetrics(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/plain")
		w.Write([]byte("# TYPE knowledge_ingest_total counter\nknowledge_ingest_total 42"))
	}))
	defer srv.Close()

	c := NewClient(srv.URL, 5*time.Second)
	resp, err := c.Metrics(context.Background())
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if resp == "" {
		t.Error("expected non-empty metrics")
	}
}

func TestSubstrateError(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusNotFound)
		w.Write([]byte(`{"error":"not found"}`))
	}))
	defer srv.Close()

	c := NewClient(srv.URL, 5*time.Second)
	_, err := c.GetEvidence(context.Background(), "nonexistent")
	if err == nil {
		t.Fatal("expected error for 404")
	}
	se, ok := err.(*SubstrateError)
	if !ok {
		t.Fatalf("expected *SubstrateError, got %T", err)
	}
	if se.StatusCode != http.StatusNotFound {
		t.Errorf("status = %d, want 404", se.StatusCode)
	}
}

func TestTriggerSynthesis(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(SynthesisTriggerResponse{WindowID: "win-1"})
	}))
	defer srv.Close()

	c := NewClient(srv.URL, 5*time.Second)
	resp, err := c.TriggerSynthesis(context.Background(), &SynthesisTriggerRequest{
		ScopeID: "s1",
		Trigger: "ManualUserAction",
	})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if resp.WindowID != "win-1" {
		t.Errorf("window_id = %q, want win-1", resp.WindowID)
	}
}

func TestGenerateKeypair(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(Keypair{Algorithm: "ml-dsa-65"})
	}))
	defer srv.Close()

	c := NewClient(srv.URL, 5*time.Second)
	resp, err := c.GenerateKeypair(context.Background())
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if resp.Algorithm != "ml-dsa-65" {
		t.Errorf("algorithm = %q, want ml-dsa-65", resp.Algorithm)
	}
}

func TestListMemories(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Query().Get("scope_id") != "s1" {
			t.Errorf("scope_id = %q", r.URL.Query().Get("scope_id"))
		}
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode([]MemoryRecord{{ID: "m1", ScopeID: "s1"}})
	}))
	defer srv.Close()

	c := NewClient(srv.URL, 5*time.Second)
	memories, err := c.ListMemories(context.Background(), "s1", nil, false)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if len(memories) != 1 {
		t.Errorf("memories = %d, want 1", len(memories))
	}
}

func TestDecaySweep(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(map[string]int{"archived_count": 5})
	}))
	defer srv.Close()

	c := NewClient(srv.URL, 5*time.Second)
	count, err := c.DecaySweep(context.Background(), "s1")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if count != 5 {
		t.Errorf("count = %d, want 5", count)
	}
}

func TestEncrypt(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(EncryptResponse{CiphertextB64: "Y2lwaGVy"})
	}))
	defer srv.Close()

	c := NewClient(srv.URL, 5*time.Second)
	resp, err := c.Encrypt(context.Background(), &EncryptRequest{
		ScopeID:      "s1",
		PlaintextB64: "cGxhaW4=",
	})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if resp.CiphertextB64 != "Y2lwaGVy" {
		t.Errorf("ciphertext = %q", resp.CiphertextB64)
	}
}

func TestDecrypt(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(DecryptResponse{PlaintextB64: "cGxhaW4="})
	}))
	defer srv.Close()

	c := NewClient(srv.URL, 5*time.Second)
	resp, err := c.Decrypt(context.Background(), &DecryptRequest{
		ScopeID:       "s1",
		CiphertextB64: "Y2lwaGVy",
	})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if resp.PlaintextB64 != "cGxhaW4=" {
		t.Errorf("plaintext = %q", resp.PlaintextB64)
	}
}
