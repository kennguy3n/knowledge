package gateway

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/go-chi/chi/v5"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
	"github.com/kennguy3n/knowledge/server/internal/metrics"
	"github.com/kennguy3n/knowledge/server/internal/middleware"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
	"github.com/kennguy3n/knowledge/server/internal/validate"
)

// synthesisFairShare is the per-tenant + global admission controller in
// front of the shared, CPU-bound llama-server synthesis path. It is a
// package-level singleton (configured from the KNOWLEDGE_SYNTHESIS_*
// environment) so a single tenant triggering many syntheses cannot
// starve the rest of the fleet. See [middleware.SynthesisFairShare].
var synthesisFairShare = middleware.NewSynthesisFairShareFromEnv()

// synthesisDedup deduplicates SynthesisSuccessTotal increments so the
// counter fires at most once per synthesis ID. It uses a time-windowed
// double-buffer: two maps alternate every reapInterval, so each entry
// lives for 1–2 intervals before being garbage-collected. This bounds
// memory to at most 2× the number of unique completions per interval.
var synthesisDedup = newSynthesisDedup(10 * time.Minute)

type synthDedup struct {
	mu      sync.Mutex
	current map[string]struct{}
	prev    map[string]struct{}
}

func newSynthesisDedup(interval time.Duration) *synthDedup {
	sd := &synthDedup{
		current: make(map[string]struct{}),
		prev:    make(map[string]struct{}),
	}
	go sd.reapLoop(interval)
	return sd
}

func (sd *synthDedup) reapLoop(interval time.Duration) {
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for range ticker.C {
		sd.mu.Lock()
		sd.prev = sd.current
		sd.current = make(map[string]struct{})
		sd.mu.Unlock()
	}
}

// seen reports whether id was already counted (and marks it if not).
func (sd *synthDedup) seen(id string) bool {
	sd.mu.Lock()
	defer sd.mu.Unlock()
	if _, ok := sd.current[id]; ok {
		return true
	}
	if _, ok := sd.prev[id]; ok {
		return true
	}
	sd.current[id] = struct{}{}
	return false
}

// countSynthesisSuccess increments SynthesisSuccessTotal at most once
// per synthesis ID within the dedup window.
func countSynthesisSuccess(id string) {
	if !synthesisDedup.seen(id) {
		metrics.SynthesisSuccessTotal.Inc()
	}
}

// triggerWriteDeadline bounds how long the synchronous synthesis-trigger
// response may take to write. It sits above the substrate client's
// synthesisTimeout (3 min) so the upstream call returns a proper result
// or error before the write deadline severs the connection.
const triggerWriteDeadline = 4 * time.Minute

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
	// Fair-share admission: cap concurrent syntheses per tenant (bounded
	// FIFO queue) and globally so one tenant cannot monopolise the shared
	// llama-server. Over-cap requests are shed with 429 + Retry-After
	// rather than piling onto the CPU-bound synthesis path.
	release, retryAfter, throttled := synthesisFairShare.Acquire(r.Context(), middleware.TenantID(r.Context()))
	if throttled != nil {
		metrics.SynthesisThrottleTotal.Inc()
		w.Header().Set("Retry-After", strconv.Itoa(retryAfter))
		httpx.WriteError(w, throttled)
		return
	}
	defer release()
	// Synthesis runs synchronously in the substrate (on-device SLM), so
	// a verbose scope can take well past the server's default 60 s
	// WriteTimeout. Extend the write deadline for this request so a
	// legitimately slow run isn't severed mid-flight; the substrate
	// client's own synthesisTimeout still bounds the upstream call.
	// Failure is non-fatal (e.g. a test ResponseWriter without deadline
	// support).
	_ = http.NewResponseController(w).SetWriteDeadline(time.Now().Add(triggerWriteDeadline))
	raw, err := h.sub.TriggerSynthesis(r.Context(), substrate.SynthesisTriggerRequest{
		ScopeID: scope,
		Trigger: trigger,
	})
	if err != nil {
		var apiErr *httpx.Error
		if errors.As(err, &apiErr) && apiErr.Status == http.StatusTooManyRequests {
			metrics.SynthesisThrottleTotal.Inc()
		} else {
			metrics.ErrorsTotal.WithLabelValues("synthesis").Inc()
		}
		httpx.WriteError(w, err)
		return
	}
	metrics.SynthesisTriggerTotal.Inc()
	writeRaw(w, http.StatusAccepted, raw)
}

// serverSynthesisRequest is the public body of
// POST /api/v1/synthesis/{domain,tenant}. The tier is fixed by the
// route, so the only field is the scope to roll up.
type serverSynthesisRequest struct {
	ScopeID string `json:"scope_id"`
}

