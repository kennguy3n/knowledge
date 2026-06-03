package connector

import (
	"context"
	"net/http"
	"time"

	"github.com/go-chi/chi/v5"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
	"github.com/kennguy3n/knowledge/server/internal/middleware"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
)

// detachedContext derives a background context for async work that
// outlives the inbound request, preserving the X-Request-Id for
// loopback propagation while dropping the request's cancellation. The
// caller must invoke the returned cancel func when the work completes.
func detachedContext(r *http.Request) (context.Context, context.CancelFunc) {
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Minute)
	if id := middleware.RequestID(r.Context()); id != "" {
		ctx = substrate.WithRequestID(ctx, id)
	}
	return ctx, cancel
}

// handleOAuthStart builds the provider authorization URL for a
// connector instance and records a single-use CSRF state. Query params
// client_id and redirect_uri are required.
func (s *Service) handleOAuthStart(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "id")
	reg, ok := s.store.get(id)
	if !ok {
		httpx.WriteError(w, httpx.NotFound("connector not found"))
		return
	}
	clientID := r.URL.Query().Get("client_id")
	redirectURI := r.URL.Query().Get("redirect_uri")
	if clientID == "" || redirectURI == "" {
		httpx.WriteError(w, httpx.BadRequest("client_id and redirect_uri are required"))
		return
	}
	state := newState()
	url, ok := authorizeURL(reg.Kind, clientID, redirectURI, state)
	if !ok {
		httpx.WriteError(w, httpx.BadRequest("connector kind has no OAuth2 provider"))
		return
	}
	s.store.putState(state, id)
	httpx.WriteJSON(w, http.StatusOK, map[string]string{
		"authorize_url": url,
		"state":         state,
	})
}

// handleOAuthCallback completes an OAuth2 code exchange: it validates
// the CSRF state, resolves the connector instance, and forwards the
// authorization code to the substrate's authenticate endpoint.
func (s *Service) handleOAuthCallback(w http.ResponseWriter, r *http.Request) {
	code := r.URL.Query().Get("code")
	state := r.URL.Query().Get("state")
	if code == "" || state == "" {
		httpx.WriteError(w, httpx.BadRequest("code and state are required"))
		return
	}
	instanceID, ok := s.store.takeState(state)
	if !ok {
		httpx.WriteError(w, httpx.BadRequest("unknown or expired OAuth state"))
		return
	}
	raw, err := s.sub.AuthenticateConnector(r.Context(), instanceID,
		substrate.AuthenticateRequest{AuthCode: code})
	if err != nil {
		httpx.WriteError(w, err)
		return
	}
	writeRaw(w, http.StatusOK, raw)
}

// handleWebhookRegister marks a connector as webhook-enabled and
// returns the callback URL the provider should post events to. Actual
// provider-side subscription is performed by the connector's own auth
// flow (fixture connectors do not perform live registration).
func (s *Service) handleWebhookRegister(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "id")
	reg, ok := s.store.get(id)
	if !ok {
		httpx.WriteError(w, httpx.NotFound("connector not found"))
		return
	}
	reg.WebhookURL = s.publicBaseURL + "/api/v1/connectors/" + id + "/webhook"
	reg.WebhookActive = true
	s.store.put(reg)
	httpx.WriteJSON(w, http.StatusOK, map[string]string{
		"webhook_url": reg.WebhookURL,
	})
}

// handleWebhookReceive accepts an inbound provider webhook. The body is
// acknowledged immediately and a sync+pipeline run is kicked off in the
// background so the provider sees a fast 202.
func (s *Service) handleWebhookReceive(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "id")
	reg, ok := s.store.get(id)
	if !ok || !reg.WebhookActive {
		httpx.WriteError(w, httpx.NotFound("no active webhook for connector"))
		return
	}
	ctx, cancel := detachedContext(r)
	// Bound the number of concurrent webhook-triggered syncs. If the
	// semaphore is full we shed load with 429 instead of spawning an
	// unbounded goroutine; providers retry webhooks on non-2xx.
	select {
	case s.webhookSem <- struct{}{}:
	default:
		cancel()
		httpx.WriteError(w, httpx.TooManyRequests("webhook processing at capacity; retry later"))
		return
	}
	s.webhookWG.Add(1)
	go func() {
		defer s.webhookWG.Done()
		defer cancel()
		defer func() { <-s.webhookSem }()
		s.syncAndProcess(ctx, id)
	}()
	w.WriteHeader(http.StatusAccepted)
}
