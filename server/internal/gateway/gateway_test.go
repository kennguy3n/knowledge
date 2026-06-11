package gateway

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/kennguy3n/knowledge/server/internal/middleware"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
)

const scopeUUID = "66666666-6666-6666-6666-666666666666"

// fakeSub implements substrateAPI for gateway handler tests.
type fakeSub struct {
	healthErr  error
	healthRaw  json.RawMessage
	statusRaws []json.RawMessage
	statusIdx  int
	memories   json.RawMessage
	channelMem json.RawMessage
	// createdMemory captures the last CreateMemory request so tests
	// can assert the gateway forwarded the validated body.
	createdMemory *substrate.CreateMemoryRequest
	// pinnedID / unpinnedID capture the last id forwarded to Pin / Unpin.
	pinnedID   string
	unpinnedID string
}

func (f *fakeSub) Ingest(context.Context, substrate.IngestRequest) (substrate.IDResponse, error) {
	return substrate.IDResponse{ID: "ev-1"}, nil
}
func (f *fakeSub) Query(context.Context, substrate.QueryRequest) (json.RawMessage, error) {
	return json.RawMessage(`{"results":[]}`), nil
}
func (f *fakeSub) GetEvidence(context.Context, string) (json.RawMessage, error) {
	return json.RawMessage(`{"id":"ev-1"}`), nil
}
func (f *fakeSub) ListMemories(context.Context, substrate.ListMemoriesRequest) (json.RawMessage, error) {
	if f.memories == nil {
		return json.RawMessage(`[]`), nil
	}
	return f.memories, nil
}
func (f *fakeSub) CreateMemory(_ context.Context, req substrate.CreateMemoryRequest) (json.RawMessage, error) {
	f.createdMemory = &req
	return json.RawMessage(`{"id":"mem-1","scope_id":"` + req.ScopeID + `","summary":"` + req.Content + `","state":"Candidate"}`), nil
}
func (f *fakeSub) Pin(_ context.Context, id string) error {
	f.pinnedID = id
	return nil
}
func (f *fakeSub) Unpin(_ context.Context, id string) error {
	f.unpinnedID = id
	return nil
}
func (f *fakeSub) ChannelMemory(context.Context, string) (json.RawMessage, error) {
	if f.channelMem == nil {
		return json.RawMessage(`{"summary":"recap"}`), nil
	}
	return f.channelMem, nil
}
func (f *fakeSub) ForgetScope(context.Context, string) error { return nil }
func (f *fakeSub) TriggerSynthesis(context.Context, substrate.SynthesisTriggerRequest) (json.RawMessage, error) {
	return json.RawMessage(`{"id":"syn-1"}`), nil
}
func (f *fakeSub) SynthesisStatus(context.Context, string) (json.RawMessage, error) {
	if f.statusIdx < len(f.statusRaws) {
		raw := f.statusRaws[f.statusIdx]
		f.statusIdx++
		return raw, nil
	}
	return json.RawMessage(`{"status":"complete"}`), nil
}
func (f *fakeSub) RecentSyntheses(context.Context, substrate.RecentSynthesisRequest) (json.RawMessage, error) {
	return json.RawMessage(`[]`), nil
}
func (f *fakeSub) Health(context.Context) (json.RawMessage, error) {
	if f.healthRaw != nil {
		return f.healthRaw, f.healthErr
	}
	return json.RawMessage(`{"store":"ok"}`), f.healthErr
}

func do(h http.Handler, method, path, body string) *httptest.ResponseRecorder {
	var r *http.Request
	if body != "" {
		r = httptest.NewRequest(method, path, strings.NewReader(body))
	} else {
		r = httptest.NewRequest(method, path, nil)
	}
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, r)
	return rec
}

