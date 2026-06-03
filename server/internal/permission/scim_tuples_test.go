package permission

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"testing"

	"github.com/kennguy3n/knowledge/server/internal/substrate"
)

// Member UUIDs used across the SCIM→tuple tests.
const (
	memberA = "33333333-3333-3333-3333-333333333333"
	memberB = "44444444-4444-4444-4444-444444444444"
	memberC = "55555555-5555-5555-5555-555555555555"
)

// seqChecker records grant/revoke calls and can be configured to fail
// the Nth grant (1-indexed) to exercise rollback.
type seqChecker struct {
	granted, revoked []substrate.RelationTuple
	failGrantOn      int
	grantCount       int
}

func (f *seqChecker) PermissionGrant(_ context.Context, t substrate.RelationTuple) error {
	f.grantCount++
	if f.failGrantOn != 0 && f.grantCount == f.failGrantOn {
		return errors.New("substrate down")
	}
	f.granted = append(f.granted, t)
	return nil
}

func (f *seqChecker) PermissionRevoke(_ context.Context, t substrate.RelationTuple) error {
	f.revoked = append(f.revoked, t)
	return nil
}

func (f *seqChecker) PermissionCheck(_ context.Context, _ substrate.RelationTuple) (bool, error) {
	return false, nil
}

// createGroup posts a SCIM group and returns its server-assigned id.
func createGroup(t *testing.T, h http.Handler, body string) string {
	t.Helper()
	rec := scimDo(t, h, http.MethodPost, "/Groups", body)
	if rec.Code != http.StatusCreated {
		t.Fatalf("create group code = %d body=%s", rec.Code, rec.Body.String())
	}
	var g Group
	if err := json.Unmarshal(rec.Body.Bytes(), &g); err != nil {
		t.Fatal(err)
	}
	return g.ID
}

// createUser posts a SCIM user and returns its server-assigned id.
func createUser(t *testing.T, h http.Handler, userName string) string {
	t.Helper()
	rec := scimDo(t, h, http.MethodPost, "/Users", `{"userName":"`+userName+`"}`)
	if rec.Code != http.StatusCreated {
		t.Fatalf("create user code = %d body=%s", rec.Code, rec.Body.String())
	}
	var u User
	if err := json.Unmarshal(rec.Body.Bytes(), &u); err != nil {
		t.Fatal(err)
	}
	return u.ID
}

func wantMemberTuple(t *testing.T, got substrate.RelationTuple, groupID, userID string) {
	t.Helper()
	if got.Object.ObjectType != "group" || got.Object.ObjectID != groupID {
		t.Fatalf("object = %+v, want group:%s", got.Object, groupID)
	}
	if got.Relation != "member" {
		t.Fatalf("relation = %q, want member", got.Relation)
	}
	if got.Subject.SubjectType != "user" || got.Subject.SubjectID != userID {
		t.Fatalf("subject = %+v, want user:%s", got.Subject, userID)
	}
}

func TestSCIMGroupMembershipGranted(t *testing.T) {
	t.Parallel()
	fc := &seqChecker{}
	h := New(fc).SCIMRoutes()

	gid := createGroup(t, h, `{"displayName":"eng","members":[{"value":"`+memberA+`"},{"value":"`+memberB+`"},{"value":"u1"}]}`)

	if len(fc.granted) != 2 {
		t.Fatalf("granted %d tuples, want 2 (non-UUID member must be skipped): %+v", len(fc.granted), fc.granted)
	}
	if len(fc.revoked) != 0 {
		t.Fatalf("unexpected revokes: %+v", fc.revoked)
	}
	for _, g := range fc.granted {
		if g.Object.ObjectID != gid {
			t.Fatalf("grant for wrong group: %+v", g)
		}
		if g.Subject.SubjectID != memberA && g.Subject.SubjectID != memberB {
			t.Fatalf("grant for unexpected subject: %+v", g)
		}
	}
}

