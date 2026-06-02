package middleware

import (
	"net/http"
	"net/http/httptest"
	"testing"
)

func reqWithXFF(remoteAddr, xff string) *http.Request {
	r := httptest.NewRequest(http.MethodGet, "/", nil)
	r.RemoteAddr = remoteAddr
	if xff != "" {
		r.Header.Set("X-Forwarded-For", xff)
	}
	return r
}

func TestProxyTrustClientIP(t *testing.T) {
	t.Parallel()
	cases := []struct {
		name    string
		trusted []string
		remote  string
		xff     string
		want    string
	}{
		{
			name:   "nil-config ignores XFF and uses peer",
			remote: "203.0.113.7:443",
			xff:    "1.2.3.4",
			want:   "203.0.113.7",
		},
		{
			name:    "untrusted peer cannot spoof via XFF",
			trusted: []string{"10.0.0.0/8"},
			remote:  "203.0.113.7:443", // not in 10/8
			xff:     "1.2.3.4",
			want:    "203.0.113.7",
		},
		{
			name:    "trusted peer: first untrusted XFF hop wins",
			trusted: []string{"10.0.0.0/8"},
			remote:  "10.0.0.1:443",
			xff:     "198.51.100.9",
			want:    "198.51.100.9",
		},
		{
			name:    "trusted peer + trusted XFF hops: walk right-to-left to real client",
			trusted: []string{"10.0.0.0/8"},
			remote:  "10.0.0.1:443",
			xff:     "198.51.100.9, 10.0.0.5, 10.0.0.6",
			want:    "198.51.100.9",
		},
		{
			name:    "trusted peer, no XFF: fall back to peer",
			trusted: []string{"10.0.0.0/8"},
			remote:  "10.0.0.1:443",
			xff:     "",
			want:    "10.0.0.1",
		},
		{
			name:    "trusted peer, all XFF hops trusted: fall back to peer",
			trusted: []string{"10.0.0.0/8"},
			remote:  "10.0.0.1:443",
			xff:     "10.0.0.5, 10.0.0.6",
			want:    "10.0.0.1",
		},
		{
			name:    "bare IP trust entry",
			trusted: []string{"10.0.0.1"},
			remote:  "10.0.0.1:443",
			xff:     "198.51.100.9",
			want:    "198.51.100.9",
		},
		{
			name:    "spoofed XFF with unique value per request is ignored (peer keyed)",
			trusted: []string{"10.0.0.0/8"},
			remote:  "203.0.113.50:1234",
			xff:     "9.9.9.9",
			want:    "203.0.113.50",
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			t.Parallel()
			var pt *ProxyTrust
			if c.trusted != nil {
				pt = NewProxyTrust(c.trusted)
			}
			if got := pt.ClientIP(reqWithXFF(c.remote, c.xff)); got != c.want {
				t.Fatalf("ClientIP = %q, want %q", got, c.want)
			}
		})
	}
}

func TestProxyTrustSkipsMalformedEntries(t *testing.T) {
	t.Parallel()
	// "not-an-ip" is dropped; the valid CIDR still applies.
	pt := NewProxyTrust([]string{"not-an-ip", "192.168.0.0/16", ""})
	got := pt.ClientIP(reqWithXFF("192.168.1.1:80", "198.51.100.1"))
	if got != "198.51.100.1" {
		t.Fatalf("ClientIP = %q, want 198.51.100.1", got)
	}
}

func TestProxyTrustPerIPSpoofingDefeated(t *testing.T) {
	t.Parallel()
	// With no trusted proxies, a flood of unique X-Forwarded-For values
	// from the same peer must all key to the same bucket, so per-IP
	// limiting still bites.
	rl := NewRateLimiter(1, 1, 1, nil)
	defer rl.Stop()
	h := rl.PerIPMiddleware(http.HandlerFunc(ok))

	first := reqWithXFF("203.0.113.9:5555", "1.1.1.1")
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, first)
	if rec.Code != http.StatusOK {
		t.Fatalf("first request blocked: %d", rec.Code)
	}
	// Same peer, different spoofed XFF: must still hit the same bucket.
	second := reqWithXFF("203.0.113.9:5555", "2.2.2.2")
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, second)
	if rec.Code != http.StatusTooManyRequests {
		t.Fatalf("spoofed XFF minted a fresh bucket: code = %d, want 429", rec.Code)
	}
}
