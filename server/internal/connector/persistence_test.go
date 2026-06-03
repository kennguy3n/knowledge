package connector

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"
)

// memRegStore is an in-process [RegistrationStore] used to assert
// write-through and rehydration behaviour. saveErr/deleteErr, when set,
// force the corresponding operation to fail.
type memRegStore struct {
	mu        sync.Mutex
	regs      map[string]registration
	saveErr   error
	deleteErr error
}

func newMemRegStore() *memRegStore {
	return &memRegStore{regs: make(map[string]registration)}
}

func (m *memRegStore) Save(_ context.Context, r registration) error {
	if m.saveErr != nil {
		return m.saveErr
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	m.regs[r.InstanceID] = r
	return nil
}

func (m *memRegStore) Delete(_ context.Context, id string) error {
	if m.deleteErr != nil {
		return m.deleteErr
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	delete(m.regs, id)
	return nil
}

func (m *memRegStore) List(_ context.Context) ([]registration, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	out := make([]registration, 0, len(m.regs))
	for _, r := range m.regs {
		out = append(out, r)
	}
	return out, nil
}

func (m *memRegStore) has(id string) bool {
	m.mu.Lock()
	defer m.mu.Unlock()
	_, ok := m.regs[id]
	return ok
}

func svcWithRegs(sub substrateAPI, regs RegistrationStore) *Service {
	return New(sub, nil, Options{
		PublicBaseURL:     "https://api.example.com",
		SyncInterval:      time.Minute,
		RegistrationStore: regs,
	})
}

func TestCreatePersistsRegistration(t *testing.T) {
	t.Parallel()
	regs := newMemRegStore()
	s := svcWithRegs(&fakeSub{createID: "inst-1"}, regs)
	h := s.Routes()

	body := `{"kind":"GoogleDrive","scope_id":"` + scopeUUID + `"}`
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodPost, "/", strings.NewReader(body)))
	if rec.Code != http.StatusCreated {
		t.Fatalf("create code = %d body=%s", rec.Code, rec.Body.String())
	}
	if !regs.has("inst-1") {
		t.Fatal("registration was not persisted on create")
	}
	if _, ok := s.store.get("inst-1"); !ok {
		t.Fatal("registration missing from in-memory cache after create")
	}
}

func TestCreateFailsWhenPersistenceFails(t *testing.T) {
	t.Parallel()
	regs := newMemRegStore()
	regs.saveErr = errors.New("db down")
	s := svcWithRegs(&fakeSub{createID: "inst-1"}, regs)
	h := s.Routes()

	body := `{"kind":"GoogleDrive","scope_id":"` + scopeUUID + `"}`
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodPost, "/", strings.NewReader(body)))
	if rec.Code != http.StatusInternalServerError {
		t.Fatalf("expected 500 when persistence fails, got %d", rec.Code)
	}
	// A connector whose registration could not be durably stored must not
	// linger in the cache where it would be served then lost on restart.
	if _, ok := s.store.get("inst-1"); ok {
		t.Fatal("registration cached despite persistence failure")
	}
}

func TestRemoveDeletesPersistedRegistration(t *testing.T) {
	t.Parallel()
	regs := newMemRegStore()
	s := svcWithRegs(&fakeSub{}, regs)
	if err := s.saveRegistration(context.Background(),
		registration{InstanceID: "inst-1", Kind: "GoogleDrive", ScopeID: scopeUUID}); err != nil {
		t.Fatal(err)
	}
	h := s.Routes()
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodDelete, "/inst-1", nil))
	if rec.Code != http.StatusNoContent {
		t.Fatalf("remove code = %d", rec.Code)
	}
	if regs.has("inst-1") {
		t.Fatal("registration still persisted after remove")
	}
}

func TestWebhookRegisterPersists(t *testing.T) {
	t.Parallel()
	regs := newMemRegStore()
	s := svcWithRegs(&fakeSub{}, regs)
	if err := s.saveRegistration(context.Background(),
		registration{InstanceID: "inst-1", Kind: "GoogleDrive", ScopeID: scopeUUID}); err != nil {
		t.Fatal(err)
	}
	h := s.Routes()
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodPost, "/inst-1/webhook/register", nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("register code = %d", rec.Code)
	}
	got, _ := regs.List(context.Background())
	if len(got) != 1 || !got[0].WebhookActive || got[0].WebhookURL == "" {
		t.Fatalf("webhook activation not persisted: %+v", got)
	}
}

