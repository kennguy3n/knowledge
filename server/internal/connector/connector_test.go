package connector

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
)

const scopeUUID = "55555555-5555-5555-5555-555555555555"

// fakeSub is a configurable substrateAPI double recording call counts.
type fakeSub struct {
	createID    string
	createErr   error
	syncRaw     json.RawMessage
	syncErr     error
	fetchRaw    json.RawMessage
	fetchErr    error
	ingestErr   error
	statusRaw   json.RawMessage
	authRaw     json.RawMessage
	removeErr   error
	ingestCalls int
	synthCalls  int
	fetchCalls  int
	// syncGate, when non-nil, blocks SyncConnector until the channel is
	// closed or receives a value. Used to pin a webhook-triggered sync
	// in-flight so the concurrency semaphore can be exercised.
	syncGate chan struct{}
}

func (f *fakeSub) CreateConnector(context.Context, substrate.CreateConnectorRequest) (substrate.IDResponse, error) {
	id := f.createID
	if id == "" {
		id = "inst-1"
	}
	return substrate.IDResponse{ID: id}, f.createErr
}
func (f *fakeSub) ListConnectors(context.Context) (json.RawMessage, error) {
	return json.RawMessage(`[]`), nil
}
func (f *fakeSub) AuthenticateConnector(context.Context, string, substrate.AuthenticateRequest) (json.RawMessage, error) {
	if f.authRaw == nil {
		return json.RawMessage(`{"ok":true}`), nil
	}
	return f.authRaw, nil
}
func (f *fakeSub) SyncConnector(context.Context, string) (json.RawMessage, error) {
	if f.syncGate != nil {
		<-f.syncGate
	}
	return f.syncRaw, f.syncErr
}
func (f *fakeSub) RemoveConnector(context.Context, string) error { return f.removeErr }
func (f *fakeSub) ConnectorStatus(context.Context, string) (json.RawMessage, error) {
	if f.statusRaw == nil {
		return json.RawMessage(`{"state":"idle"}`), nil
	}
	return f.statusRaw, nil
}
func (f *fakeSub) FetchContent(context.Context, substrate.FetchContentRequest) (json.RawMessage, error) {
	f.fetchCalls++
	return f.fetchRaw, f.fetchErr
}
func (f *fakeSub) Ingest(context.Context, substrate.IngestRequest) (substrate.IDResponse, error) {
	f.ingestCalls++
	return substrate.IDResponse{ID: "ev-1"}, f.ingestErr
}
func (f *fakeSub) TriggerSynthesis(context.Context, substrate.SynthesisTriggerRequest) (json.RawMessage, error) {
	f.synthCalls++
	return json.RawMessage(`{}`), nil
}

func newSvc(sub substrateAPI) *Service {
	return New(sub, nil, Options{PublicBaseURL: "https://api.example.com/", SyncInterval: time.Minute})
}

func TestPipelineFetchIngestSynthesis(t *testing.T) {
	t.Parallel()
	sub := &fakeSub{fetchRaw: json.RawMessage(`{"body":"hello","source":"gdrive","importance":"Useful"}`)}
	s := newSvc(sub)
	res, err := s.runPipeline(context.Background(), "inst-1", scopeUUID, "GoogleDrive", []string{"r1", "r2"})
	if err != nil {
		t.Fatal(err)
	}
	if res.Fetched != 2 || res.Ingested != 2 {
		t.Fatalf("res = %+v", res)
	}
	if sub.synthCalls != 1 {
		t.Fatalf("synthesis triggered %d times", sub.synthCalls)
	}
}

func TestPipelineFetchContentUnavailable(t *testing.T) {
	t.Parallel()
	sub := &fakeSub{fetchErr: &httpx.Error{Status: http.StatusNotImplemented, Kind: "NotImplemented"}}
	s := newSvc(sub)
	res, err := s.runPipeline(context.Background(), "inst-1", scopeUUID, "GoogleDrive", []string{"r1"})
	if err != nil {
		t.Fatalf("501 should degrade gracefully: %v", err)
	}
	if !res.Unavailable || sub.ingestCalls != 0 {
		t.Fatalf("expected unavailable+no ingest: %+v", res)
	}
}

func TestPipelineEmptyBodySkipped(t *testing.T) {
	t.Parallel()
	sub := &fakeSub{fetchRaw: json.RawMessage(`{"body":""}`)}
	s := newSvc(sub)
	res, err := s.runPipeline(context.Background(), "inst-1", scopeUUID, "GoogleDrive", []string{"r1"})
	if err != nil {
		t.Fatal(err)
	}
	if res.Ingested != 0 || res.Skipped != 1 || sub.synthCalls != 0 {
		t.Fatalf("res = %+v synth=%d", res, sub.synthCalls)
	}
}

