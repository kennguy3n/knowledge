package connector

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"

	"github.com/kennguy3n/knowledge/server/internal/httpx"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
)

// substrateAPI is the subset of [substrate.Client] the connector
// service needs. Narrowing it lets tests inject a fake loopback.
type substrateAPI interface {
	CreateConnector(ctx context.Context, req substrate.CreateConnectorRequest) (substrate.IDResponse, error)
	ListConnectors(ctx context.Context) (json.RawMessage, error)
	AuthenticateConnector(ctx context.Context, id string, req substrate.AuthenticateRequest) (json.RawMessage, error)
	SyncConnector(ctx context.Context, id string) (json.RawMessage, error)
	RemoveConnector(ctx context.Context, id string) error
	ConnectorStatus(ctx context.Context, id string) (json.RawMessage, error)
	FetchContent(ctx context.Context, req substrate.FetchContentRequest) (json.RawMessage, error)
	Ingest(ctx context.Context, req substrate.IngestRequest) (substrate.IDResponse, error)
	TriggerSynthesis(ctx context.Context, req substrate.SynthesisTriggerRequest) (json.RawMessage, error)
}

// fetchedContent is the expected shape of a `POST /connector/fetch_content`
// reply. Fields are decoded leniently so the pipeline tolerates a
// partially-specified upstream.
type fetchedContent struct {
	Body       string `json:"body"`
	Source     string `json:"source"`
	Importance string `json:"importance"`
}

// PipelineResult summarises one content-pipeline run.
type PipelineResult struct {
	// Fetched is the number of content refs fetched successfully.
	Fetched int `json:"fetched"`
	// Ingested is the number of items written to the evidence store.
	Ingested int `json:"ingested"`
	// Skipped is the number of refs skipped (empty body or unavailable).
	Skipped int `json:"skipped"`
	// Unavailable is true when fetch_content is not yet implemented
	// upstream (HTTP 501); the pipeline degrades gracefully.
	Unavailable bool `json:"unavailable"`
}

// runPipeline fetches each content ref, ingests non-empty bodies into
// the evidence store, and triggers observation extraction once any
// content was ingested. A 501 from fetch_content marks the feature
// unavailable and short-circuits without error so callers still
// succeed while the content-fetch endpoint is unavailable.
func (s *Service) runPipeline(ctx context.Context, instanceID, scopeID, kind string, refs []string) (PipelineResult, error) {
	var res PipelineResult
	for i, ref := range refs {
		raw, err := s.sub.FetchContent(ctx, substrate.FetchContentRequest{
			InstanceID: instanceID,
			ContentRef: ref,
		})
		if err != nil {
			if isNotImplemented(err) {
				res.Unavailable = true
				// Short-circuit: count the current ref plus every
				// not-yet-attempted ref as skipped so the invariant
				// Fetched+Ingested+Skipped == len(refs) holds.
				res.Skipped += len(refs) - i
				return res, nil
			}
			return res, err
		}
		res.Fetched++

		var fc fetchedContent
		if err := json.Unmarshal(raw, &fc); err != nil || fc.Body == "" {
			res.Skipped++
			continue
		}
		source := fc.Source
		if source == "" {
			source = sourceKindForConnector(kind)
		}
		importance := fc.Importance
		if importance == "" {
			importance = "Useful"
		}
		if _, err := s.sub.Ingest(ctx, substrate.IngestRequest{
			ScopeID:    scopeID,
			Body:       fc.Body,
			Source:     source,
			Importance: importance,
		}); err != nil {
			return res, err
		}
		res.Ingested++
	}

	if res.Ingested > 0 {
		if _, err := s.sub.TriggerSynthesis(ctx, substrate.SynthesisTriggerRequest{
			ScopeID: scopeID,
			Trigger: "ConnectorSyncCompleted",
		}); err != nil {
			return res, err
		}
	}
	return res, nil
}

// sourceKindForConnector maps a connector kind (the snake_case
// ConnectorKindTag the SPA/substrate speak) to the coarse SourceKind tag
// the substrate's IngestRequest deserializes (`ffi::SourceKind`, which is
// PascalCase with only the variants below). It's the fallback source for
// fetched content that doesn't declare its own.
//
// SourceKind is deliberately coarser than the full connector-kind
// taxonomy: a whole product family collapses to one transport tag, per
// the SourceKind doc (`GoogleWorkspace` = Drive/Docs/Calendar;
// `MicrosoftGraph` = Outlook/OneDrive/SharePoint/Teams). Kinds with no
// transport family (notion, git_hub, figma, …) collapse to "Other" so
// ingestion always deserializes rather than 400-ing on an unknown tag.
// The Google/Microsoft sibling kinds below aren't all in the admin SPA's
// ConnectorKind union yet, but they're enumerated ConnectorKindTag
// variants — mapping them now means they resolve correctly the moment
// they're offered, without another round here. (The evidence store keeps
// a finer opaque tag in its source_ref column via Rust's
// connector_source_tag; that's a different column with no enum
// constraint, so it is intentionally not reused here.)
//
// Note: this fallback only matters once fetch_content is implemented —
// today it returns 501 and runPipeline short-circuits before reaching
// the ingest path. Passing reg.Kind verbatim (snake_case) would have
// failed SourceKind deserialization for every kind once that lands.
func sourceKindForConnector(kind string) string {
	switch kind {
	case "google_drive", "google_docs", "google_sheets", "google_calendar", "google_meet":
		return "GoogleWorkspace"
	case "one_drive", "share_point", "teams":
		return "MicrosoftGraph"
	case "slack":
		return "Slack"
	case "jira", "confluence":
		return "Atlassian"
	case "hub_spot":
		return "HubSpot"
	case "email":
		return "Email"
	default:
		return "Other"
	}
}

// isNotImplemented reports whether err is an upstream HTTP 501.
func isNotImplemented(err error) bool {
	var apiErr *httpx.Error
	return errors.As(err, &apiErr) && apiErr.Status == http.StatusNotImplemented
}
