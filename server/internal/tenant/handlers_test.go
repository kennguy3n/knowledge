package tenant

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/google/uuid"
)

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

func createTenant(t *testing.T, h http.Handler) Tenant {
	t.Helper()
	rec := req(h, http.MethodPost, "/", `{"name":"Acme"}`)
	if rec.Code != http.StatusCreated {
		t.Fatalf("create code = %d body=%s", rec.Code, rec.Body.String())
	}
	var tn Tenant
	if err := json.Unmarshal(rec.Body.Bytes(), &tn); err != nil {
		t.Fatal(err)
	}
	return tn
}

func TestHandlerListUpdateConfigDelete(t *testing.T) {
	t.Parallel()
	s, _ := newService()
	h := s.Routes(Authz{})
	tn := createTenant(t, h)

	if rec := req(h, http.MethodGet, "/", ""); rec.Code != http.StatusOK {
		t.Fatalf("list code = %d", rec.Code)
	}

	// Update config (valid + invalid tier).
	rec := req(h, http.MethodPut, "/"+tn.ID+"/config", `{"connector_limit":5,"synthesis_tier":"premium","retention_days":30}`)
	if rec.Code != http.StatusOK {
		t.Fatalf("update config code = %d body=%s", rec.Code, rec.Body.String())
	}
	rec = req(h, http.MethodPut, "/"+tn.ID+"/config", `{"synthesis_tier":"nope"}`)
	if rec.Code != http.StatusBadRequest {
		t.Fatalf("invalid tier code = %d", rec.Code)
	}

	// Rotate key.
	if rec := req(h, http.MethodPost, "/"+tn.ID+"/key/rotate", ""); rec.Code != http.StatusOK {
		t.Fatalf("rotate code = %d", rec.Code)
	}

	// Delete + invalid id.
	if rec := req(h, http.MethodDelete, "/"+tn.ID, ""); rec.Code != http.StatusNoContent {
		t.Fatalf("delete code = %d", rec.Code)
	}
	if rec := req(h, http.MethodDelete, "/not-a-uuid", ""); rec.Code != http.StatusBadRequest {
		t.Fatalf("delete bad id code = %d", rec.Code)
	}
}

// TestUpdateConfigPreservesQuotaWhenOmitted guards the fix for a PUT
// that omits the quota object silently resetting a custom override to
// defaults. An unrelated config edit (retention_days) must leave a
// previously-set quota untouched.
func TestUpdateConfigPreservesQuotaWhenOmitted(t *testing.T) {
	t.Parallel()
	s, _ := newService()
	h := s.Routes(Authz{})
	tn := createTenant(t, h)

	// Set a custom (non-default) quota.
	rec := req(h, http.MethodPut, "/"+tn.ID+"/config",
		`{"connector_limit":10,"synthesis_tier":"standard","retention_days":365,`+
			`"quota":{"requests_per_min":42,"syntheses_per_day":7,"storage_soft_cap_bytes":123456}}`)
	if rec.Code != http.StatusOK {
		t.Fatalf("set quota code = %d body=%s", rec.Code, rec.Body.String())
	}

	// Edit an unrelated field with no quota object in the body.
	rec = req(h, http.MethodPut, "/"+tn.ID+"/config",
		`{"connector_limit":10,"synthesis_tier":"standard","retention_days":30}`)
	if rec.Code != http.StatusOK {
		t.Fatalf("update code = %d body=%s", rec.Code, rec.Body.String())
	}
	var got Tenant
	if err := json.Unmarshal(rec.Body.Bytes(), &got); err != nil {
		t.Fatal(err)
	}
	want := Quota{RequestsPerMin: 42, SynthesesPerDay: 7, StorageSoftCapBytes: 123456}
	if got.Config.Quota != want {
		t.Fatalf("quota not preserved: got %+v want %+v", got.Config.Quota, want)
	}
	if got.Config.RetentionDays != 30 {
		t.Fatalf("retention not updated: got %d", got.Config.RetentionDays)
	}
}

// TestUpdateConfigFiresConfigChangeHook verifies the hook used to bust a
// quota cache is invoked with the tenant ID on a successful config
// update (and not on a rejected one).
func TestUpdateConfigFiresConfigChangeHook(t *testing.T) {
	t.Parallel()
	s, _ := newService()
	var fired []string
	s.SetConfigChangeHook(func(id string) { fired = append(fired, id) })
	h := s.Routes(Authz{})
	tn := createTenant(t, h)

	if rec := req(h, http.MethodPut, "/"+tn.ID+"/config",
		`{"connector_limit":5,"synthesis_tier":"standard","retention_days":30}`); rec.Code != http.StatusOK {
		t.Fatalf("update code = %d body=%s", rec.Code, rec.Body.String())
	}
	if rec := req(h, http.MethodPut, "/"+tn.ID+"/config", `{"synthesis_tier":"nope"}`); rec.Code != http.StatusBadRequest {
		t.Fatalf("invalid update code = %d", rec.Code)
	}
	if len(fired) != 1 || fired[0] != tn.ID {
		t.Fatalf("hook fired = %v, want exactly [%s]", fired, tn.ID)
	}
}

func TestHandlerMemberLifecycle(t *testing.T) {
	t.Parallel()
	s, _ := newService()
	h := s.Routes(Authz{})
	tn := createTenant(t, h)
	uid := uuid.NewString()

	// Invite.
	rec := req(h, http.MethodPost, "/"+tn.ID+"/members", `{"user_id":"`+uid+`","email":"u@x.io"}`)
	if rec.Code != http.StatusCreated {
		t.Fatalf("invite code = %d body=%s", rec.Code, rec.Body.String())
	}

	// List members.
	if rec := req(h, http.MethodGet, "/"+tn.ID+"/members", ""); rec.Code != http.StatusOK {
		t.Fatalf("list members code = %d", rec.Code)
	}

	// Activate, suspend.
	if rec := req(h, http.MethodPost, "/"+tn.ID+"/members/"+uid+"/activate", ""); rec.Code != http.StatusOK {
		t.Fatalf("activate code = %d", rec.Code)
	}
	if rec := req(h, http.MethodPost, "/"+tn.ID+"/members/"+uid+"/suspend", ""); rec.Code != http.StatusOK {
		t.Fatalf("suspend code = %d", rec.Code)
	}

	// Remove.
	if rec := req(h, http.MethodDelete, "/"+tn.ID+"/members/"+uid, ""); rec.Code != http.StatusNoContent {
		t.Fatalf("remove code = %d", rec.Code)
	}

	// Transition a missing member → 404.
	if rec := req(h, http.MethodPost, "/"+tn.ID+"/members/"+uuid.NewString()+"/activate", ""); rec.Code != http.StatusNotFound {
		t.Fatalf("activate missing code = %d", rec.Code)
	}
}
