package gateway

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/golang-jwt/jwt/v5"

	"github.com/kennguy3n/knowledge/server/internal/middleware"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
)

// blockingSub embeds fakeSub but blocks inside TriggerSynthesis until
// released, so a test can hold synthesis slots open and observe the
// fair-share admission decisions made by concurrent callers.
type blockingSub struct {
	*fakeSub
	entered chan string   // tenant/marker signalled on entry
	release chan struct{} // closed to let all in-flight calls return
}

func (b *blockingSub) TriggerSynthesis(ctx context.Context, _ substrate.SynthesisTriggerRequest) (json.RawMessage, error) {
	b.entered <- "in"
	select {
	case <-b.release:
	case <-ctx.Done():
		return nil, ctx.Err()
	}
	return json.RawMessage(`{"id":"syn-1"}`), nil
}

func tenantJWT(t *testing.T, secret, tenantID string) string {
	t.Helper()
	tok := jwt.NewWithClaims(jwt.SigningMethodHS256, jwt.MapClaims{
		"sub": "user-" + tenantID, "tenant_id": tenantID,
	})
	s, err := tok.SignedString([]byte(secret))
	if err != nil {
		t.Fatal(err)
	}
	return s
}

func triggerReq(t *testing.T, h http.Handler, token string) *httptest.ResponseRecorder {
	t.Helper()
	r := httptest.NewRequest(http.MethodPost, "/api/v1/synthesis/trigger",
		strings.NewReader(`{"scope_id":"`+scopeUUID+`"}`))
	r.Header.Set("Authorization", "Bearer "+token)
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, r)
	return rec
}

// TestSynthesisFairShareThrottlesNoisyTenant verifies, through the full
// gateway stack, that a tenant at its synthesis cap is shed with 429 +
// Retry-After while a different tenant proceeds — the 5k-tenant
// noisy-neighbour protection.
func TestSynthesisFairShareThrottlesNoisyTenant(t *testing.T) {
	// Tighten the package-level controller to cap=1/queue=0 for the test,
	// then restore it. Not parallel: mutates the shared singleton.
	prev := synthesisFairShare
	synthesisFairShare = middleware.NewSynthesisFairShare(middleware.FairShareConfig{
		TenantConcurrency: 1,
		TenantQueue:       0,
		GlobalConcurrency: 10,
		QueueWait:         100 * time.Millisecond,
	})
	t.Cleanup(func() {
		synthesisFairShare.Stop()
		synthesisFairShare = prev
	})

	const secret = "fairshare-test-secret"
	sub := &blockingSub{fakeSub: &fakeSub{}, entered: make(chan string, 4), release: make(chan struct{})}
	h := NewRouter(Deps{Substrate: sub, Auth: middleware.NewAuthenticator("", secret)})

	tokA := tenantJWT(t, secret, "tenant-a")
	tokB := tenantJWT(t, secret, "tenant-b")

	// A1 enters synthesis and holds tenant-a's only slot.
	a1 := make(chan int, 1)
	go func() { a1 <- triggerReq(t, h, tokA).Code }()
	select {
	case <-sub.entered:
	case <-time.After(2 * time.Second):
		t.Fatal("A1 never reached substrate")
	}

	// A2 (same tenant, at cap, no queue) must be shed immediately.
	recA2 := triggerReq(t, h, tokA)
	if recA2.Code != http.StatusTooManyRequests {
		t.Fatalf("A2 code = %d, want 429", recA2.Code)
	}
	if recA2.Header().Get("Retry-After") == "" {
		t.Fatal("A2 missing Retry-After header")
	}

	// B1 (different tenant) is unaffected and reaches synthesis.
	b1 := make(chan int, 1)
	go func() { b1 <- triggerReq(t, h, tokB).Code }()
	select {
	case <-sub.entered:
	case <-time.After(2 * time.Second):
		t.Fatal("B1 starved by tenant-a's load")
	}

	// Release in-flight syntheses; both held requests complete 202.
	close(sub.release)
	if code := <-a1; code != http.StatusAccepted {
		t.Fatalf("A1 code = %d, want 202", code)
	}
	if code := <-b1; code != http.StatusAccepted {
		t.Fatalf("B1 code = %d, want 202", code)
	}
}
