package connector

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

func TestHandleListAuthSyncStatus(t *testing.T) {
	t.Parallel()
	report := `{"instanceId":"inst-1","mode":"incremental","ingestedEvidenceIds":[]}`
	sub := &fakeSub{syncRaw: json.RawMessage(report)}
	s := newSvc(sub)
	s.store.put(registration{InstanceID: "inst-1", Kind: "google_drive", ScopeID: scopeUUID})
	h := s.Routes()

	if rec := req(h, http.MethodGet, "/", ""); rec.Code != http.StatusOK {
		t.Fatalf("list code = %d", rec.Code)
	}
	if rec := req(h, http.MethodPost, "/inst-1/authenticate", `{"auth_code":"abc"}`); rec.Code != http.StatusOK {
		t.Fatalf("authenticate code = %d", rec.Code)
	}
	if rec := req(h, http.MethodPost, "/inst-1/sync", ""); rec.Code != http.StatusOK {
		t.Fatalf("sync code = %d body=%s", rec.Code, rec.Body.String())
	}
	if rec := req(h, http.MethodGet, "/inst-1/status", ""); rec.Code != http.StatusOK {
		t.Fatalf("status code = %d", rec.Code)
	}
}

func TestOAuthStartMissingParams(t *testing.T) {
	t.Parallel()
	s := newSvc(&fakeSub{})
	s.store.put(registration{InstanceID: "inst-1", Kind: "google_drive", ScopeID: scopeUUID})
	h := s.Routes()
	if rec := req(h, http.MethodGet, "/inst-1/oauth/start", ""); rec.Code != http.StatusBadRequest {
		t.Fatalf("missing params code = %d", rec.Code)
	}
	if rec := req(h, http.MethodGet, "/missing/oauth/start?client_id=c&redirect_uri=u", ""); rec.Code != http.StatusNotFound {
		t.Fatalf("missing connector code = %d", rec.Code)
	}
}

func TestOAuthCallbackMissingParams(t *testing.T) {
	t.Parallel()
	s := newSvc(&fakeSub{})
	h := s.Routes()
	if rec := req(h, http.MethodGet, "/oauth/callback", ""); rec.Code != http.StatusBadRequest {
		t.Fatalf("missing code/state = %d", rec.Code)
	}
	if rec := req(h, http.MethodGet, "/oauth/callback?code=c&state=unknown", ""); rec.Code != http.StatusBadRequest {
		t.Fatalf("unknown state = %d", rec.Code)
	}
}

func TestStoreList(t *testing.T) {
	t.Parallel()
	st := newStore()
	st.put(registration{InstanceID: "a"})
	st.put(registration{InstanceID: "b"})
	if got := st.list(); len(got) != 2 {
		t.Fatalf("list len = %d", len(got))
	}
}

func TestSchedulerStartScheduleStop(t *testing.T) {
	t.Parallel()
	done := make(chan struct{}, 1)
	sched := NewScheduler(func(context.Context, string) {
		select {
		case done <- struct{}{}:
		default:
		}
	})
	sched.Start(context.Background())
	sched.Schedule("inst-1", 10*time.Millisecond)
	sched.Schedule("inst-1", 10*time.Millisecond) // reschedule replaces
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("scheduled job never fired")
	}
	if sched.Count() != 1 {
		t.Fatalf("count = %d", sched.Count())
	}
	sched.Schedule("inst-2", 0) // ignored
	if sched.Count() != 1 {
		t.Fatalf("zero interval should be ignored, count = %d", sched.Count())
	}
	sched.Stop()
	if sched.Count() != 0 {
		t.Fatalf("after stop count = %d", sched.Count())
	}
}

// req is a local helper mirroring the gateway/tenant test helpers.
func req(h http.Handler, method, path, body string) *httptest.ResponseRecorder {
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
