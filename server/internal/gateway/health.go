package gateway

import (
	"encoding/json"
	"net/http"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
	"github.com/kennguy3n/knowledge/server/internal/metrics"
)

// health probes downstream subsystems and reports an aggregate status.
// The substrate loopback is probed live; optional subsystems (Postgres,
// NATS) are reported from the readiness flags supplied at construction.
func (h *handlers) health(w http.ResponseWriter, r *http.Request) {
	subsystems := map[string]string{}
	overall := "ok"

	// replication carries the active-passive failover summary lifted out
	// of the substrate's health payload (role / lag_frames /
	// last_applied_at, …). nil for a standalone substrate or when the
	// substrate is unreachable.
	var replication json.RawMessage

	raw, err := h.sub.Health(r.Context())
	if err != nil {
		subsystems["substrate"] = "down"
		overall = "degraded"
		metrics.SubsystemStatus.WithLabelValues("substrate").Set(0)
	} else {
		subsystems["substrate"] = "ok"
		metrics.SubsystemStatus.WithLabelValues("substrate").Set(1)
		if len(raw) > 0 {
			subsystems["substrate_detail"] = string(raw)
			replication = extractReplication(raw)
		}
	}
	for name, ready := range h.ready {
		if ready {
			subsystems[name] = "ok"
			metrics.SubsystemStatus.WithLabelValues(name).Set(1)
		} else {
			subsystems[name] = "disabled"
			// Do not publish a gauge for disabled subsystems — gauge 0
			// is indistinguishable from "down" and would trigger
			// KnowledgeSubsystemDown. Omitting the time series means
			// the alert expression has nothing to match.
		}
	}

	status := http.StatusOK
	if overall != "ok" {
		status = http.StatusServiceUnavailable
	}
	body := map[string]any{
		"status":     overall,
		"subsystems": rawSubsystems(subsystems),
	}
	if replication != nil {
		body["replication"] = replication
	}
	httpx.WriteJSON(w, status, body)
}

// extractReplication pulls the `replication` object out of the
// substrate's health payload so the gateway can surface failover state
// (role / lag_frames / last_applied_at) at the top level. Returns nil
// when the field is absent (standalone substrate) or the payload is not
// a JSON object.
func extractReplication(raw json.RawMessage) json.RawMessage {
	var envelope struct {
		Replication json.RawMessage `json:"replication"`
	}
	if err := json.Unmarshal(raw, &envelope); err != nil {
		return nil
	}
	return envelope.Replication
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