func TestHealthOKAndDegraded(t *testing.T) {
	t.Parallel()
	h := NewRouter(Deps{Substrate: &fakeSub{}, Ready: map[string]bool{"postgres": true, "nats": false}})
	rec := do(h, http.MethodGet, "/health", "")
	if rec.Code != http.StatusOK {
		t.Fatalf("health code = %d body=%s", rec.Code, rec.Body.String())
	}
	if !strings.Contains(rec.Body.String(), `"postgres":"ok"`) || !strings.Contains(rec.Body.String(), `"nats":"disabled"`) {
		t.Fatalf("subsystems wrong: %s", rec.Body.String())
	}

	down := NewRouter(Deps{Substrate: &fakeSub{healthErr: context.DeadlineExceeded}})
	rec = do(down, http.MethodGet, "/health", "")
	if rec.Code != http.StatusServiceUnavailable {
		t.Fatalf("degraded code = %d", rec.Code)
	}
}

func TestHealthSurfacesReplication(t *testing.T) {
	t.Parallel()
	// When the substrate embeds a replication object, the gateway lifts
	// it to a top-level `replication` field.
	repl := `{"store":"ok","replication":{"enabled":true,"role":"primary","lag_frames":0}}`
	h := NewRouter(Deps{Substrate: &fakeSub{healthRaw: json.RawMessage(repl)}})
	rec := do(h, http.MethodGet, "/health", "")
	if rec.Code != http.StatusOK {
		t.Fatalf("health code = %d body=%s", rec.Code, rec.Body.String())
	}
	var body struct {
		Replication struct {
			Enabled bool   `json:"enabled"`
			Role    string `json:"role"`
		} `json:"replication"`
	}
	if err := json.Unmarshal(rec.Body.Bytes(), &body); err != nil {
		t.Fatalf("decode health body: %v", err)
	}
	if !body.Replication.Enabled || body.Replication.Role != "primary" {
		t.Fatalf("replication not surfaced: %s", rec.Body.String())
	}

	// A standalone substrate (no replication key) omits the field.
	h2 := NewRouter(Deps{Substrate: &fakeSub{}})
	rec2 := do(h2, http.MethodGet, "/health", "")
	if strings.Contains(rec2.Body.String(), `"replication"`) {
		t.Fatalf("standalone health should omit replication: %s", rec2.Body.String())
	}
}

func TestIngestValidationAndSuccess(t *testing.T) {
	t.Parallel()
	h := NewRouter(Deps{Substrate: &fakeSub{}})

	rec := do(h, http.MethodPost, "/api/v1/ingest", `{"scope_id":"bad","body":"x"}`)
	if rec.Code != http.StatusBadRequest {
		t.Fatalf("bad scope code = %d", rec.Code)
	}
	rec = do(h, http.MethodPost, "/api/v1/ingest", `{"scope_id":"`+scopeUUID+`","body":""}`)
	if rec.Code != http.StatusBadRequest {
		t.Fatalf("empty body code = %d", rec.Code)
	}
	rec = do(h, http.MethodPost, "/api/v1/ingest", `{"scope_id":"`+scopeUUID+`","body":"hello"}`)
	if rec.Code != http.StatusCreated {
		t.Fatalf("ingest code = %d body=%s", rec.Code, rec.Body.String())
	}
}

func TestQueryAndForget(t *testing.T) {
	t.Parallel()
	h := NewRouter(Deps{Substrate: &fakeSub{}})
	rec := do(h, http.MethodPost, "/api/v1/query", `{"scope_id":"`+scopeUUID+`","query_text":"hi"}`)
	if rec.Code != http.StatusOK {
		t.Fatalf("query code = %d", rec.Code)
	}
	rec = do(h, http.MethodPost, "/api/v1/forget/"+scopeUUID, "")
	if rec.Code != http.StatusNoContent {
		t.Fatalf("forget code = %d", rec.Code)
	}
}

func TestListMemoriesLimit(t *testing.T) {
	t.Parallel()
	h := NewRouter(Deps{Substrate: &fakeSub{memories: json.RawMessage(`[{"a":1},{"b":2},{"c":3}]`)}})
	rec := do(h, http.MethodGet, "/api/v1/memories?scope_id="+scopeUUID+"&limit=2", "")
	if rec.Code != http.StatusOK {
		t.Fatalf("memories code = %d", rec.Code)
	}
	var items []json.RawMessage
	if err := json.Unmarshal(rec.Body.Bytes(), &items); err != nil {
		t.Fatal(err)
	}
	if len(items) != 2 {
		t.Fatalf("limit not applied: %d", len(items))
	}
}

