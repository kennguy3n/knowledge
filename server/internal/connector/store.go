// Package connector is the Go connector service: it drives connector
// lifecycle through the Rust connectors crate (via substrate_server),
// orchestrates OAuth2 flows, registers webhook receivers, schedules
// periodic syncs, and runs the fetch→ingest→synthesise content
// pipeline.
package connector

import (
	"sync"
	"time"
)

// registration is the Go-side orchestration metadata for one connector
// instance. The authoritative connector record lives in the substrate;
// this tracks supplemental state (OAuth, webhook, schedule).
type registration struct {
	InstanceID    string        `json:"instance_id"`
	Kind          string        `json:"kind"`
	ScopeID       string        `json:"scope_id"`
	WebhookURL    string        `json:"webhook_url,omitempty"`
	WebhookActive bool          `json:"webhook_active"`
	SyncInterval  time.Duration `json:"sync_interval_ns"`
	CreatedAt     time.Time     `json:"created_at"`
}

// oauthStateTTL bounds how long a pending OAuth2 authorization may sit
// before it is considered abandoned. Browser round-trips complete in
// seconds; ten minutes is a generous ceiling that still prevents the
// pending-state map from growing without bound when users start an
// authorize flow and never return.
const oauthStateTTL = 10 * time.Minute

// oauthState is a pending OAuth2 authorization, keyed by CSRF state.
type oauthState struct {
	InstanceID string
	CreatedAt  time.Time
}

// store holds connector orchestration metadata in memory.
type store struct {
	mu     sync.RWMutex
	regs   map[string]registration // instanceID -> registration
	states map[string]oauthState   // csrf state -> pending oauth
}

func newStore() *store {
	return &store{
		regs:   make(map[string]registration),
		states: make(map[string]oauthState),
	}
}

func (s *store) put(r registration) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.regs[r.InstanceID] = r
}

func (s *store) get(id string) (registration, bool) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	r, ok := s.regs[id]
	return r, ok
}

func (s *store) list() []registration {
	s.mu.RLock()
	defer s.mu.RUnlock()
	out := make([]registration, 0, len(s.regs))
	for _, r := range s.regs {
		out = append(out, r)
	}
	return out
}

func (s *store) delete(id string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	delete(s.regs, id)
}

func (s *store) putState(state, instanceID string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	// Opportunistically evict expired pending states so abandoned
	// OAuth flows cannot accumulate unbounded. putState runs once per
	// authorize initiation, so this stays cheap.
	s.pruneExpiredStatesLocked(time.Now().UTC())
	s.states[state] = oauthState{InstanceID: instanceID, CreatedAt: time.Now().UTC()}
}

// takeState consumes a pending OAuth state, returning the associated
// instance id. The state is single-use and removed on lookup. A state
// older than [oauthStateTTL] is treated as expired: it is removed and
// reported as absent so a stale callback cannot complete a flow.
func (s *store) takeState(state string) (string, bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	st, ok := s.states[state]
	if !ok {
		return "", false
	}
	delete(s.states, state)
	if time.Since(st.CreatedAt) > oauthStateTTL {
		return "", false
	}
	return st.InstanceID, true
}

// pruneExpiredStatesLocked removes every pending OAuth state older than
// [oauthStateTTL]. The caller must hold s.mu for writing.
func (s *store) pruneExpiredStatesLocked(now time.Time) {
	for k, st := range s.states {
		if now.Sub(st.CreatedAt) > oauthStateTTL {
			delete(s.states, k)
		}
	}
}
