package connector

import (
	"context"
	"crypto/hmac"
	"crypto/sha256"
	"encoding/hex"
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
	listRaw     json.RawMessage
	listErr     error
	removeErr   error
	ingestCalls int
	synthCalls  int
	// lastIngestSource records the Source of the most recent Ingest call
	// so the connector-kind source fallback can be asserted.
	lastIngestSource string
	fetchCalls       int
	removeCalls      int
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
	if f.listErr != nil {
		return nil, f.listErr
	}
	if f.listRaw == nil {
		return json.RawMessage(`[]`), nil
	}
	return f.listRaw, nil
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
func (f *fakeSub) RemoveConnector(context.Context, string) error {
	f.removeCalls++
	return f.removeErr
}
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
func (f *fakeSub) Ingest(_ context.Context, req substrate.IngestRequest) (substrate.IDResponse, error) {
	f.ingestCalls++
	f.lastIngestSource = req.Source
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
	sub := &fakeSub{fetchRaw: json.RawMessage(`{"body":"hello","source":"Slack","importance":"Useful"}`)}
	s := newSvc(sub)
	res, err := s.runPipeline(context.Background(), "inst-1", scopeUUID, "google_drive", []string{"r1", "r2"})
	if err != nil {
		t.Fatal(err)
	}
	if res.Fetched != 2 || res.Ingested != 2 {
		t.Fatalf("res = %+v", res)
	}
	if sub.synthCalls != 1 {
		t.Fatalf("synthesis triggered %d times", sub.synthCalls)
	}
	// Content declared its own source, so the connector-kind fallback
	// must not override it.
	if sub.lastIngestSource != "Slack" {
		t.Fatalf("ingest source = %q, want content-declared \"Slack\"", sub.lastIngestSource)
	}
}

// TestPipelineSourceFallbackMapsKind verifies that when fetched content
// omits a source, the pipeline ingests the connector kind mapped to its
// coarse SourceKind tag (not the raw snake_case kind, which would fail
// SourceKind deserialization in the substrate).
func TestPipelineSourceFallbackMapsKind(t *testing.T) {
	t.Parallel()
	sub := &fakeSub{fetchRaw: json.RawMessage(`{"body":"hello"}`)}
	s := newSvc(sub)
	if _, err := s.runPipeline(context.Background(), "inst-1", scopeUUID, "google_drive", []string{"r1"}); err != nil {
		t.Fatal(err)
	}
	if sub.lastIngestSource != "GoogleWorkspace" {
		t.Fatalf("fallback ingest source = %q, want \"GoogleWorkspace\"", sub.lastIngestSource)
	}
}

func TestSourceKindForConnector(t *testing.T) {
	t.Parallel()
	cases := map[string]string{
		// Google Workspace family collapses to one transport tag.
		"google_drive":    "GoogleWorkspace",
		"google_docs":     "GoogleWorkspace",
		"google_sheets":   "GoogleWorkspace",
		"google_calendar": "GoogleWorkspace",
		"google_meet":     "GoogleWorkspace",
		// Microsoft Graph family.
		"one_drive":   "MicrosoftGraph",
		"share_point": "MicrosoftGraph",
		"teams":       "MicrosoftGraph",
		"slack":       "Slack",
		"jira":        "Atlassian",
		"confluence":  "Atlassian",
		"hub_spot":    "HubSpot",
		"email":       "Email",
		// Kinds without a dedicated SourceKind variant collapse to Other.
		"notion":          "Other",
		"git_hub":         "Other",
		"figma":           "Other",
		"generic_webhook": "Other",
		"":                "Other",
	}
	for kind, want := range cases {
		if got := sourceKindForConnector(kind); got != want {
			t.Errorf("sourceKindForConnector(%q) = %q, want %q", kind, got, want)
		}
	}
}

func TestPipelineFetchContentUnavailable(t *testing.T) {
	t.Parallel()
	sub := &fakeSub{fetchErr: &httpx.Error{Status: http.StatusNotImplemented, Kind: "NotImplemented"}}
	s := newSvc(sub)
	res, err := s.runPipeline(context.Background(), "inst-1", scopeUUID, "google_drive", []string{"r1"})
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
	res, err := s.runPipeline(context.Background(), "inst-1", scopeUUID, "google_drive", []string{"r1"})
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
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodPost, "/", strings.NewReader(`{"kind":"google_drive","scope_id":"bad"}`)))
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
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodPost, "/", strings.NewReader(`{"kind":"google_drive","scope_id":"`+scopeUUID+`"}`)))
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
	s.store.put(registration{InstanceID: "inst-1", Kind: "google_drive", ScopeID: scopeUUID})
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

	// Create a connector to get a real instance id registered. The kind
	// is the on-the-wire snake_case ConnectorKindTag the admin SPA sends;
	// OAuth start must resolve it against defaultProviders (keyed the same
	// way) rather than a PascalCase variant.
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodPost, "/", strings.NewReader(`{"kind":"google_drive","scope_id":"`+scopeUUID+`"}`)))
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

