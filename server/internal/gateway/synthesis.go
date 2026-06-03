package gateway

import (
	"encoding/json"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/go-chi/chi/v5"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
	"github.com/kennguy3n/knowledge/server/internal/metrics"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
	"github.com/kennguy3n/knowledge/server/internal/validate"
)

// synthesisSuccessCounted tracks synthesis IDs that have already been
// counted as successful so the counter increments exactly once per
// synthesis regardless of how many status polls or SSE streams observe
// the terminal state. Entries are inserted on first success observation
// and periodically reaped to bound memory.
var synthesisSuccessCounted sync.Map // map[string]struct{}

// countSynthesisSuccess increments SynthesisSuccessTotal at most once
// per synthesis ID.
func countSynthesisSuccess(id string) {
	if _, loaded := synthesisSuccessCounted.LoadOrStore(id, struct{}{}); !loaded {
		metrics.SynthesisSuccessTotal.Inc()
	}
}

// triggerRequest is the public body of POST /api/v1/synthesis/trigger.
type triggerRequest struct {
	ScopeID string `json:"scope_id"`
	Trigger string `json:"trigger"`
}

func (h *handlers) triggerSynthesis(w http.ResponseWriter, r *http.Request) {
	var req triggerRequest
	if err := httpx.DecodeJSON(r, &req); err != nil {
		httpx.WriteError(w, err)
		return
	}
	scope, err := validate.ScopeID(req.ScopeID)
	if err != nil {
		httpx.WriteError(w, httpx.BadRequest("scope_id must be a UUID"))
		return
	}
	trigger := req.Trigger
	if trigger == "" {
		trigger = "ManualUserAction"
	}
	raw, err := h.sub.TriggerSynthesis(r.Context(), substrate.SynthesisTriggerRequest{
		ScopeID: scope,
		Trigger: trigger,
	})
	if err != nil {
		metrics.ErrorsTotal.WithLabelValues("synthesis").Inc()
		httpx.WriteError(w, err)
		return
	}
	metrics.SynthesisTriggerTotal.Inc()
	writeRaw(w, http.StatusAccepted, raw)
}

func (h *handlers) recentSyntheses(w http.ResponseWriter, r *http.Request) {
	scope, err := validate.ScopeID(r.URL.Query().Get("scope_id"))
	if err != nil {
		httpx.WriteError(w, httpx.BadRequest("scope_id must be a UUID"))
		return
	}
	raw, err := h.sub.RecentSyntheses(r.Context(), substrate.RecentSynthesisRequest{ScopeID: scope})
	if err != nil {
		httpx.WriteError(w, err)
		return
	}
	writeRaw(w, http.StatusOK, raw)
}

// synthesisStatus returns the status of a synthesis run. When the
// client requests it via Accept: text/event-stream (or ?stream=true),
// the status is streamed as Server-Sent Events, polling the substrate
// until a terminal state is reached; otherwise a single JSON snapshot
// is returned.
func (h *handlers) synthesisStatus(w http.ResponseWriter, r *http.Request) {
	id, err := validate.ScopeID(chi.URLParam(r, "id"))
	if err != nil {
		httpx.WriteError(w, httpx.BadRequest("synthesis id must be a UUID"))
		return
	}
	if wantsStream(r) {
		h.streamSynthesis(w, r, id)
		return
	}
	raw, err := h.sub.SynthesisStatus(r.Context(), id)
	if err != nil {
		httpx.WriteError(w, err)
		return
	}
	if isSuccessStatus(raw) {
		countSynthesisSuccess(id)
	}
	writeRaw(w, http.StatusOK, raw)
}

// streamPollInterval is the cadence at which SSE status polls the
// substrate.
const streamPollInterval = time.Second

// streamMaxPolls bounds an SSE stream so a stuck run cannot hold a
// connection open indefinitely.
const streamMaxPolls = 300

