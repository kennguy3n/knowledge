package gateway

import (
	"net/http"
	"net/http/httptest"
	"sort"
	"strings"
	"testing"
	"time"
)

// BenchmarkGatewayIngest measures end-to-end gateway throughput for the
// hot ingest path (router + middleware + handler) against an in-memory
// substrate fake. It reports requests/sec plus p50/p99 latency so the
// gateway's overhead can be tracked over time.
func BenchmarkGatewayIngest(b *testing.B) {
	h := NewRouter(Deps{Substrate: &fakeSub{}})
	body := `{"scope_id":"` + scopeUUID + `","body":"hello world","source":"bench","importance":"Useful"}`

	lat := make([]time.Duration, 0, b.N)
	b.ReportAllocs()
	b.ResetTimer()
	start := time.Now()
	for i := 0; i < b.N; i++ {
		t0 := time.Now()
		r := httptest.NewRequest(http.MethodPost, "/api/v1/ingest", strings.NewReader(body))
		rec := httptest.NewRecorder()
		h.ServeHTTP(rec, r)
		lat = append(lat, time.Since(t0))
		if rec.Code != http.StatusOK && rec.Code != http.StatusCreated {
			b.Fatalf("unexpected status %d: %s", rec.Code, rec.Body.String())
		}
	}
	b.StopTimer()

	elapsed := time.Since(start)
	if rps := float64(b.N) / elapsed.Seconds(); rps > 0 {
		b.ReportMetric(rps, "req/sec")
	}
	sort.Slice(lat, func(i, j int) bool { return lat[i] < lat[j] })
	if len(lat) > 0 {
		p50 := lat[len(lat)*50/100]
		p99 := lat[min(len(lat)*99/100, len(lat)-1)]
		b.ReportMetric(float64(p50.Microseconds()), "p50-us")
		b.ReportMetric(float64(p99.Microseconds()), "p99-us")
	}
}