func TestCreateMemoryValidationAndSuccess(t *testing.T) {
	t.Parallel()
	sub := &fakeSub{}
	h := NewRouter(Deps{Substrate: sub})

	// Bad scope id → 400.
	rec := do(h, http.MethodPost, "/api/v1/memories", `{"scope_id":"bad","observation_type":"note","content":"x"}`)
	if rec.Code != http.StatusBadRequest {
		t.Fatalf("bad scope code = %d", rec.Code)
	}
	// Empty content → 400.
	rec = do(h, http.MethodPost, "/api/v1/memories", `{"scope_id":"`+scopeUUID+`","observation_type":"note","content":""}`)
	if rec.Code != http.StatusBadRequest {
		t.Fatalf("empty content code = %d", rec.Code)
	}
	// Empty observation_type → 400.
	rec = do(h, http.MethodPost, "/api/v1/memories", `{"scope_id":"`+scopeUUID+`","observation_type":"","content":"x"}`)
	if rec.Code != http.StatusBadRequest {
		t.Fatalf("empty observation_type code = %d", rec.Code)
	}
	// Unknown sensitivity → 400 (fail-closed on bad input).
	rec = do(h, http.MethodPost, "/api/v1/memories", `{"scope_id":"`+scopeUUID+`","observation_type":"note","content":"x","sensitivity":"Bogus"}`)
	if rec.Code != http.StatusBadRequest {
		t.Fatalf("bad sensitivity code = %d", rec.Code)
	}

	// Happy path → 201 with the created record, and the gateway must
	// forward the canonicalised scope + body to the substrate.
	rec = do(h, http.MethodPost, "/api/v1/memories", `{"scope_id":"`+scopeUUID+`","observation_type":"preference","content":"prefers async standups","sensitivity":"Useful"}`)
	if rec.Code != http.StatusCreated {
		t.Fatalf("create code = %d body=%s", rec.Code, rec.Body.String())
	}
	if sub.createdMemory == nil {
		t.Fatal("CreateMemory was not called")
	}
	if sub.createdMemory.ScopeID != scopeUUID || sub.createdMemory.Content != "prefers async standups" {
		t.Fatalf("forwarded request wrong: %+v", *sub.createdMemory)
	}
	if sub.createdMemory.ObservationType != "preference" || sub.createdMemory.Sensitivity != "Useful" {
		t.Fatalf("forwarded request wrong: %+v", *sub.createdMemory)
	}
	var body map[string]any
	if err := json.Unmarshal(rec.Body.Bytes(), &body); err != nil {
		t.Fatal(err)
	}
	if body["state"] != "Candidate" {
		t.Fatalf("unexpected created record: %v", body)
	}
}

func TestPinUnpinMemory(t *testing.T) {
	t.Parallel()
	sub := &fakeSub{}
	h := NewRouter(Deps{Substrate: sub})

	// Happy path: pin forwards the canonicalised memory id and 204s.
	rec := do(h, http.MethodPost, "/api/v1/memories/"+scopeUUID+"/pin", "")
	if rec.Code != http.StatusNoContent {
		t.Fatalf("pin code = %d body=%s", rec.Code, rec.Body.String())
	}
	if sub.pinnedID != scopeUUID {
		t.Fatalf("pin forwarded id = %q, want %q", sub.pinnedID, scopeUUID)
	}

	rec = do(h, http.MethodPost, "/api/v1/memories/"+scopeUUID+"/unpin", "")
	if rec.Code != http.StatusNoContent {
		t.Fatalf("unpin code = %d", rec.Code)
	}
	if sub.unpinnedID != scopeUUID {
		t.Fatalf("unpin forwarded id = %q, want %q", sub.unpinnedID, scopeUUID)
	}

	// A malformed memory id is rejected before reaching the substrate.
	bad := do(h, http.MethodPost, "/api/v1/memories/not-a-uuid/pin", "")
	if bad.Code != http.StatusBadRequest {
		t.Fatalf("bad pin id code = %d, want 400", bad.Code)
	}
}

