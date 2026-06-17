package tenant

import (
	"net/http"
	"net/http/httptest"
	"testing"
)

// tagGuard returns a middleware that short-circuits with the given
// status code without invoking next. A route's response code therefore
// identifies which Authz slot chi applied to it, so the wiring is
// asserted without touching the (nil-backed) handlers.
func tagGuard(code int) func(http.Handler) http.Handler {
	return func(http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) { w.WriteHeader(code) })
	}
}

func TestRoutesAppliesPerRouteAuthz(t *testing.T) {
	t.Parallel()
	const (
		svcCode    = 521
		adminCode  = 522
		viewerCode = 523
	)
	h := (&Service{}).Routes(Authz{
		Service: tagGuard(svcCode),
		Admin:   tagGuard(adminCode),
		Viewer:  tagGuard(viewerCode),
	})

	const (
		id  = "11111111-1111-1111-1111-111111111111"
		uid = "22222222-2222-2222-2222-222222222222"
	)
	cases := []struct {
		method string
		path   string
		want   int
	}{
		{http.MethodPost, "/", svcCode},
		{http.MethodGet, "/", svcCode},
		{http.MethodDelete, "/" + id, svcCode},
		{http.MethodGet, "/" + id, viewerCode},
		{http.MethodGet, "/" + id + "/members", viewerCode},
		{http.MethodPut, "/" + id + "/config", adminCode},
		{http.MethodPost, "/" + id + "/key/rotate", adminCode},
		{http.MethodPost, "/" + id + "/members", adminCode},
		{http.MethodPost, "/" + id + "/members/" + uid + "/activate", adminCode},
		{http.MethodPost, "/" + id + "/members/" + uid + "/suspend", adminCode},
		{http.MethodDelete, "/" + id + "/members/" + uid, adminCode},
	}
	for _, tc := range cases {
		rec := httptest.NewRecorder()
		h.ServeHTTP(rec, httptest.NewRequest(tc.method, tc.path, nil))
		if rec.Code != tc.want {
			t.Fatalf("%s %s: guard code = %d, want %d", tc.method, tc.path, rec.Code, tc.want)
		}
	}
}