func (h *handlers) streamSynthesis(w http.ResponseWriter, r *http.Request, id string) {
	flusher, ok := w.(http.Flusher)
	if !ok {
		httpx.WriteError(w, httpx.Internal("streaming unsupported"))
		return
	}
	// SSE streams are long-lived, so opt this request out of the
	// server's bounded WriteTimeout by clearing the write deadline. The
	// zero deadline means "no timeout". Failure is non-fatal (e.g. the
	// test ResponseWriter doesn't support deadlines); the streamMaxPolls
	// cap still bounds the stream's lifetime.
	_ = http.NewResponseController(w).SetWriteDeadline(time.Time{})

	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("Connection", "keep-alive")
	w.WriteHeader(http.StatusOK)

	ticker := time.NewTicker(streamPollInterval)
	defer ticker.Stop()
	ctx := r.Context()

	for i := 0; i < streamMaxPolls; i++ {
		// Emit a comment heartbeat before the (potentially slow)
		// status fetch so bytes keep flowing and HTTP intermediaries
		// with short idle timeouts don't drop a stream that is merely
		// waiting on a slow substrate poll.
		writeSSEComment(w, flusher, "keepalive")
		raw, err := h.sub.SynthesisStatus(ctx, id)
		if err != nil {
			writeSSE(w, flusher, "error", json.RawMessage(`{"message":"status unavailable"}`))
			return
		}
		writeSSE(w, flusher, "status", raw)
		if isTerminalStatus(raw) {
			if isSuccessStatus(raw) {
				countSynthesisSuccess(id)
			}
			writeSSE(w, flusher, "done", json.RawMessage(`{"complete":true}`))
			return
		}
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
		}
	}
}

// wantsStream reports whether the client requested SSE streaming.
func wantsStream(r *http.Request) bool {
	if r.URL.Query().Get("stream") == "true" {
		return true
	}
	return strings.Contains(r.Header.Get("Accept"), "text/event-stream")
}

// writeSSE writes one Server-Sent Event frame and flushes it.
func writeSSE(w http.ResponseWriter, f http.Flusher, event string, data json.RawMessage) {
	_, _ = w.Write([]byte("event: " + event + "\n"))
	_, _ = w.Write([]byte("data: "))
	_, _ = w.Write(data)
	_, _ = w.Write([]byte("\n\n"))
	f.Flush()
}

// writeSSEComment writes an SSE comment line (a frame beginning with
// ':'), used as a keepalive. Comments are ignored by SSE clients but
// keep the connection warm through idle-sensitive proxies.
func writeSSEComment(w http.ResponseWriter, f http.Flusher, text string) {
	_, _ = w.Write([]byte(": " + text + "\n\n"))
	f.Flush()
}

// synthesisProbe extracts Status/State from a synthesis status doc.
type synthesisProbe struct {
	Status string `json:"status"`
	State  string `json:"state"`
}

func parseSynthesisStatus(raw json.RawMessage) (synthesisProbe, bool) {
	var p synthesisProbe
	if err := json.Unmarshal(raw, &p); err != nil {
		return p, false
	}
	return p, true
}

// isTerminalStatus reports whether a synthesis status document
// represents a completed or failed run.
func isTerminalStatus(raw json.RawMessage) bool {
	p, ok := parseSynthesisStatus(raw)
	if !ok {
		return true // undecodable: stop streaming rather than loop forever
	}
	s := strings.ToLower(p.Status + " " + p.State)
	return strings.Contains(s, "complete") || strings.Contains(s, "fail") ||
		strings.Contains(s, "done") || strings.Contains(s, "error")
}

// isSuccessStatus reports whether a synthesis status document
// represents a successful completion (not a failure).
func isSuccessStatus(raw json.RawMessage) bool {
	p, ok := parseSynthesisStatus(raw)
	if !ok {
		return false
	}
	s := strings.ToLower(p.Status + " " + p.State)
	if strings.Contains(s, "fail") || strings.Contains(s, "error") {
		return false
	}
	return strings.Contains(s, "complete") || strings.Contains(s, "done")
}