// TestAuthorizeURLUsesSnakeCaseKinds locks the OAuth provider registry to
// the on-the-wire snake_case ConnectorKindTag values. The admin SPA and the
// substrate both speak snake_case (`ffi::ConnectorKindTag` is
// `rename_all = "snake_case"`), and handleOAuthStart looks up `reg.Kind`
// verbatim — so a PascalCase key would silently 400 every wizard OAuth
// start. Guards against regressing the map keys back to PascalCase.
func TestAuthorizeURLUsesSnakeCaseKinds(t *testing.T) {
	t.Parallel()
	// Every kind the first-run wizard offers (the OAuth-capable subset in
	// admin/src/lib/connectorKinds.ts) must resolve to a provider.
	for _, kind := range []string{
		"google_drive", "one_drive", "notion", "slack",
		"git_hub", "jira", "confluence",
	} {
		if _, ok := authorizeURL(kind, "cid", "https://cb", "state"); !ok {
			t.Errorf("snake_case kind %q has no OAuth provider", kind)
		}
	}
	// PascalCase is the historical bug: the wire never carries it, so it
	// must not resolve (otherwise the map drifted back to PascalCase).
	if _, ok := authorizeURL("GoogleDrive", "cid", "https://cb", "state"); ok {
		t.Error("PascalCase kind \"GoogleDrive\" resolved; map keys must be snake_case")
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
	s.store.put(registration{InstanceID: "inst-1", Kind: "google_drive", ScopeID: scopeUUID})
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
	s.store.put(registration{InstanceID: "inst-1", Kind: "google_drive", ScopeID: scopeUUID, WebhookActive: true})
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
	s.store.put(registration{InstanceID: "inst-1", Kind: "google_drive", ScopeID: scopeUUID})
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

// hmacSig returns the "sha256=<hex>" signature an inbound webhook must
// carry for body signed with secret.
func hmacSig(secret, body string) string {
	mac := hmac.New(sha256.New, []byte(secret))
	mac.Write([]byte(body))
	return "sha256=" + hex.EncodeToString(mac.Sum(nil))
}

// signedReq posts body to path with the X-Webhook-Signature header set
// verbatim to sig (caller controls it so the missing/invalid cases can
// be exercised).
func signedReq(h http.Handler, path, body, sig string) *httptest.ResponseRecorder {
	r := httptest.NewRequest(http.MethodPost, path, strings.NewReader(body))
	if sig != "" {
		r.Header.Set(webhookSignatureHeader, sig)
	}
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, r)
	return rec
}

func webhookSvc(t *testing.T, opts Options) *Service {
	t.Helper()
	if opts.PublicBaseURL == "" {
		opts.PublicBaseURL = "https://api.example.com"
	}
	if opts.SyncInterval == 0 {
		opts.SyncInterval = time.Minute
	}
	report := `{"instanceId":"inst-1","mode":"incremental","ingestedEvidenceIds":[]}`
	sub := &fakeSub{syncRaw: json.RawMessage(report)}
	s := New(sub, nil, opts)
	s.store.put(registration{InstanceID: "inst-1", Kind: "google_drive", ScopeID: scopeUUID, WebhookActive: true})
	t.Cleanup(s.Stop)
	return s
}

// TestWebhookHMACVerification covers the three signature outcomes once a
// signing secret is configured: a body signed with the right key is
// accepted (202), while a wrong signature and an absent signature are
// both rejected (401) before any background sync is scheduled.
func TestWebhookHMACVerification(t *testing.T) {
	t.Parallel()
	const secret = "s3cr3t-webhook-key"
	const body = `{"event":"push"}`
	h := webhookSvc(t, Options{WebhookSecret: secret}).Routes()

	if rec := signedReq(h, "/inst-1/webhook", body, hmacSig(secret, body)); rec.Code != http.StatusAccepted {
		t.Fatalf("valid signature code = %d, want 202; body=%s", rec.Code, rec.Body.String())
	}

	// Signature computed over a different body (i.e. wrong digest).
	if rec := signedReq(h, "/inst-1/webhook", body, hmacSig(secret, "tampered")); rec.Code != http.StatusUnauthorized {
		t.Fatalf("invalid signature code = %d, want 401", rec.Code)
	}

	// Signature produced with a different key.
	if rec := signedReq(h, "/inst-1/webhook", body, hmacSig("wrong-key", body)); rec.Code != http.StatusUnauthorized {
		t.Fatalf("wrong-key signature code = %d, want 401", rec.Code)
	}

	// No signature header at all.
	if rec := signedReq(h, "/inst-1/webhook", body, ""); rec.Code != http.StatusUnauthorized {
		t.Fatalf("missing signature code = %d, want 401", rec.Code)
	}

	// Present but not valid hex.
	if rec := signedReq(h, "/inst-1/webhook", body, "sha256=not-hex"); rec.Code != http.StatusUnauthorized {
		t.Fatalf("malformed signature code = %d, want 401", rec.Code)
	}
}

// TestWebhookBodyTooLarge verifies that an oversized signed body is
// rejected with 413 (rather than being silently truncated and then
// reported as an invalid signature, which would be misleading).
func TestWebhookBodyTooLarge(t *testing.T) {
	t.Parallel()
	const secret = "s3cr3t-webhook-key"
	body := strings.Repeat("a", (1<<20)+1) // one byte over the 1 MiB cap
	h := webhookSvc(t, Options{WebhookSecret: secret}).Routes()
	if rec := signedReq(h, "/inst-1/webhook", body, hmacSig(secret, body)); rec.Code != http.StatusRequestEntityTooLarge {
		t.Fatalf("oversized body code = %d, want 413; body=%s", rec.Code, rec.Body.String())
	}
}

// TestWebhookHMACDisabledWhenNoSecret verifies that, with no signing
// secret configured, the endpoint stays open (dev mode / upstream-
// terminated auth): an unsigned webhook is accepted rather than 401.
func TestWebhookHMACDisabledWhenNoSecret(t *testing.T) {
	t.Parallel()
	h := webhookSvc(t, Options{}).Routes()
	if rec := signedReq(h, "/inst-1/webhook", `{"event":"push"}`, ""); rec.Code != http.StatusAccepted {
		t.Fatalf("unsigned webhook with no secret code = %d, want 202", rec.Code)
	}
}

// TestProviderRateLimiterPerProviderIsolation unit-tests the limiter:
// each provider kind draws from its own bucket, so exhausting one does
// not throttle another, and per-provider overrides take effect.
func TestProviderRateLimiterPerProviderIsolation(t *testing.T) {
	t.Parallel()
	l := newProviderRateLimiter(RateLimitConfig{
		Default:     ProviderRateLimit{RPS: 1, Burst: 1},
		PerProvider: map[string]ProviderRateLimit{"slack": {RPS: 1, Burst: 3}},
	})
	// google_drive falls back to the default burst of 1.
	if !l.allow("google_drive") {
		t.Fatal("first google_drive call should be allowed")
	}
	if l.allow("google_drive") {
		t.Fatal("second google_drive call should be rate limited")
	}
	// slack has its own bucket (burst 3) and is unaffected by the
	// google_drive exhaustion above.
	for i := 0; i < 3; i++ {
		if !l.allow("slack") {
			t.Fatalf("slack call %d should be allowed by its own bucket", i+1)
		}
	}
	if l.allow("slack") {
		t.Fatal("fourth slack call should be rate limited")
	}
}

// TestWebhookRateLimitShedsLoad drives the limiter through the request
// path: with a per-provider burst of 1, the second inbound webhook for
// the same provider is shed with 429 before a sync is scheduled.
func TestWebhookRateLimitShedsLoad(t *testing.T) {
	t.Parallel()
	s := webhookSvc(t, Options{
		RateLimit: RateLimitConfig{Default: ProviderRateLimit{RPS: 1, Burst: 1}},
	})
	h := s.Routes()

	if rec := req(h, http.MethodPost, "/inst-1/webhook", ""); rec.Code != http.StatusAccepted {
		t.Fatalf("first webhook code = %d, want 202", rec.Code)
	}
	if rec := req(h, http.MethodPost, "/inst-1/webhook", ""); rec.Code != http.StatusTooManyRequests {
		t.Fatalf("second webhook code = %d, want 429 (rate limited)", rec.Code)
	}
}
