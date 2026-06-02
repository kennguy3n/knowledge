package gateway

import (
	"encoding/json"
	"net/http"
	"strings"
	"time"

	"github.com/go-chi/chi/v5"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
	"github.com/kennguy3n/knowledge/server/internal/validate"
)

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
		httpx.WriteError(w, err)
		return
	}
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
	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("Connection", "keep-alive")
	w.WriteHeader(http.StatusOK)

	ticker := time.NewTicker(streamPollInterval)
	defer ticker.Stop()
	ctx := r.Context()

	for i := 0; i < streamMaxPolls; i++ {
		raw, err := h.sub.SynthesisStatus(ctx, id)
		if err != nil {
			writeSSE(w, flusher, "error", json.RawMessage(`{"message":"status unavailable"}`))
			return
		}
		writeSSE(w, flusher, "status", raw)
		if isTerminalStatus(raw) {
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

// isTerminalStatus reports whether a synthesis status document
// represents a completed or failed run.
func isTerminalStatus(raw json.RawMessage) bool {
	var probe struct {
		Status string `json:"status"`
		State  string `json:"state"`
	}
	if err := json.Unmarshal(raw, &probe); err != nil {
		return true // undecodable: stop streaming rather than loop forever
	}
	s := strings.ToLower(probe.Status + probe.State)
	return strings.Contains(s, "complete") || strings.Contains(s, "fail") ||
		strings.Contains(s, "done") || strings.Contains(s, "error")
}