// triggerDomainSynthesis rolls up a domain's registered channel outputs
// into a DomainSummary (the server-side domain tier of the synthesis
// hierarchy). It mirrors [handlers.triggerSynthesis]'s fair-share
// admission, extended write deadline and metrics, differing only in the
// substrate method it dispatches.
func (h *handlers) triggerDomainSynthesis(w http.ResponseWriter, r *http.Request) {
	h.triggerServerSynthesis(w, r, h.sub.TriggerDomainSynthesis)
}

// triggerTenantSynthesis rolls up a tenant's registered domain outputs
// plus approved documents into a TenantSummary (the tenant tier). The
// tenant-tier counterpart to [handlers.triggerDomainSynthesis].
func (h *handlers) triggerTenantSynthesis(w http.ResponseWriter, r *http.Request) {
	h.triggerServerSynthesis(w, r, h.sub.TriggerTenantSynthesis)
}

// triggerServerSynthesis is the shared plumbing behind the domain and
// tenant trigger routes. It validates the scope, applies the same
// per-tenant + global fair-share admission as the channel trigger
// (server-side synthesis hits the same CPU-bound SLM path), extends the
// write deadline past the substrate client's synthesis timeout, and
// dispatches `call` (the tier-specific substrate method).
func (h *handlers) triggerServerSynthesis(
	w http.ResponseWriter,
	r *http.Request,
	call func(context.Context, substrate.ServerSynthesisRequest) (json.RawMessage, error),
) {
	var req serverSynthesisRequest
	if err := httpx.DecodeJSON(r, &req); err != nil {
		httpx.WriteError(w, err)
		return
	}
	scope, err := validate.ScopeID(req.ScopeID)
	if err != nil {
		httpx.WriteError(w, httpx.BadRequest("scope_id must be a UUID"))
		return
	}
	release, retryAfter, throttled := synthesisFairShare.Acquire(r.Context(), middleware.TenantID(r.Context()))
	if throttled != nil {
		metrics.SynthesisThrottleTotal.Inc()
		w.Header().Set("Retry-After", strconv.Itoa(retryAfter))
		httpx.WriteError(w, throttled)
		return
	}
	defer release()
	_ = http.NewResponseController(w).SetWriteDeadline(time.Now().Add(triggerWriteDeadline))
	raw, err := call(r.Context(), substrate.ServerSynthesisRequest{ScopeID: scope})
	if err != nil {
		var apiErr *httpx.Error
		if errors.As(err, &apiErr) && apiErr.Status == http.StatusTooManyRequests {
			metrics.SynthesisThrottleTotal.Inc()
		} else {
			metrics.ErrorsTotal.WithLabelValues("synthesis").Inc()
		}
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

// Recognised lifecycle tokens, matched by exact equality on a single
// status field rather than substring containment so a value like
// "incomplete" cannot be mistaken for "complete" (its substring).
//
// The canonical synthesis vocabulary is the snake_case form of the
// substrate's WindowStatus — pending / in_progress / complete / failed.
// The success/failure aliases (done, completed, error, …) cover the
// model-readiness `state` field and defensive spellings so an alternate
// doc shape is still classified as terminal rather than streamed until
// the poll cap.
var (
	successTokens = map[string]struct{}{
		"complete": {}, "completed": {}, "done": {},
	}
	failureTokens = map[string]struct{}{
		"fail": {}, "failed": {}, "failure": {},
		"error": {}, "errored": {},
	}
)

// statusTokens returns the lowercased, whitespace-trimmed values of the
// lifecycle `status` and model-readiness `state` fields, dropping any
// that are empty. Each is classified on its own so matching never spans
// a field boundary.
func statusTokens(p synthesisProbe) []string {
	tokens := make([]string, 0, 2)
	for _, field := range [...]string{p.Status, p.State} {
		if t := strings.ToLower(strings.TrimSpace(field)); t != "" {
			tokens = append(tokens, t)
		}
	}
	return tokens
}

// isTerminalStatus reports whether a synthesis status document
// represents a completed or failed run.
func isTerminalStatus(raw json.RawMessage) bool {
	p, ok := parseSynthesisStatus(raw)
	if !ok {
		return true // undecodable: stop streaming rather than loop forever
	}
	for _, t := range statusTokens(p) {
		if _, success := successTokens[t]; success {
			return true
		}
		if _, failure := failureTokens[t]; failure {
			return true
		}
	}
	return false
}

// isSuccessStatus reports whether a synthesis status document
// represents a successful completion (not a failure). A failure token
// on either field wins, so a doc that is somehow both complete and
// failed is treated conservatively as a non-success.
func isSuccessStatus(raw json.RawMessage) bool {
	p, ok := parseSynthesisStatus(raw)
	if !ok {
		return false
	}
	tokens := statusTokens(p)
	for _, t := range tokens {
		if _, failure := failureTokens[t]; failure {
			return false
		}
	}
	for _, t := range tokens {
		if _, success := successTokens[t]; success {
			return true
		}
	}
	return false
}
