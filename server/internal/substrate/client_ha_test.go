package substrate

import (
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"sync/atomic"
	"testing"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
)

// haPair spins up two substrate stand-ins (a "primary" and a "standby")
// and returns an HA client wired to both plus per-node hit counters.
func haPair(t *testing.T, primary, standby http.HandlerFunc) (*Client, *int32, *int32) {
	t.Helper()
	var pHits, sHits int32
	ps := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		atomic.AddInt32(&pHits, 1)
		primary(w, r)
	}))
	ss := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		atomic.AddInt32(&sHits, 1)
		standby(w, r)
	}))
	t.Cleanup(ps.Close)
	t.Cleanup(ss.Close)
	c := NewHA(ps.URL, []string{ss.URL}, ps.Client())
	return c, &pHits, &sHits
}

// standby503 emulates a read-only standby rejecting a write with the
// 503 + replication-standby marker that guard_writable emits.
func standby503(w http.ResponseWriter, _ *http.Request) {
	w.WriteHeader(http.StatusServiceUnavailable)
	_, _ = w.Write([]byte(`{"kind":"Unavailable","detail":"replication-standby: node is standby"}`))
}

func TestHAWriteFailsOverToPromotedStandby(t *testing.T) {
	t.Parallel()
	// node0 is wedged as a standby (503); node1 accepts the write.
	c, p, s := haPair(t,
		standby503,
		func(w http.ResponseWriter, _ *http.Request) { _, _ = w.Write([]byte(`{"id":"ev-1"}`)) },
	)
	got, err := c.Ingest(context.Background(), IngestRequest{ScopeID: "s", Body: "b"})
	if err != nil {
		t.Fatalf("ingest after failover: %v", err)
	}
	if got.ID != "ev-1" {
		t.Fatalf("id = %q", got.ID)
	}
	if atomic.LoadInt32(p) != 1 || atomic.LoadInt32(s) != 1 {
		t.Fatalf("expected one attempt per node, got primary=%d standby=%d", *p, *s)
	}
	// The believed primary should now be node1: a second write must hit
	// it directly without retrying the wedged node0.
	if _, err := c.Ingest(context.Background(), IngestRequest{ScopeID: "s", Body: "b2"}); err != nil {
		t.Fatalf("second ingest: %v", err)
	}
	if atomic.LoadInt32(p) != 1 {
		t.Fatalf("believed-primary not updated: node0 hit again (primary=%d)", *p)
	}
	if atomic.LoadInt32(s) != 2 {
		t.Fatalf("second write should target node1, standby hits=%d", *s)
	}
}

func TestHAWritePrefersPrimary(t *testing.T) {
	t.Parallel()
	c, p, s := haPair(t,
		func(w http.ResponseWriter, _ *http.Request) { _, _ = w.Write([]byte(`{"id":"ev-1"}`)) },
		func(w http.ResponseWriter, _ *http.Request) {
			t.Error("standby must not receive a write while primary is healthy")
			w.WriteHeader(http.StatusInternalServerError)
		},
	)
	if _, err := c.Ingest(context.Background(), IngestRequest{ScopeID: "s", Body: "b"}); err != nil {
		t.Fatalf("ingest: %v", err)
	}
	if atomic.LoadInt32(p) != 1 || atomic.LoadInt32(s) != 0 {
		t.Fatalf("primary=%d standby=%d", *p, *s)
	}
}

func TestHAReadPrefersStandby(t *testing.T) {
	t.Parallel()
	c, p, s := haPair(t,
		func(w http.ResponseWriter, _ *http.Request) {
			t.Error("primary must not receive a read while standby is healthy")
			w.WriteHeader(http.StatusInternalServerError)
		},
		func(w http.ResponseWriter, _ *http.Request) { _, _ = w.Write([]byte(`[]`)) },
	)
	if _, err := c.Query(context.Background(), QueryRequest{ScopeID: "s", QueryText: "q"}); err != nil {
		t.Fatalf("query: %v", err)
	}
	if atomic.LoadInt32(p) != 0 || atomic.LoadInt32(s) != 1 {
		t.Fatalf("primary=%d standby=%d", *p, *s)
	}
}