func TestHandleCreateValidationAndScheduling(t *testing.T) {
	t.Parallel()
	sub := &fakeSub{}
	s := newSvc(sub)
	h := s.Routes()

	// Invalid scope.
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodPost, "/", strings.NewReader(`{"kind":"GoogleDrive","scope_id":"bad"}`)))
	if rec.Code != http.StatusBadRequest {
		t.Fatalf("bad scope code = %d", rec.Code)
	}

	// Missing kind.
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodPost, "/", strings.NewReader(`{"scope_id":"`+scopeUUID+`"}`)))
	if rec.Code != http.StatusBadRequest {
		t.Fatalf("missing kind code = %d", rec.Code)
	}

	// Happy path registers a scheduled job.
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodPost, "/", strings.NewReader(`{"kind":"GoogleDrive","scope_id":"`+scopeUUID+`"}`)))
	if rec.Code != http.StatusCreated {
		t.Fatalf("create code = %d body=%s", rec.Code, rec.Body.String())
	}
	if s.sched.Count() != 1 {
		t.Fatalf("scheduler count = %d", s.sched.Count())
	}
}

func TestSyncOnce(t *testing.T) {
	t.Parallel()
	report := `{"instanceId":"inst-1","mode":"incremental","eventsTotal":1,"eventsIngested":1,"ingestedEvidenceIds":["r1"]}`
	sub := &fakeSub{
		syncRaw:  json.RawMessage(report),
		fetchRaw: json.RawMessage(`{"body":"data"}`),
	}
	s := newSvc(sub)
	s.store.put(registration{InstanceID: "inst-1", Kind: "GoogleDrive", ScopeID: scopeUUID})
	rep, res, err := s.syncOnce(context.Background(), "inst-1")
	if err != nil {
		t.Fatal(err)
	}
	if rep.EventsIngested != 1 || res.Ingested != 1 {
		t.Fatalf("rep=%+v res=%+v", rep, res)
	}
}

func TestSyncOnceMissingRegistration(t *testing.T) {
	t.Parallel()
	report := `{"instanceId":"inst-1","mode":"incremental","eventsTotal":1,"eventsIngested":1,"ingestedEvidenceIds":["r1"]}`
	sub := &fakeSub{
		syncRaw:  json.RawMessage(report),
		fetchRaw: json.RawMessage(`{"body":"data"}`),
	}
	s := newSvc(sub)
	// No registration for inst-1: syncOnce must error rather than
	// fall back to the connector instance id as the ingest scope.
	if _, _, err := s.syncOnce(context.Background(), "inst-1"); err == nil {
		t.Fatal("expected error for missing registration")
	}
	if sub.ingestCalls != 0 {
		t.Fatalf("ingest should not run without a scope; calls = %d", sub.ingestCalls)
	}
}

func TestOAuthStartAndCallback(t *testing.T) {
	t.Parallel()
	sub := &fakeSub{}
	s := newSvc(sub)
	h := s.Routes()

	// Create a connector to get a real instance id registered.
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodPost, "/", strings.NewReader(`{"kind":"GoogleDrive","scope_id":"`+scopeUUID+`"}`)))
	var reg registration
	if err := json.Unmarshal(rec.Body.Bytes(), &reg); err != nil {
		t.Fatal(err)
	}

	// Start OAuth.
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet,
		"/"+reg.InstanceID+"/oauth/start?client_id=cid&redirect_uri=https://cb", nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("oauth start code = %d body=%s", rec.Code, rec.Body.String())
	}
	var start map[string]string
	_ = json.Unmarshal(rec.Body.Bytes(), &start)
	if !strings.Contains(start["authorize_url"], "accounts.google.com") || start["state"] == "" {
		t.Fatalf("bad start payload: %+v", start)
	}

	// Callback consumes the state.
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet,
		"/oauth/callback?code=abc&state="+start["state"], nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("callback code = %d", rec.Code)
	}

	// Re-using the state fails (single-use).
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet,
		"/oauth/callback?code=abc&state="+start["state"], nil))
	if rec.Code != http.StatusBadRequest {
		t.Fatalf("reused state code = %d", rec.Code)
	}
}

func TestOAuthStartUnknownKind(t *testing.T) {
	t.Parallel()
	sub := &fakeSub{}
	s := newSvc(sub)
	// Register a connector whose kind has no OAuth provider.
	s.store.put(registration{InstanceID: "x", Kind: "FilesystemFixture", ScopeID: scopeUUID})
	h := s.Routes()
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet,
		"/x/oauth/start?client_id=c&redirect_uri=https://cb", nil))
	if rec.Code != http.StatusBadRequest {
		t.Fatalf("expected 400 for unknown kind, got %d", rec.Code)
	}
}

