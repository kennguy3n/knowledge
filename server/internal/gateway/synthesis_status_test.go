package gateway

import (
	"encoding/json"
	"testing"
)

// TestSynthesisStatusClassification pins the SSE terminal/success
// classification against the canonical WindowStatus vocabulary
// (pending / in_progress / complete / failed) and its tolerated
// aliases, and guards the substring-matching regression where a value
// like "incomplete" was mistaken for "complete".
func TestSynthesisStatusClassification(t *testing.T) {
	t.Parallel()
	cases := []struct {
		name     string
		raw      string
		terminal bool
		success  bool
	}{
		{"pending", `{"status":"pending"}`, false, false},
		{"in_progress", `{"status":"in_progress"}`, false, false},
		{"complete", `{"status":"complete"}`, true, true},
		{"failed", `{"status":"failed"}`, true, false},

		// Tolerated aliases on the model-readiness `state` field.
		{"state_complete", `{"state":"complete"}`, true, true},
		{"state_failed", `{"state":"failed"}`, true, false},
		{"done_alias", `{"status":"done"}`, true, true},
		{"error_alias", `{"status":"error"}`, true, false},

		// Case-insensitive.
		{"upper_complete", `{"status":"COMPLETE"}`, true, true},
		{"mixed_failed", `{"status":"Failed"}`, true, false},

		// Regression: substrings of a recognised token must NOT match.
		{"incomplete_is_not_complete", `{"status":"incomplete"}`, false, false},
		{"completing_is_not_complete", `{"status":"completing"}`, false, false},
		{"failover_is_not_fail", `{"status":"failover"}`, false, false},

		// A failure token wins over a success token (conservative).
		{"complete_but_failed", `{"status":"complete","state":"failed"}`, true, false},

		// Unknown but decodable status: keep streaming until the cap.
		{"unknown_running", `{"status":"running"}`, false, false},
		{"empty_fields", `{"status":"","state":""}`, false, false},

		// Undecodable doc: terminal (stop looping) but not a success.
		{"undecodable", `not json`, true, false},
	}

	for _, tc := range cases {
		tc := tc
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			raw := json.RawMessage(tc.raw)
			if got := isTerminalStatus(raw); got != tc.terminal {
				t.Errorf("isTerminalStatus(%s) = %v, want %v", tc.raw, got, tc.terminal)
			}
			if got := isSuccessStatus(raw); got != tc.success {
				t.Errorf("isSuccessStatus(%s) = %v, want %v", tc.raw, got, tc.success)
			}
		})
	}
}
