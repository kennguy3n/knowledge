package middleware

import (
	"context"
	"net/http"
	"net/http/httptest"
	"testing"
)

func TestRequireService(t *testing.T) {
	t.Parallel()
	ok := http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) { w.WriteHeader(http.StatusOK) })
	guard := RequireService(ok)

	cases := []struct {
		name string
		ctx  context.Context
		want int
	}{
		{
			name: "service principal admitted",
			ctx:  withPrincipal(context.Background(), Principal{Subject: "service", Service: true}),
			want: http.StatusOK,
		},
		{
			name: "tenant-user principal forbidden",
			ctx:  withPrincipal(context.Background(), Principal{Subject: "u1", TenantID: "t1"}),
			want: http.StatusForbidden,
		},
		{
			name: "no principal forbidden",
			ctx:  context.Background(),
			want: http.StatusForbidden,
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			r := httptest.NewRequest(http.MethodGet, "/", nil).WithContext(tc.ctx)
			rec := httptest.NewRecorder()
			guard.ServeHTTP(rec, r)
			if rec.Code != tc.want {
				t.Fatalf("code = %d, want %d (body=%s)", rec.Code, tc.want, rec.Body.String())
			}
		})
	}
}
