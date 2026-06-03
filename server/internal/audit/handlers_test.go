package audit

import (
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"
)

func auditReq(h http.Handler, path string) *httptest.ResponseRecorder {
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, path, nil))
	return rec
}

func TestHandleQueryParams(t *testing.T) {
	t.Parallel()
	s := New(NewMemoryStore())
	_, _ = s.Record(context.Background(), Event{TenantID: "t1", Action: "export", Actor: "a"})
	h := s.Routes()

	now := time.Now().UTC().Format(time.RFC3339)
	if rec := auditReq(h, "/?tenant_id=t1&from="+now+"&to="+now+"&limit=10"); rec.Code != http.StatusOK {
		t.Fatalf("valid params code = %d", rec.Code)
	}
	for _, bad := range []string{"/?from=nope", "/?to=nope", "/?limit=-1", "/?limit=abc"} {
		if rec := auditReq(h, bad); rec.Code != http.StatusBadRequest {
			t.Fatalf("%s code = %d, want 400", bad, rec.Code)
		}
	}
}

type errStore struct{ Store }

func (errStore) Query(context.Context, Filter) ([]Event, error) { return nil, errors.New("db down") }

func TestServiceQueryError(t *testing.T) {
	t.Parallel()
	s := New(errStore{Store: NewMemoryStore()})
	if _, err := s.Query(context.Background(), Filter{}); err == nil {
		t.Fatal("expected query error")
	}
	rec := auditReq(s.Routes(), "/")
	if rec.Code != http.StatusInternalServerError {
		t.Fatalf("query error code = %d", rec.Code)
	}
}

func TestRecordPersistError(t *testing.T) {
	t.Parallel()
	s := New(appendErrStore{NewMemoryStore()})
	if _, err := s.Record(context.Background(), Event{TenantID: "t", Action: "a", Actor: "x"}); err == nil {
		t.Fatal("expected persist error")
	}
}

type appendErrStore struct{ Store }

func (appendErrStore) Append(context.Context, Event) error { return errors.New("write failed") }