func TestSCIMGroupReplaceDelta(t *testing.T) {
	t.Parallel()
	fc := &seqChecker{}
	h := New(fc).SCIMRoutes()

	gid := createGroup(t, h, `{"displayName":"eng","members":[{"value":"`+memberA+`"},{"value":"`+memberB+`"}]}`)
	fc.granted = nil // reset; assert only the delta

	// Drop B, add C; keep A.
	rec := scimDo(t, h, http.MethodPut, "/Groups/"+gid,
		`{"displayName":"eng","members":[{"value":"`+memberA+`"},{"value":"`+memberC+`"}]}`)
	if rec.Code != http.StatusOK {
		t.Fatalf("replace code = %d body=%s", rec.Code, rec.Body.String())
	}
	if len(fc.granted) != 1 {
		t.Fatalf("granted %d, want 1 (C added): %+v", len(fc.granted), fc.granted)
	}
	wantMemberTuple(t, fc.granted[0], gid, memberC)
	if len(fc.revoked) != 1 {
		t.Fatalf("revoked %d, want 1 (B removed): %+v", len(fc.revoked), fc.revoked)
	}
	wantMemberTuple(t, fc.revoked[0], gid, memberB)
}

func TestSCIMDeleteGroupRevokesAll(t *testing.T) {
	t.Parallel()
	fc := &seqChecker{}
	h := New(fc).SCIMRoutes()

	gid := createGroup(t, h, `{"displayName":"eng","members":[{"value":"`+memberA+`"},{"value":"`+memberB+`"}]}`)

	rec := scimDo(t, h, http.MethodDelete, "/Groups/"+gid, "")
	if rec.Code != http.StatusNoContent {
		t.Fatalf("delete code = %d", rec.Code)
	}
	if len(fc.revoked) != 2 {
		t.Fatalf("revoked %d, want 2: %+v", len(fc.revoked), fc.revoked)
	}
}

func TestSCIMDeleteUserRevokesMemberships(t *testing.T) {
	t.Parallel()
	fc := &seqChecker{}
	h := New(fc).SCIMRoutes()

	uid := createUser(t, h, "alice@example.com")
	gid := createGroup(t, h, `{"displayName":"eng","members":[{"value":"`+uid+`"}]}`)
	if len(fc.granted) != 1 {
		t.Fatalf("expected 1 grant for active member, got %+v", fc.granted)
	}

	rec := scimDo(t, h, http.MethodDelete, "/Users/"+uid, "")
	if rec.Code != http.StatusNoContent {
		t.Fatalf("delete user code = %d", rec.Code)
	}
	if len(fc.revoked) != 1 {
		t.Fatalf("revoked %d, want 1: %+v", len(fc.revoked), fc.revoked)
	}
	wantMemberTuple(t, fc.revoked[0], gid, uid)

	// The deleted user must also be stripped from the group's member list.
	rec = scimDo(t, h, http.MethodGet, "/Groups/"+gid, "")
	var g Group
	if err := json.Unmarshal(rec.Body.Bytes(), &g); err != nil {
		t.Fatal(err)
	}
	for _, m := range g.Members {
		if m.Value == uid {
			t.Fatalf("deleted user still in group members: %+v", g.Members)
		}
	}
}

func TestSCIMUserDeactivateReactivate(t *testing.T) {
	t.Parallel()
	fc := &seqChecker{}
	h := New(fc).SCIMRoutes()

	uid := createUser(t, h, "alice@example.com")
	gid := createGroup(t, h, `{"displayName":"eng","members":[{"value":"`+uid+`"}]}`)
	if len(fc.granted) != 1 {
		t.Fatalf("expected 1 grant on group create, got %+v", fc.granted)
	}
	fc.granted = nil

	// Deactivate → membership tuple revoked.
	rec := scimDo(t, h, http.MethodPut, "/Users/"+uid, `{"userName":"alice@example.com","active":false}`)
	if rec.Code != http.StatusOK {
		t.Fatalf("deactivate code = %d", rec.Code)
	}
	if len(fc.revoked) != 1 {
		t.Fatalf("revoked %d on deactivate, want 1: %+v", len(fc.revoked), fc.revoked)
	}
	wantMemberTuple(t, fc.revoked[0], gid, uid)

	// Reactivate → membership tuple re-granted.
	rec = scimDo(t, h, http.MethodPut, "/Users/"+uid, `{"userName":"alice@example.com","active":true}`)
	if rec.Code != http.StatusOK {
		t.Fatalf("reactivate code = %d", rec.Code)
	}
	if len(fc.granted) != 1 {
		t.Fatalf("granted %d on reactivate, want 1: %+v", len(fc.granted), fc.granted)
	}
	wantMemberTuple(t, fc.granted[0], gid, uid)
}

