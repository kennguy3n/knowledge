// Package export is the Go export service: it renders portable concept
// profiles by evaluating them against an export policy in the Rust
// export_plane (via substrate_server), assembles a policy-enforced
// evidence pack, produces Markdown/HTML summaries, and writes an audit
// entry on every export.
package export

import (
	"fmt"
	"html"
	"strings"

	"github.com/kennguy3n/knowledge/server/internal/substrate"
)

// Format selects the rendering of an export decision.
type Format string

// Supported export formats.
const (
	FormatJSON     Format = "json"
	FormatMarkdown Format = "markdown"
	FormatHTML     Format = "html"
)

// EvidencePack is the policy-enforced export payload. RawEvidenceOmitted
// is true when the policy denied raw evidence, in which case only
// concept-level metadata is exported.
type EvidencePack struct {
	Approved           []substrate.ApprovedConcept `json:"approved"`
	Warnings           []string                    `json:"warnings"`
	RejectedCount      int                         `json:"rejected_count"`
	RawEvidenceOmitted bool                        `json:"raw_evidence_omitted"`
}

// buildPack applies policy enforcement to an export decision.
func buildPack(d substrate.ExportDecision) EvidencePack {
	return EvidencePack{
		Approved:           d.Approved,
		Warnings:           d.Warnings,
		RejectedCount:      len(d.Rejected),
		RawEvidenceOmitted: !d.AllowRawEvidence,
	}
}

// renderMarkdown renders a human-readable Markdown summary.
func renderMarkdown(pack EvidencePack) string {
	var b strings.Builder
	b.WriteString("# Concept Profile Export\n\n")
	fmt.Fprintf(&b, "- Approved concepts: %d\n", len(pack.Approved))
	fmt.Fprintf(&b, "- Rejected concepts: %d\n", pack.RejectedCount)
	fmt.Fprintf(&b, "- Raw evidence included: %t\n\n", !pack.RawEvidenceOmitted)
	if len(pack.Warnings) > 0 {
		b.WriteString("## Warnings\n\n")
		for _, w := range pack.Warnings {
			fmt.Fprintf(&b, "- %s\n", w)
		}
		b.WriteString("\n")
	}
	b.WriteString("## Concepts\n\n")
	for _, c := range pack.Approved {
		fmt.Fprintf(&b, "### %s\n\n", c.Label)
		fmt.Fprintf(&b, "- Sensitivity: %s\n", c.SensitivityClass)
		if c.Definition != "" {
			fmt.Fprintf(&b, "- Definition: %s\n", c.Definition)
		}
		b.WriteString("\n")
	}
	return b.String()
}

// renderHTML renders an HTML summary with all dynamic text escaped.
func renderHTML(pack EvidencePack) string {
	var b strings.Builder
	b.WriteString("<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\">")
	b.WriteString("<title>Concept Profile Export</title></head><body>\n")
	b.WriteString("<h1>Concept Profile Export</h1>\n<ul>\n")
	fmt.Fprintf(&b, "<li>Approved concepts: %d</li>\n", len(pack.Approved))
	fmt.Fprintf(&b, "<li>Rejected concepts: %d</li>\n", pack.RejectedCount)
	fmt.Fprintf(&b, "<li>Raw evidence included: %t</li>\n", !pack.RawEvidenceOmitted)
	b.WriteString("</ul>\n")
	if len(pack.Warnings) > 0 {
		b.WriteString("<h2>Warnings</h2>\n<ul>\n")
		for _, w := range pack.Warnings {
			fmt.Fprintf(&b, "<li>%s</li>\n", html.EscapeString(w))
		}
		b.WriteString("</ul>\n")
	}
	b.WriteString("<h2>Concepts</h2>\n")
	for _, c := range pack.Approved {
		fmt.Fprintf(&b, "<h3>%s</h3>\n<ul>\n", html.EscapeString(c.Label))
		fmt.Fprintf(&b, "<li>Sensitivity: %s</li>\n", html.EscapeString(c.SensitivityClass))
		if c.Definition != "" {
			fmt.Fprintf(&b, "<li>Definition: %s</li>\n", html.EscapeString(c.Definition))
		}
		b.WriteString("</ul>\n")
	}
	b.WriteString("</body></html>\n")
	return b.String()
}
