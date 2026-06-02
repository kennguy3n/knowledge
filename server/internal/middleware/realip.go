package middleware

import (
	"net"
	"net/http"
	"net/netip"
	"strings"
)

// ProxyTrust resolves the client IP to attribute a request to, used as
// the per-IP rate-limit key. It treats the spoofable X-Forwarded-For
// header as authoritative only when the request actually arrived from a
// configured trusted proxy.
//
// The zero value (and a nil *ProxyTrust) trusts no proxy: it ignores
// X-Forwarded-For entirely and keys on the transport peer address. This
// is the secure default for a gateway exposed directly to clients —
// otherwise an attacker could send a unique X-Forwarded-For on every
// request and mint a fresh rate-limit bucket each time, defeating per-IP
// limiting.
type ProxyTrust struct {
	// nets is the set of trusted reverse-proxy networks. Empty means no
	// proxy is trusted.
	nets []netip.Prefix
}

// NewProxyTrust builds a [ProxyTrust] from a list of trusted reverse-proxy
// entries, each either a CIDR ("10.0.0.0/8") or a bare IP ("10.0.0.1",
// promoted to a /32 or /128). Malformed entries are skipped. An empty
// list yields a ProxyTrust that trusts no proxy.
func NewProxyTrust(entries []string) *ProxyTrust {
	pt := &ProxyTrust{}
	for _, e := range entries {
		e = strings.TrimSpace(e)
		if e == "" {
			continue
		}
		if p, err := netip.ParsePrefix(e); err == nil {
			pt.nets = append(pt.nets, p.Masked())
			continue
		}
		if addr, err := netip.ParseAddr(e); err == nil {
			pt.nets = append(pt.nets, netip.PrefixFrom(addr, addr.BitLen()))
		}
	}
	return pt
}

// ClientIP returns the client IP used for per-IP rate limiting and log
// correlation.
//
//   - With no trusted proxies configured, or when the direct peer is not
//     a trusted proxy, it returns the transport peer host (RemoteAddr)
//     and ignores X-Forwarded-For, which is client-controlled and cannot
//     be trusted.
//   - When the direct peer is a trusted proxy, it walks X-Forwarded-For
//     right-to-left, skipping further trusted-proxy hops, and returns the
//     first untrusted address — the real client as seen by the edge. If
//     every hop is trusted (or XFF is absent), it falls back to the peer.
func (pt *ProxyTrust) ClientIP(r *http.Request) string {
	peer := remoteHost(r.RemoteAddr)
	if pt == nil || len(pt.nets) == 0 || !pt.trusted(peer) {
		return peer
	}
	xff := r.Header.Get("X-Forwarded-For")
	if xff == "" {
		return peer
	}
	parts := strings.Split(xff, ",")
	for i := len(parts) - 1; i >= 0; i-- {
		hop := strings.TrimSpace(parts[i])
		if hop == "" || pt.trusted(hop) {
			continue
		}
		return hop
	}
	return peer
}

// trusted reports whether ip parses and falls within a trusted-proxy
// network.
func (pt *ProxyTrust) trusted(ip string) bool {
	addr, err := netip.ParseAddr(ip)
	if err != nil {
		return false
	}
	for _, n := range pt.nets {
		if n.Contains(addr) {
			return true
		}
	}
	return false
}

// remoteHost strips the port from a "host:port" RemoteAddr, returning
// the original string if it has no port.
func remoteHost(remoteAddr string) string {
	host, _, err := net.SplitHostPort(remoteAddr)
	if err != nil {
		return remoteAddr
	}
	return host
}
