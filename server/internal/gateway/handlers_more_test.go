package gateway

import (
	"context"
	"encoding/json"
	"net/http"
	"testing"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
)

// errSub returns an httpx error from every method to exercise the
// gateway's error-propagation paths.
type errSub struct{ err error }

func (e errSub) Ingest(context.Context, substrate.IngestRequest) (substrate.IDResponse, error) {
	return substrate.IDResponse{}, e.err
}
func (e errSub) Query(context.Context, substrate.QueryRequest) (json.RawMessage, error) {
	return nil, e.err
}
func (e errSub) GetEvidence(context.Context, string) (json.RawMessage, error) { return nil, e.err }
func (e errSub) ListMemories(context.Context, substrate.ListMemoriesRequest) (json.RawMessage, error) {
	return nil, e.err
}
func (e errSub) CreateMemory(context.Context, substrate.CreateMemoryRequest) (json.RawMessage, error) {
	return nil, e.err
}
func (e errSub) Pin(context.Context, string) error   { return e.err }
func (e errSub) Unpin(context.Context, string) error { return e.err }
func (e errSub) ChannelMemory(context.Context, string) (json.RawMessage, error) {
	return nil, e.err
}
func (e errSub) ConceptGraph(context.Context, string) (json.RawMessage, error) {
	return nil, e.err
}
func (e errSub) ReasoningContradictions(context.Context, substrate.ReasoningScopeRequest) (json.RawMessage, error) {
	return nil, e.err
}
func (e errSub) ReasoningDrift(context.Context, substrate.ReasoningScopeRequest) (json.RawMessage, error) {
	return nil, e.err
}
func (e errSub) ReasoningExplain(context.Context, substrate.ExplainQueryRequest) (json.RawMessage, error) {
	return nil, e.err
}
func (e errSub) ForgetScope(context.Context, string) error { return e.err }
func (e errSub) TriggerSynthesis(context.Context, substrate.SynthesisTriggerRequest) (json.RawMessage, error) {
	return nil, e.err
}
func (e errSub) TriggerDomainSynthesis(context.Context, substrate.ServerSynthesisRequest) (json.RawMessage, error) {
	return nil, e.err
}
func (e errSub) TriggerTenantSynthesis(context.Context, substrate.ServerSynthesisRequest) (json.RawMessage, error) {
	return nil, e.err
}
func (e errSub) SynthesisStatus(context.Context, string) (json.RawMessage, error) {
	return nil, e.err
}
func (e errSub) RecentSyntheses(context.Context, substrate.RecentSynthesisRequest) (json.RawMessage, error) {
	return nil, e.err
}
func (e errSub) Health(context.Context) (json.RawMessage, error) { return nil, e.err }

func TestGetEvidenceAndRecent(t *testing.T) {
	t.Parallel()
	h := NewRouter(Deps{Substrate: &fakeSub{}})

	if rec := do(h, http.MethodGet, "/api/v1/evidence/"+scopeUUID, ""); rec.Code != http.StatusOK {
		t.Fatalf("get evidence code = %d", rec.Code)
	}
	if rec := do(h, http.MethodGet, "/api/v1/evidence/not-a-uuid", ""); rec.Code != http.StatusBadRequest {
		t.Fatalf("bad evidence id code = %d", rec.Code)
	}
	if rec := do(h, http.MethodGet, "/api/v1/synthesis/recent?scope_id="+scopeUUID, ""); rec.Code != http.StatusOK {
		t.Fatalf("recent code = %d", rec.Code)
	}
	if rec := do(h, http.MethodGet, "/api/v1/synthesis/recent?scope_id=bad", ""); rec.Code != http.StatusBadRequest {
		t.Fatalf("recent bad scope code = %d", rec.Code)
	}
}