func TestChannelMemory(t *testing.T) {
	t.Parallel()
	recap := `{"id":"cm-1","scope_id":"` + scopeUUID + `","summary":"X200 gasket recall recap","state":"Reinforced"}`
	h := NewRouter(Deps{Substrate: &fakeSub{channelMem: json.RawMessage(recap)}})

	// Happy path: the synthesised recap is returned verbatim.
	rec := do(h, http.MethodGet, "/api/v1/memories/channel?scope_id="+scopeUUID, "")
	if rec.Code != http.StatusOK {
		t.Fatalf("channel memory code = %d", rec.Code)
	}
	var body map[string]any
	if err := json.Unmarshal(rec.Body.Bytes(), &body); err != nil {
		t.Fatal(err)
	}
	if body["summary"] != "X200 gasket recall recap" {
		t.Fatalf("unexpected recap: %v", body["summary"])
	}

	// A malformed scope id is rejected before hitting the substrate.
	bad := do(h, http.MethodGet, "/api/v1/memories/channel?scope_id=not-a-uuid", "")
	if bad.Code != http.StatusBadRequest {
		t.Fatalf("bad scope id code = %d, want 400", bad.Code)
	}
}

func TestSynthesisTriggerAndStatus(t *testing.T) {
	t.Parallel()
	h := NewRouter(Deps{Substrate: &fakeSub{}})
	rec := do(h, http.MethodPost, "/api/v1/synthesis/trigger", `{"scope_id":"`+scopeUUID+`"}`)
	if rec.Code != http.StatusAccepted {
		t.Fatalf("trigger code = %d", rec.Code)
	}
	rec = do(h, http.MethodGet, "/api/v1/synthesis/"+scopeUUID+"/status", "")
	if rec.Code != http.StatusOK {
		t.Fatalf("status code = %d", rec.Code)
	}
}

func TestSynthesisStatusSSE(t *testing.T) {
	t.Parallel()
	sub := &fakeSub{statusRaws: []json.RawMessage{
		json.RawMessage(`{"status":"running"}`),
		json.RawMessage(`{"status":"complete"}`),
	}}
	h := NewRouter(Deps{Substrate: sub})
	rec := do(h, http.MethodGet, "/api/v1/synthesis/"+scopeUUID+"/status?stream=true", "")
	if rec.Code != http.StatusOK {
		t.Fatalf("sse code = %d", rec.Code)
	}
	body := rec.Body.String()
	if !strings.Contains(body, "event: status") || !strings.Contains(body, "event: done") {
		t.Fatalf("sse frames missing: %s", body)
	}
}

func TestMetricsEndpoint(t *testing.T) {
	t.Parallel()
	h := NewRouter(Deps{Substrate: &fakeSub{}})
	// Generate some traffic so a counter appears.
	do(h, http.MethodGet, "/health", "")
	rec := do(h, http.MethodGet, "/metrics", "")
	if rec.Code != http.StatusOK {
		t.Fatalf("metrics code = %d", rec.Code)
	}
	if !strings.Contains(rec.Body.String(), "gateway_") {
		t.Fatalf("no gateway metrics in exposition: %s", rec.Body.String()[:minInt(200, len(rec.Body.String()))])
	}
}

func TestAuthEnforced(t *testing.T) {
	t.Parallel()
	auth := middleware.NewAuthenticator("secret-key", "")
	h := NewRouter(Deps{Substrate: &fakeSub{}, Auth: auth})

	// Missing token → 401.
	rec := do(h, http.MethodPost, "/api/v1/query", `{"scope_id":"`+scopeUUID+`","query_text":"hi"}`)
	if rec.Code != http.StatusUnauthorized {
		t.Fatalf("missing auth code = %d", rec.Code)
	}

	// Correct token → 200.
	req := httptest.NewRequest(http.MethodPost, "/api/v1/query", strings.NewReader(`{"scope_id":"`+scopeUUID+`","query_text":"hi"}`))
	req.Header.Set("Authorization", "Bearer secret-key")
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Fatalf("authed code = %d body=%s", rec.Code, rec.Body.String())
	}
}

func minInt(a, b int) int {
	if a < b {
		return a
	}
	return b
}