func TestHAReadFallsBackToPrimary(t *testing.T) {
	t.Parallel()
	// Standby is down (503); the read must fall back to the primary.
	c, p, s := haPair(t,
		func(w http.ResponseWriter, _ *http.Request) { _, _ = w.Write([]byte(`[]`)) },
		standby503,
	)
	if _, err := c.Query(context.Background(), QueryRequest{ScopeID: "s", QueryText: "q"}); err != nil {
		t.Fatalf("query: %v", err)
	}
	if atomic.LoadInt32(s) != 1 || atomic.LoadInt32(p) != 1 {
		t.Fatalf("expected standby then primary, got primary=%d standby=%d", *p, *s)
	}
}

func TestHAReadMissFallsBackToPrimary(t *testing.T) {
	t.Parallel()
	// A standby that hasn't yet replayed the primary's WAL 404s on a
	// just-written row; the read must fall through to the primary,
	// which has it (read-after-write consistency).
	notFound := func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusNotFound)
		_, _ = w.Write([]byte(`{"kind":"NotFound","detail":"no such evidence"}`))
	}
	c, p, s := haPair(t,
		func(w http.ResponseWriter, _ *http.Request) { _, _ = w.Write([]byte(`{"id":"ev-1"}`)) },
		notFound,
	)
	got, err := c.GetEvidence(context.Background(), "ev-1")
	if err != nil {
		t.Fatalf("get after standby miss: %v", err)
	}
	if string(got) != `{"id":"ev-1"}` {
		t.Fatalf("body = %s", got)
	}
	if atomic.LoadInt32(s) != 1 || atomic.LoadInt32(p) != 1 {
		t.Fatalf("expected standby then primary, got primary=%d standby=%d", *p, *s)
	}
}

func TestHAReadMissOnPrimaryIsReturned(t *testing.T) {
	t.Parallel()
	// When both nodes 404 the miss is genuine and surfaced to the
	// caller — we must not loop or swallow it.
	notFound := func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusNotFound)
		_, _ = w.Write([]byte(`{"kind":"NotFound","detail":"no such evidence"}`))
	}
	c, p, s := haPair(t, notFound, notFound)
	_, err := c.GetEvidence(context.Background(), "missing")
	var apiErr *httpx.Error
	if err == nil || !errors.As(err, &apiErr) || apiErr.Status != http.StatusNotFound {
		t.Fatalf("expected 404, got %v", err)
	}
	if atomic.LoadInt32(s) != 1 || atomic.LoadInt32(p) != 1 {
		t.Fatalf("both nodes should be tried once, got primary=%d standby=%d", *p, *s)
	}
}

func TestHAWriteMissNotRetried(t *testing.T) {
	t.Parallel()
	// A 404 on a write (e.g. POST to a missing sub-resource) is a real
	// application error: it must surface from the primary without
	// touching the standby (the standby would reject the write anyway).
	c, p, s := haPair(t,
		func(w http.ResponseWriter, _ *http.Request) {
			w.WriteHeader(http.StatusNotFound)
			_, _ = w.Write([]byte(`{"kind":"NotFound","detail":"no such connector"}`))
		},
		func(w http.ResponseWriter, _ *http.Request) {
			t.Error("standby must not be tried after a write 404")
		},
	)
	_, err := c.SyncConnector(context.Background(), "missing")
	var apiErr *httpx.Error
	if err == nil || !errors.As(err, &apiErr) || apiErr.Status != http.StatusNotFound {
		t.Fatalf("expected 404, got %v", err)
	}
	if atomic.LoadInt32(p) != 1 || atomic.LoadInt32(s) != 0 {
		t.Fatalf("primary=%d standby=%d", *p, *s)
	}
}

