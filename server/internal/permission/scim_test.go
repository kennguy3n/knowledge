package permission

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func scimDo(t *testing.T, h http.Handler, method, path, body string) *httptest.ResponseRecorder {
	t.Helper()
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

func TestSCIMUserLifecycle(t *testing.T) {
	t.Parallel()
	h := New(&fakeChecker{}).SCIMRoutes()

	// Create.
	rec := scimDo(t, h, http.MethodPost, "/Users", `{"userName":"alice@example.com","emails":[{"value":"alice@example.com","primary":true}]}`)
	if rec.Code != http.StatusCreated {
		t.Fatalf("create code = %d body=%s", rec.Code, rec.Body.String())
	}
	var u User
	if err := json.Unmarshal(rec.Body.Bytes(), &u); err != nil {
		t.Fatal(err)
	}
	if u.ID == "" || !u.Active {
		t.Fatalf("bad created user: %+v", u)
	}

	// Duplicate userName → 409.
	rec = scimDo(t, h, http.MethodPost, "/Users", `{"userName":"alice@example.com"}`)
	if rec.Code != http.StatusConflict {
		t.Fatalf("duplicate code = %d", rec.Code)
	}

	// Get.
	rec = scimDo(t, h, http.MethodGet, "/Users/"+u.ID, "")
	if rec.Code != http.StatusOK {
		t.Fatalf("get code = %d", rec.Code)
	}

	// List.
	rec = scimDo(t, h, http.MethodGet, "/Users", "")
	if rec.Code != http.StatusOK || !strings.Contains(rec.Body.String(), "totalResults") {
		t.Fatalf("list code = %d body=%s", rec.Code, rec.Body.String())
	}

	// Replace.
	rec = scimDo(t, h, http.MethodPut, "/Users/"+u.ID, `{"userName":"alice2@example.com","active":false}`)
	if rec.Code != http.StatusOK {
		t.Fatalf("put code = %d", rec.Code)
	}

	// Delete.
	rec = scimDo(t, h, http.MethodDelete, "/Users/"+u.ID, "")
	if rec.Code != http.StatusNoContent {
		t.Fatalf("delete code = %d", rec.Code)
	}
	rec = scimDo(t, h, http.MethodGet, "/Users/"+u.ID, "")
	if rec.Code != http.StatusNotFound {
		t.Fatalf("get after delete = %d", rec.Code)
	}
}

func TestSCIMUserValidation(t *testing.T) {
	t.Parallel()
	h := New(&fakeChecker{}).SCIMRoutes()
	rec := scimDo(t, h, http.MethodPost, "/Users", `{}`)
	if rec.Code != http.StatusBadRequest {
		t.Fatalf("missing userName code = %d", rec.Code)
	}
}

func TestSCIMGroupLifecycle(t *testing.T) {
	t.Parallel()
	h := New(&fakeChecker{}).SCIMRoutes()
	rec := scimDo(t, h, http.MethodPost, "/Groups", `{"displayName":"engineers","members":[{"value":"u1"}]}`)
	if rec.Code != http.StatusCreated {
		t.Fatalf("create group code = %d body=%s", rec.Code, rec.Body.String())
	}
	var g Group
	if err := json.Unmarshal(rec.Body.Bytes(), &g); err != nil {
		t.Fatal(err)
	}
	if g.ID == "" || g.DisplayName != "engineers" {
		t.Fatalf("bad group: %+v", g)
	}
	rec = scimDo(t, h, http.MethodGet, "/Groups/"+g.ID, "")
	if rec.Code != http.StatusOK {
		t.Fatalf("get group = %d", rec.Code)
	}
	rec = scimDo(t, h, http.MethodDelete, "/Groups/"+g.ID, "")
	if rec.Code != http.StatusNoContent {
		t.Fatalf("delete group = %d", rec.Code)
	}
}
