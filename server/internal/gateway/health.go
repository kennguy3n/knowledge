package gateway

import (
	"encoding/json"
	"net/http"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
)

// health probes downstream subsystems and reports an aggregate status.
// The substrate loopback is probed live; optional subsystems (Postgres,
// NATS) are reported from the readiness flags supplied at construction.
func (h *handlers) health(w http.ResponseWriter, r *http.Request) {
	subsystems := map[string]string{}
	overall := "ok"

	raw, err := h.sub.Health(r.Context())
	if err != nil {
		subsystems["substrate"] = "down"
		overall = "degraded"
	} else {
		subsystems["substrate"] = "ok"
		if len(raw) > 0 {
			subsystems["substrate_detail"] = string(raw)
		}
	}
	for name, ready := range h.ready {
		if ready {
			subsystems[name] = "ok"
		} else {
			subsystems[name] = "disabled"
		}
	}

	status := http.StatusOK
	if overall != "ok" {
		status = http.StatusServiceUnavailable
	}
	httpx.WriteJSON(w, status, map[string]any{
		"status":     overall,
		"subsystems": rawSubsystems(subsystems),
	})
}

// rawSubsystems renders the substrate detail field (which is itself
// JSON) inline rather than as an escaped string.
func rawSubsystems(in map[string]string) map[string]json.RawMessage {
	out := make(map[string]json.RawMessage, len(in))
	for k, v := range in {
		if k == "substrate_detail" {
			out[k] = json.RawMessage(v)
			continue
		}
		b, _ := json.Marshal(v)
		out[k] = b
	}
	return out
}
