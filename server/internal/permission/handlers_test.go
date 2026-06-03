package permission

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func reqRec(h http.Handler, method, path, body string) *httptest.ResponseRecorder {
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

func tuple() string {
	return `{"object":{"object_type":"tenant","object_id":"` + objUUID +
		`"},"relation":"owner","subject":{"subject_type":"user","subject_id":"` + subUUID + `"}}`
}

func TestHandleRevokeAndCheck(t *testing.T) {
	t.Parallel()
	h := New(&fakeChecker{allow: true}).Routes()

	rec := reqRec(h, http.MethodPost, "/revoke", tuple())
	if rec.Code != http.StatusOK {
		t.Fatalf("revoke code = %d body=%s", rec.Code, rec.Body.String())
	}
	rec = reqRec(h, http.MethodPost, "/check", tuple())
	if rec.Code != http.StatusOK || !strings.Contains(rec.Body.String(), `"allowed":true`) {
		t.Fatalf("check code = %d body=%s", rec.Code, rec.Body.String())
	}
}

func TestHandleInvalidJSON(t *testing.T) {
	t.Parallel()
	h := New(&fakeChecker{}).Routes()
	rec := reqRec(h, http.MethodPost, "/grant", `{not json`)
	if rec.Code != http.StatusBadRequest {
		t.Fatalf("expected 400, got %d", rec.Code)
	}
}

func TestSCIMGroupReplaceAndList(t *testing.T) {
	t.Parallel()
	h := New(&fakeChecker{}).SCIMRoutes()

	rec := reqRec(h, http.MethodPost, "/Groups", `{"displayName":"team"}`)
	var g Group
	if err := json.Unmarshal(rec.Body.Bytes(), &g); err != nil {
		t.Fatal(err)
	}

	rec = reqRec(h, http.MethodPut, "/Groups/"+g.ID, `{"displayName":"team2","members":[{"value":"u9"}]}`)
	if rec.Code != http.StatusOK || !strings.Contains(rec.Body.String(), "team2") {
		t.Fatalf("replace code = %d body=%s", rec.Code, rec.Body.String())
	}
	rec = reqRec(h, http.MethodGet, "/Groups", "")
	if rec.Code != http.StatusOK || !strings.Contains(rec.Body.String(), "totalResults") {
		t.Fatalf("list groups code = %d", rec.Code)
	}

	// Replace / delete missing group → 404.
	rec = reqRec(h, http.MethodPut, "/Groups/missing", `{"displayName":"x"}`)
	if rec.Code != http.StatusNotFound {
		t.Fatalf("replace missing code = %d", rec.Code)
	}
	rec = reqRec(h, http.MethodDelete, "/Groups/missing", "")
	if rec.Code != http.StatusNotFound {
		t.Fatalf("delete missing code = %d", rec.Code)
	}
}

func TestSCIMUserNotFoundPaths(t *testing.T) {
	t.Parallel()
	h := New(&fakeChecker{}).SCIMRoutes()
	if rec := reqRec(h, http.MethodGet, "/Users/missing", ""); rec.Code != http.StatusNotFound {
		t.Fatalf("get missing = %d", rec.Code)
	}
	if rec := reqRec(h, http.MethodPut, "/Users/missing", `{"userName":"x"}`); rec.Code != http.StatusNotFound {
		t.Fatalf("put missing = %d", rec.Code)
	}
	if rec := reqRec(h, http.MethodDelete, "/Users/missing", ""); rec.Code != http.StatusNotFound {
		t.Fatalf("delete missing = %d", rec.Code)
	}
	if rec := reqRec(h, http.MethodGet, "/Groups/missing", ""); rec.Code != http.StatusNotFound {
		t.Fatalf("get missing group = %d", rec.Code)
	}
}