func TestSCIMInactiveMemberNotGranted(t *testing.T) {
	t.Parallel()
	fc := &seqChecker{}
	h := New(fc).SCIMRoutes()

	uid := createUser(t, h, "bob@example.com")
	// Deactivate before any group membership (no tuples yet).
	rec := scimDo(t, h, http.MethodPut, "/Users/"+uid, `{"userName":"bob@example.com","active":false}`)
	if rec.Code != http.StatusOK {
		t.Fatalf("deactivate code = %d", rec.Code)
	}
	if len(fc.revoked) != 0 || len(fc.granted) != 0 {
		t.Fatalf("no tuple ops expected for member-less deactivate: g=%+v r=%+v", fc.granted, fc.revoked)
	}

	// Add the inactive user to a group → no grant (tuple created only on reactivation).
	gid := createGroup(t, h, `{"displayName":"eng","members":[{"value":"`+uid+`"}]}`)
	if len(fc.granted) != 0 {
		t.Fatalf("inactive member must not be granted: %+v", fc.granted)
	}

	// Reactivate → tuple is granted across the user's groups.
	rec = scimDo(t, h, http.MethodPut, "/Users/"+uid, `{"userName":"bob@example.com","active":true}`)
	if rec.Code != http.StatusOK {
		t.Fatalf("reactivate code = %d", rec.Code)
	}
	if len(fc.granted) != 1 {
		t.Fatalf("granted %d on reactivate, want 1: %+v", len(fc.granted), fc.granted)
	}
	wantMemberTuple(t, fc.granted[0], gid, uid)
}

func TestSCIMNonUUIDMembersIgnored(t *testing.T) {
	t.Parallel()
	fc := &seqChecker{}
	h := New(fc).SCIMRoutes()

	createGroup(t, h, `{"displayName":"eng","members":[{"value":"u1"},{"value":"alice"}]}`)
	if len(fc.granted) != 0 || len(fc.revoked) != 0 {
		t.Fatalf("non-UUID members must not produce tuple ops: g=%+v r=%+v", fc.granted, fc.revoked)
	}
}

func TestSCIMGroupCreateTupleFailureRollsBack(t *testing.T) {
	t.Parallel()
	// Fail the 2nd grant; the 1st grant must be rolled back (revoked) and
	// the group must not be persisted.
	fc := &seqChecker{failGrantOn: 2}
	h := New(fc).SCIMRoutes()

	rec := scimDo(t, h, http.MethodPost, "/Groups",
		`{"displayName":"eng","members":[{"value":"`+memberA+`"},{"value":"`+memberB+`"}]}`)
	if rec.Code != http.StatusInternalServerError {
		t.Fatalf("want 500 on tuple failure, got %d body=%s", rec.Code, rec.Body.String())
	}
	if len(fc.granted) != 1 {
		t.Fatalf("expected exactly 1 successful grant before failure, got %+v", fc.granted)
	}
	if len(fc.revoked) != 1 {
		t.Fatalf("expected the successful grant to be rolled back (1 revoke), got %+v", fc.revoked)
	}
	wantMemberTuple(t, fc.revoked[0], fc.granted[0].Object.ObjectID, fc.granted[0].Subject.SubjectID)

	// Group must not have been persisted.
	rec = scimDo(t, h, http.MethodGet, "/Groups", "")
	var lr listResponse
	if err := json.Unmarshal(rec.Body.Bytes(), &lr); err != nil {
		t.Fatal(err)
	}
	if lr.TotalResults != 0 {
		t.Fatalf("group must not persist after tuple failure, got %d groups", lr.TotalResults)
	}
}
