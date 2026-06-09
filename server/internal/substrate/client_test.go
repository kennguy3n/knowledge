package substrate

import (
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
)

func newTestClient(t *testing.T, h http.HandlerFunc) *Client {
	t.Helper()
	srv := httptest.NewServer(h)
	t.Cleanup(srv.Close)
	return New(srv.URL, srv.Client())
}

func TestIngestSuccess(t *testing.T) {
	t.Parallel()
	c := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/ingest" || r.Method != http.MethodPost {
			t.Errorf("unexpected request %s %s", r.Method, r.URL.Path)
		}
		_, _ = w.Write([]byte(`{"id":"ev-1"}`))
	})
	got, err := c.Ingest(context.Background(), IngestRequest{ScopeID: "s", Body: "b"})
	if err != nil {
		t.Fatal(err)
	}
	if got.ID != "ev-1" {
		t.Fatalf("id = %q", got.ID)
	}
}

func TestRequestIDPropagation(t *testing.T) {
	t.Parallel()
	c := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("X-Request-Id") != "rid-42" {
			t.Errorf("X-Request-Id = %q", r.Header.Get("X-Request-Id"))
		}
		_, _ = w.Write([]byte(`[]`))
	})
	ctx := WithRequestID(context.Background(), "rid-42")
	if _, err := c.Query(ctx, QueryRequest{ScopeID: "s", QueryText: "q"}); err != nil {
		t.Fatal(err)
	}
}

func TestErrorMappingPreservesStatusAndKind(t *testing.T) {
	t.Parallel()
	c := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusNotFound)
		_, _ = w.Write([]byte(`{"kind":"NotFound","detail":"no such evidence"}`))
	})
	_, err := c.GetEvidence(context.Background(), "missing")
	var apiErr *httpx.Error
	if !errors.As(err, &apiErr) {
		t.Fatalf("err type = %T", err)
	}
	if apiErr.Status != http.StatusNotFound || apiErr.Kind != "NotFound" {
		t.Fatalf("mapped err = %+v", apiErr)
	}
}

func TestFetchContentNotImplemented(t *testing.T) {
	t.Parallel()
	c := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusNotImplemented)
		_, _ = w.Write([]byte(`{"kind":"NotImplemented","detail":"pending session B"}`))
	})
	_, err := c.FetchContent(context.Background(), FetchContentRequest{InstanceID: "i", ContentRef: "r"})
	var apiErr *httpx.Error
	if !errors.As(err, &apiErr) || apiErr.Status != http.StatusNotImplemented {
		t.Fatalf("expected 501, got %v", err)
	}
}

func TestPermissionCheck(t *testing.T) {
	t.Parallel()
	c := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte(`{"allowed":true}`))
	})
	allowed, err := c.PermissionCheck(context.Background(), RelationTuple{Relation: "owner"})
	if err != nil || !allowed {
		t.Fatalf("allowed=%v err=%v", allowed, err)
	}
}

func TestUnreachableSubstrate(t *testing.T) {
	t.Parallel()
	c := New("http://127.0.0.1:0", &http.Client{})
	_, err := c.Health(context.Background())
	var apiErr *httpx.Error
	if !errors.As(err, &apiErr) || apiErr.Status != http.StatusBadGateway {
		t.Fatalf("expected 502, got %v", err)
	}
}

// TestSynthClientInheritsTransport verifies that the dedicated
// synthesis client reuses the caller-supplied transport (so custom TLS /
// proxy / mTLS tuning is honoured) while widening only the end-to-end
// timeout to synthesisTimeout.
func TestSynthClientInheritsTransport(t *testing.T) {
	t.Parallel()
	custom := &http.Transport{MaxIdleConnsPerHost: 7}
	hc := &http.Client{Transport: custom, Timeout: 30 * time.Second}

	c := New("http://example.invalid", hc)
	if c.synthHTTP.Transport != http.RoundTripper(custom) {
		t.Fatalf("synthHTTP did not inherit caller transport: got %#v", c.synthHTTP.Transport)
	}
	if c.synthHTTP.Timeout != synthesisTimeout {
		t.Fatalf("synthHTTP timeout = %v, want %v", c.synthHTTP.Timeout, synthesisTimeout)
	}
	if c.http.Timeout == c.synthHTTP.Timeout {
		t.Fatalf("expected base and synth timeouts to differ; both = %v", c.http.Timeout)
	}
}

// TestSynthClientFallsBackWhenNoTransport guards the nil-transport path:
// a base client with no explicit transport still yields a usable synth
// client (hardened default transport, widened timeout).
func TestSynthClientFallsBackWhenNoTransport(t *testing.T) {
	t.Parallel()
	c := New("http://example.invalid", &http.Client{})
	if c.synthHTTP.Transport == nil {
		t.Fatal("synthHTTP.Transport should fall back to a default, got nil")
	}
	if c.synthHTTP.Timeout != synthesisTimeout {
		t.Fatalf("synthHTTP timeout = %v, want %v", c.synthHTTP.Timeout, synthesisTimeout)
	}
}
