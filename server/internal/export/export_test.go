package export

import (
	"context"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/kennguy3n/knowledge/server/internal/audit"
	"github.com/kennguy3n/knowledge/server/internal/substrate"
)

const (
	scopeUUID  = "33333333-3333-3333-3333-333333333333"
	tenantUUID = "44444444-4444-4444-4444-444444444444"
)

type fakeExporter struct {
	decision substrate.ExportDecision
	err      error
	gotReq   substrate.ExportEvaluateRequest
}

func (f *fakeExporter) ExportEvaluate(_ context.Context, req substrate.ExportEvaluateRequest) (substrate.ExportDecision, error) {
	f.gotReq = req
	return f.decision, f.err
}

type fakeRecorder struct{ events []audit.Event }

func (f *fakeRecorder) Record(_ context.Context, e audit.Event) (audit.Event, error) {
	f.events = append(f.events, e)
	return e, nil
}

func decision() substrate.ExportDecision {
	return substrate.ExportDecision{
		Approved: []substrate.ApprovedConcept{
			{ConceptID: "c1", Label: "Quarterly Revenue", Definition: "money", SensitivityClass: "high"},
		},
		Rejected:         []substrate.ExportRejection{{ConceptID: "c2"}},
		Warnings:         []string{"one concept rejected"},
		AllowRawEvidence: false,
	}
}

func TestExportValidation(t *testing.T) {
	t.Parallel()
	s := New(&fakeExporter{}, &fakeRecorder{})
	cases := []ProfileRequest{
		{ScopeID: "bad", TenantID: tenantUUID, Profile: []byte(`{}`)},
		{ScopeID: scopeUUID, TenantID: "bad", Profile: []byte(`{}`)},
		{ScopeID: scopeUUID, TenantID: tenantUUID},
	}
	for i, c := range cases {
		if _, err := s.Export(context.Background(), c); err == nil {
			t.Fatalf("case %d: expected validation error", i)
		}
	}
}

func TestExportEnforcesPolicyAndAudits(t *testing.T) {
	t.Parallel()
	exp := &fakeExporter{decision: decision()}
	rec := &fakeRecorder{}
	s := New(exp, rec)
	pack, err := s.Export(context.Background(), ProfileRequest{
		ScopeID:  scopeUUID,
		TenantID: tenantUUID,
		Profile:  []byte(`{"concepts":[]}`),
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(pack.Approved) != 1 || pack.RejectedCount != 1 || !pack.RawEvidenceOmitted {
		t.Fatalf("policy enforcement wrong: %+v", pack)
	}
	if len(rec.events) != 1 || rec.events[0].Action != "export.profile" {
		t.Fatalf("audit not recorded: %+v", rec.events)
	}
}

func TestHandleProfileFormats(t *testing.T) {
	t.Parallel()
	body := `{"scope_id":"` + scopeUUID + `","tenant_id":"` + tenantUUID + `","format":%q,"profile":{"x":1}}`

	for _, tc := range []struct {
		format      string
		contentType string
		contains    string
	}{
		{"json", "application/json", `"approved"`},
		{"markdown", "text/markdown", "# Concept Profile Export"},
		{"html", "text/html", "<h1>Concept Profile Export</h1>"},
	} {
		s := New(&fakeExporter{decision: decision()}, &fakeRecorder{})
		rec := httptest.NewRecorder()
		req := httptest.NewRequest(http.MethodPost, "/profile",
			strings.NewReader(strings.Replace(body, "%q", `"`+tc.format+`"`, 1)))
		s.Routes().ServeHTTP(rec, req)
		if rec.Code != http.StatusOK {
			t.Fatalf("%s: code = %d body=%s", tc.format, rec.Code, rec.Body.String())
		}
		if ct := rec.Header().Get("Content-Type"); !strings.Contains(ct, tc.contentType) {
			t.Fatalf("%s: content-type = %q", tc.format, ct)
		}
		if !strings.Contains(rec.Body.String(), tc.contains) {
			t.Fatalf("%s: body missing %q: %s", tc.format, tc.contains, rec.Body.String())
		}
	}
}

func TestRenderHTMLEscapes(t *testing.T) {
	t.Parallel()
	pack := EvidencePack{
		Approved: []substrate.ApprovedConcept{{Label: "<script>alert(1)</script>", SensitivityClass: "x"}},
		Warnings: []string{"<b>warn</b>"},
	}
	out := renderHTML(pack)
	if strings.Contains(out, "<script>alert(1)</script>") || strings.Contains(out, "<b>warn</b>") {
		t.Fatalf("html not escaped: %s", out)
	}
	if !strings.Contains(out, "&lt;script&gt;") {
		t.Fatalf("expected escaped entity: %s", out)
	}
}