func TestOAuthStateTTLExpiry(t *testing.T) {
	t.Parallel()
	st := newStore()

	// A fresh state is single-use and resolves.
	st.putState("fresh", "inst-fresh")
	if id, ok := st.takeState("fresh"); !ok || id != "inst-fresh" {
		t.Fatalf("fresh state: id=%q ok=%v", id, ok)
	}

	// An expired state resolves as absent and is removed.
	st.states["stale"] = oauthState{InstanceID: "inst-stale", CreatedAt: time.Now().Add(-2 * oauthStateTTL)}
	if _, ok := st.takeState("stale"); ok {
		t.Fatal("expired state should not resolve")
	}

	// putState prunes abandoned states so the map cannot grow unbounded.
	st.states["abandoned"] = oauthState{InstanceID: "inst-old", CreatedAt: time.Now().Add(-2 * oauthStateTTL)}
	st.putState("new", "inst-new")
	if _, ok := st.states["abandoned"]; ok {
		t.Fatal("putState should have pruned the abandoned state")
	}
	if len(st.states) != 1 {
		t.Fatalf("expected only the new state to remain, got %d", len(st.states))
	}
}

func TestWebhookRegisterAndReceive(t *testing.T) {
	t.Parallel()
	report := `{"instanceId":"inst-1","mode":"incremental","ingestedEvidenceIds":[]}`
	sub := &fakeSub{syncRaw: json.RawMessage(report)}
	s := newSvc(sub)
	s.store.put(registration{InstanceID: "inst-1", Kind: "GoogleDrive", ScopeID: scopeUUID})
	h := s.Routes()

	// Receiving before registration → 404.
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodPost, "/inst-1/webhook", nil))
	if rec.Code != http.StatusNotFound {
		t.Fatalf("inactive webhook code = %d", rec.Code)
	}

	// Register.
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodPost, "/inst-1/webhook/register", nil))
	if rec.Code != http.StatusOK || !strings.Contains(rec.Body.String(), "/api/v1/connectors/inst-1/webhook") {
		t.Fatalf("register code = %d body=%s", rec.Code, rec.Body.String())
	}

	// Receive → 202 accepted.
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodPost, "/inst-1/webhook", nil))
	if rec.Code != http.StatusAccepted {
		t.Fatalf("receive code = %d", rec.Code)
	}
}

func TestWebhookConcurrencyBound(t *testing.T) {
	t.Parallel()
	gate := make(chan struct{})
	report := `{"instanceId":"inst-1","mode":"incremental","ingestedEvidenceIds":[]}`
	sub := &fakeSub{syncRaw: json.RawMessage(report), syncGate: gate}
	s := New(sub, nil, Options{
		PublicBaseURL:         "https://api.example.com",
		SyncInterval:          time.Minute,
		MaxWebhookConcurrency: 1,
	})
	s.store.put(registration{InstanceID: "inst-1", Kind: "GoogleDrive", ScopeID: scopeUUID, WebhookActive: true})
	h := s.Routes()

	// First webhook acquires the only slot; its sync blocks on the gate.
	if rec := req(h, http.MethodPost, "/inst-1/webhook", ""); rec.Code != http.StatusAccepted {
		t.Fatalf("first webhook code = %d", rec.Code)
	}
	waitFor(t, func() bool { return len(s.webhookSem) == 1 })

	// Second webhook finds the semaphore saturated → 429 (load shed),
	// and must NOT spawn another goroutine.
	if rec := req(h, http.MethodPost, "/inst-1/webhook", ""); rec.Code != http.StatusTooManyRequests {
		t.Fatalf("second webhook code = %d, want 429", rec.Code)
	}

	// Release the in-flight sync and let it drain.
	close(gate)
	waitFor(t, func() bool { return len(s.webhookSem) == 0 })

	// With the slot free again, a fresh webhook is accepted.
	if rec := req(h, http.MethodPost, "/inst-1/webhook", ""); rec.Code != http.StatusAccepted {
		t.Fatalf("third webhook code = %d", rec.Code)
	}
	// Drain the scheduler + in-flight syncs before the test exits.
	s.Stop()
}

// waitFor polls cond up to two seconds, failing the test if it never
// becomes true. Used to observe async goroutine state deterministically.
func waitFor(t *testing.T, cond func() bool) {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if cond() {
			return
		}
		time.Sleep(time.Millisecond)
	}
	t.Fatal("condition not met within timeout")
}

func TestRemoveUnschedules(t *testing.T) {
	t.Parallel()
	sub := &fakeSub{}
	s := newSvc(sub)
	s.store.put(registration{InstanceID: "inst-1", Kind: "GoogleDrive", ScopeID: scopeUUID})
	s.sched.Schedule("inst-1", time.Minute)
	h := s.Routes()
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodDelete, "/inst-1", nil))
	if rec.Code != http.StatusNoContent {
		t.Fatalf("remove code = %d", rec.Code)
	}
	if s.sched.Count() != 0 {
		t.Fatalf("scheduler still has %d jobs", s.sched.Count())
	}
	if _, ok := s.store.get("inst-1"); ok {
		t.Fatal("registration not deleted")
	}
}
