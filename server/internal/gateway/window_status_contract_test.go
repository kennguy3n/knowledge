package gateway

import (
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"testing"
	"unicode"
)

// windowStatusSourcePath is the substrate's canonical synthesis
// lifecycle enum, relative to this package directory
// (server/internal/gateway).
const windowStatusSourcePath = "../../../crates/synthesis_pipeline/src/window.rs"

var (
	windowStatusEnumRe    = regexp.MustCompile(`(?s)pub enum WindowStatus\s*\{(.*?)\n\}`)
	windowStatusVariantRe = regexp.MustCompile(`^([A-Z][A-Za-z0-9]*)\s*,?$`)
)

// rustSnakeCase mirrors serde's rename_all = "snake_case": insert an
// underscore before each interior uppercase letter and lowercase the
// rest (InProgress -> in_progress, Pending -> pending).
func rustSnakeCase(variant string) string {
	var b strings.Builder
	for i, r := range variant {
		if i > 0 && unicode.IsUpper(r) {
			b.WriteByte('_')
		}
		b.WriteRune(unicode.ToLower(r))
	}
	return b.String()
}

// parseWindowStatusVariants extracts the WindowStatus variant
// identifiers from the Rust source. The Rust compiler enforces that
// WindowStatus::as_str matches every variant exhaustively, so the enum
// body is the authoritative list of statuses the substrate can emit.
func parseWindowStatusVariants(t *testing.T, src string) []string {
	t.Helper()
	body := windowStatusEnumRe.FindStringSubmatch(src)
	if body == nil {
		t.Fatalf("could not locate `pub enum WindowStatus { ... }` in %s; "+
			"the enum may have been renamed or restructured", windowStatusSourcePath)
	}
	var variants []string
	for _, line := range strings.Split(body[1], "\n") {
		line = strings.TrimSpace(line)
		if line == "" || strings.HasPrefix(line, "//") || strings.HasPrefix(line, "#[") {
			continue
		}
		if m := windowStatusVariantRe.FindStringSubmatch(line); m != nil {
			variants = append(variants, m[1])
		}
	}
	return variants
}

// TestWindowStatusContract guards against drift between the substrate's
// WindowStatus enum (the source of synthesis status strings) and the Go
// gateway's status classifier. If a new variant is added on the Rust
// side (e.g. Cancelled, TimedOut) without a matching entry in
// successTokens / failureTokens / pendingTokens, the SSE stream would
// silently treat it as non-terminal and poll to streamMaxPolls instead
// of ending the stream. This test fails in that case, forcing an
// explicit classification decision.
func TestWindowStatusContract(t *testing.T) {
	t.Parallel()

	raw, err := os.ReadFile(filepath.Clean(windowStatusSourcePath))
	if err != nil {
		t.Fatalf("read WindowStatus source %s: %v", windowStatusSourcePath, err)
	}

	variants := parseWindowStatusVariants(t, string(raw))
	// The current vocabulary is Pending/InProgress/Complete/Failed; a
	// parse yielding fewer means the regex no longer matches the source.
	if len(variants) < 4 {
		t.Fatalf("parsed %d WindowStatus variants (%v) from %s; expected at least 4 — "+
			"the enum layout likely changed and the parser needs updating",
			len(variants), variants, windowStatusSourcePath)
	}

	recognized := func(token string) bool {
		for _, set := range []map[string]struct{}{successTokens, failureTokens, pendingTokens} {
			if _, ok := set[token]; ok {
				return true
			}
		}
		return false
	}

	for _, variant := range variants {
		token := rustSnakeCase(variant)
		if !recognized(token) {
			t.Errorf("WindowStatus::%s (serializes as %q) is not classified by the gateway; "+
				"add it to successTokens, failureTokens, or pendingTokens in synthesis.go",
				variant, token)
		}
	}
}