func TestValidationErrors(t *testing.T) {
	t.Parallel()
	h := NewRouter(Deps{Substrate: &fakeSub{}})
	cases := []struct {
		method, path, body string
		want               int
	}{
		{http.MethodPost, "/api/v1/query", `{"scope_id":"bad","query_text":"x"}`, http.StatusBadRequest},
		{http.MethodPost, "/api/v1/query", `{"scope_id":"` + scopeUUID + `","query_text":""}`, http.StatusBadRequest},
		{http.MethodPost, "/api/v1/forget/bad", "", http.StatusBadRequest},
		{http.MethodPost, "/api/v1/synthesis/trigger", `{"scope_id":"bad"}`, http.StatusBadRequest},
		{http.MethodGet, "/api/v1/synthesis/bad/status", "", http.StatusBadRequest},
		{http.MethodGet, "/api/v1/memories?scope_id=bad", "", http.StatusBadRequest},
		{http.MethodPost, "/api/v1/memories", `{"scope_id":"bad","observation_type":"note","content":"x"}`, http.StatusBadRequest},
		{http.MethodPost, "/api/v1/memories", `{bad json`, http.StatusBadRequest},
		{http.MethodPost, "/api/v1/ingest", `{bad json`, http.StatusBadRequest},
	}
	for _, c := range cases {
		if rec := do(h, c.method, c.path, c.body); rec.Code != c.want {
			t.Errorf("%s %s = %d, want %d", c.method, c.path, rec.Code, c.want)
		}
	}
}

func TestDownstreamErrorPropagation(t *testing.T) {
	t.Parallel()
	h := NewRouter(Deps{Substrate: errSub{err: httpx.NewError(http.StatusBadGateway, "Substrate", "boom")}})
	cases := []struct{ method, path, body string }{
		{http.MethodPost, "/api/v1/ingest", `{"scope_id":"` + scopeUUID + `","body":"x"}`},
		{http.MethodPost, "/api/v1/query", `{"scope_id":"` + scopeUUID + `","query_text":"x"}`},
		{http.MethodGet, "/api/v1/evidence/" + scopeUUID, ""},
		{http.MethodPost, "/api/v1/forget/" + scopeUUID, ""},
		{http.MethodPost, "/api/v1/synthesis/trigger", `{"scope_id":"` + scopeUUID + `"}`},
		{http.MethodPost, "/api/v1/synthesis/domain", `{"scope_id":"` + scopeUUID + `"}`},
		{http.MethodPost, "/api/v1/synthesis/tenant", `{"scope_id":"` + scopeUUID + `"}`},
		{http.MethodGet, "/api/v1/synthesis/recent?scope_id=" + scopeUUID, ""},
		{http.MethodGet, "/api/v1/synthesis/" + scopeUUID + "/status", ""},
		{http.MethodGet, "/api/v1/memories?scope_id=" + scopeUUID, ""},
		{http.MethodPost, "/api/v1/memories", `{"scope_id":"` + scopeUUID + `","observation_type":"note","content":"x"}`},
		{http.MethodGet, "/api/v1/memories/channel?scope_id=" + scopeUUID, ""},
		{http.MethodGet, "/api/v1/memories/concept-graph?scope_id=" + scopeUUID, ""},
		{http.MethodPost, "/api/v1/reasoning/contradictions", `{"scope_id":"` + scopeUUID + `"}`},
		{http.MethodPost, "/api/v1/reasoning/drift", `{"scope_id":"` + scopeUUID + `"}`},
		{http.MethodPost, "/api/v1/reasoning/explain", `{"scope_id":"` + scopeUUID + `","query":"x"}`},
	}
	for _, c := range cases {
		if rec := do(h, c.method, c.path, c.body); rec.Code != http.StatusBadGateway {
			t.Errorf("%s %s = %d, want 502", c.method, c.path, rec.Code)
		}
	}
}

func TestSSEErrorFrame(t *testing.T) {
	t.Parallel()
	h := NewRouter(Deps{Substrate: errSub{err: httpx.NewError(http.StatusBadGateway, "Substrate", "boom")}})
	rec := do(h, http.MethodGet, "/api/v1/synthesis/"+scopeUUID+"/status", "")
	// Non-stream path returns the error status.
	if rec.Code != http.StatusBadGateway {
		t.Fatalf("status err code = %d", rec.Code)
	}
}