func TestRehydrateRestoresSchedules(t *testing.T) {
	t.Parallel()
	regs := newMemRegStore()
	for _, id := range []string{"inst-1", "inst-2"} {
		regs.regs[id] = registration{
			InstanceID: id, Kind: "GoogleDrive", ScopeID: scopeUUID,
			SyncInterval: time.Minute, CreatedAt: time.Now().UTC(),
		}
	}
	// Substrate reports both connectors as live.
	live, _ := json.Marshal([]map[string]string{
		{"instanceId": "inst-1"}, {"instanceId": "inst-2"},
	})
	s := svcWithRegs(&fakeSub{listRaw: live}, regs)

	if err := s.Rehydrate(context.Background()); err != nil {
		t.Fatal(err)
	}
	if s.sched.Count() != 2 {
		t.Fatalf("expected 2 rescheduled jobs, got %d", s.sched.Count())
	}
	if _, ok := s.store.get("inst-1"); !ok {
		t.Fatal("inst-1 not restored to cache")
	}
}

func TestRehydratePrunesStaleRegistrations(t *testing.T) {
	t.Parallel()
	regs := newMemRegStore()
	regs.regs["live"] = registration{InstanceID: "live", Kind: "GoogleDrive", ScopeID: scopeUUID, SyncInterval: time.Minute}
	regs.regs["gone"] = registration{InstanceID: "gone", Kind: "GoogleDrive", ScopeID: scopeUUID, SyncInterval: time.Minute}
	// Substrate only knows about "live"; "gone" was deleted while down.
	live, _ := json.Marshal([]map[string]string{{"instanceId": "live"}})
	s := svcWithRegs(&fakeSub{listRaw: live}, regs)

	if err := s.Rehydrate(context.Background()); err != nil {
		t.Fatal(err)
	}
	if s.sched.Count() != 1 {
		t.Fatalf("expected only the live connector scheduled, got %d", s.sched.Count())
	}
	if regs.has("gone") {
		t.Fatal("stale registration was not pruned from durable store")
	}
	if _, ok := s.store.get("gone"); ok {
		t.Fatal("stale registration leaked into cache")
	}
}

func TestRehydrateKeepsAllWhenSubstrateUnavailable(t *testing.T) {
	t.Parallel()
	regs := newMemRegStore()
	regs.regs["a"] = registration{InstanceID: "a", Kind: "GoogleDrive", ScopeID: scopeUUID, SyncInterval: time.Minute}
	regs.regs["b"] = registration{InstanceID: "b", Kind: "GoogleDrive", ScopeID: scopeUUID, SyncInterval: time.Minute}
	// Substrate loopback is down: reconciliation must be skipped, not
	// treated as "no connectors exist" (which would drop every schedule).
	s := svcWithRegs(&fakeSub{listErr: errors.New("loopback down")}, regs)

	if err := s.Rehydrate(context.Background()); err != nil {
		t.Fatal(err)
	}
	if s.sched.Count() != 2 {
		t.Fatalf("expected both schedules retained, got %d", s.sched.Count())
	}
	if !regs.has("a") || !regs.has("b") {
		t.Fatal("registrations must not be pruned when substrate is unavailable")
	}
}

func TestRehydrateDefaultsZeroInterval(t *testing.T) {
	t.Parallel()
	regs := newMemRegStore()
	// A registration persisted without a sync interval (e.g. legacy row)
	// must fall back to the service default rather than scheduling at 0.
	regs.regs["x"] = registration{InstanceID: "x", Kind: "GoogleDrive", ScopeID: scopeUUID}
	live, _ := json.Marshal([]map[string]string{{"instanceId": "x"}})
	s := svcWithRegs(&fakeSub{listRaw: live}, regs)

	if err := s.Rehydrate(context.Background()); err != nil {
		t.Fatal(err)
	}
	if s.sched.Count() != 1 {
		t.Fatalf("expected 1 scheduled job, got %d", s.sched.Count())
	}
}

// Ensure the noop store satisfies the interface and is inert.
func TestNoopRegistrationStore(t *testing.T) {
	t.Parallel()
	rs := NewNoopRegistrationStore()
	if err := rs.Save(context.Background(), registration{InstanceID: "x"}); err != nil {
		t.Fatal(err)
	}
	if err := rs.Delete(context.Background(), "x"); err != nil {
		t.Fatal(err)
	}
	got, err := rs.List(context.Background())
	if err != nil || got != nil {
		t.Fatalf("noop list = %v, %v", got, err)
	}
}