func TestHANonFailoverErrorNotRetried(t *testing.T) {
	t.Parallel()
	// A 400 from the primary is a real application error: it must be
	// returned immediately without touching the standby.
	c, p, s := haPair(t,
		func(w http.ResponseWriter, _ *http.Request) {
			w.WriteHeader(http.StatusBadRequest)
			_, _ = w.Write([]byte(`{"kind":"BadRequest","detail":"bad scope"}`))
		},
		func(w http.ResponseWriter, _ *http.Request) {
			t.Error("standby must not be tried after a non-failover error")
		},
	)
	_, err := c.Ingest(context.Background(), IngestRequest{ScopeID: "s", Body: "b"})
	var apiErr *httpx.Error
	if err == nil || !errors.As(err, &apiErr) || apiErr.Status != http.StatusBadRequest {
		t.Fatalf("expected 400, got %v", err)
	}
	if atomic.LoadInt32(p) != 1 || atomic.LoadInt32(s) != 0 {
		t.Fatalf("primary=%d standby=%d", *p, *s)
	}
}

func TestHAUpstream502NotTreatedAsFailover(t *testing.T) {
	t.Parallel()
	// A 502 from the substrate's *own* upstream (e.g. a connector
	// failure) is an application error, not node unreachability, so it
	// must not fail over.
	c, _, s := haPair(t,
		func(w http.ResponseWriter, _ *http.Request) {
			w.WriteHeader(http.StatusBadGateway)
			_, _ = w.Write([]byte(`{"kind":"Connector","detail":"upstream timeout"}`))
		},
		func(w http.ResponseWriter, _ *http.Request) {
			t.Error("standby must not be tried on an upstream 502")
		},
	)
	_, err := c.Ingest(context.Background(), IngestRequest{ScopeID: "s", Body: "b"})
	var apiErr *httpx.Error
	if err == nil || !errors.As(err, &apiErr) || apiErr.Kind != "Connector" {
		t.Fatalf("expected Connector 502, got %v", err)
	}
	if atomic.LoadInt32(s) != 0 {
		t.Fatalf("standby should not be hit, hits=%d", *s)
	}
}
func TestIsReadRoute(t *testing.T) {
	t.Parallel()
	reads := []struct {
		method, path string
	}{
		{http.MethodGet, "/health"},
		{http.MethodGet, "/connectors"},
		{http.MethodGet, "/synthesis/x/status"},
		{http.MethodPost, "/query"},
		{http.MethodPost, "/memories"},
		{http.MethodPost, "/synthesis/recent"},
		{http.MethodPost, "/permission/check"},
	}
	for _, r := range reads {
		if !isReadRoute(r.method, r.path) {
			t.Errorf("%s %s should be a read route", r.method, r.path)
		}
	}
	writes := []struct {
		method, path string
	}{
		{http.MethodPost, "/ingest"},
		{http.MethodPost, "/pin"},
		{http.MethodPost, "/connectors"},
		{http.MethodPost, "/permission/grant"},
		{http.MethodDelete, "/connectors/x"},
	}
	for _, w := range writes {
		if isReadRoute(w.method, w.path) {
			t.Errorf("%s %s should be a write route", w.method, w.path)
		}
	}
}

func TestIsFailoverErr(t *testing.T) {
	t.Parallel()
	if !isFailoverErr(httpx.NewError(http.StatusBadGateway, "SubstrateUnavailable", "x")) {
		t.Error("502 SubstrateUnavailable should fail over")
	}
	if !isFailoverErr(httpx.NewError(http.StatusServiceUnavailable, "Unavailable", "x")) {
		t.Error("503 should fail over")
	}
	if isFailoverErr(httpx.NewError(http.StatusBadGateway, "Connector", "x")) {
		t.Error("upstream 502 should not fail over")
	}
	if isFailoverErr(httpx.NewError(http.StatusNotFound, "NotFound", "x")) {
		t.Error("404 should not fail over")
	}
}
