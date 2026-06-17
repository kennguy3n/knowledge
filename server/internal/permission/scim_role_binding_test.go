package permission

import (
	"net/http"
	"testing"

	"github.com/kennguy3n/knowledge/server/internal/substrate"
)

// Tenant UUIDs used across the role-binding tests.
const (
	tenantUUID  = "11111111-1111-1111-1111-111111111111"
	tenantUUID2 = "22222222-2222-2222-2222-222222222222"
)

func roleName(tenantID, role string) string {
	return groupRoleNamespace + ":" + tenantObjectType + ":" + tenantID + ":" + role
}

// roleTuples returns the tenant role-binding tuples (object type
// "tenant") from a list, ignoring group-membership tuples.
func roleTuples(tuples []substrate.RelationTuple) []substrate.RelationTuple {
	var out []substrate.RelationTuple
	for _, x := range tuples {
		if x.Object.ObjectType == tenantObjectType {
			out = append(out, x)
		}
	}
	return out
}

func wantRoleTuple(t *testing.T, got substrate.RelationTuple, tenantID, role, groupID string) {
	t.Helper()
	if got.Object.ObjectType != "tenant" || got.Object.ObjectID != tenantID {
		t.Fatalf("object = %+v, want tenant:%s", got.Object, tenantID)
	}
	if got.Relation != role {
		t.Fatalf("relation = %q, want %q", got.Relation, role)
	}
	if got.Subject.SubjectType != "group" || got.Subject.SubjectID != groupID {
		t.Fatalf("subject = %+v, want group:%s", got.Subject, groupID)
	}
	if got.Subject.SubjectRelation == nil || *got.Subject.SubjectRelation != "member" {
		t.Fatalf("subject_relation = %v, want member", got.Subject.SubjectRelation)
	}
}

func wantRoleTupleOp(t *testing.T, op tupleOp, grant bool, tenantID, role, groupID string) {
	t.Helper()
	if op.grant != grant {
		t.Fatalf("op.grant = %v, want %v (tuple=%+v)", op.grant, grant, op.tuple)
	}
	wantRoleTuple(t, op.tuple, tenantID, role, groupID)
}

func TestParseGroupRole(t *testing.T) {
	t.Parallel()
	cases := []struct {
		name        string
		displayName string
		wantTenant  string
		wantRole    string
		wantOK      bool
	}{
		{"admin binding", roleName(tenantUUID, "admin"), tenantUUID, "admin", true},
		{"editor binding", roleName(tenantUUID, "editor"), tenantUUID, "editor", true},
		{"member binding", roleName(tenantUUID, "member"), tenantUUID, "member", true},
		{"viewer binding", roleName(tenantUUID, "viewer"), tenantUUID, "viewer", true},
		{"owner excluded", roleName(tenantUUID, "owner"), "", "", false},
		{"synthesizer excluded", roleName(tenantUUID, "synthesizer"), "", "", false},
		{"proposer excluded", roleName(tenantUUID, "proposer"), "", "", false},
		{"unknown role", roleName(tenantUUID, "superuser"), "", "", false},
		{"uppercase role rejected", roleName(tenantUUID, "Admin"), "", "", false},
		{"bad uuid", "knowledge:tenant:not-a-uuid:admin", "", "", false},
		{"wrong namespace", "other:tenant:" + tenantUUID + ":admin", "", "", false},
		{"wrong object", "knowledge:scope:" + tenantUUID + ":admin", "", "", false},
		{"too few segments", "knowledge:tenant:" + tenantUUID, "", "", false},
		{"too many segments", roleName(tenantUUID, "admin") + ":extra", "", "", false},
		{"plain name", "eng", "", "", false},
		{"empty", "", "", "", false},
	}
	for _, tc := range cases {
		tc := tc
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			gotTenant, gotRole, gotOK := parseGroupRole(tc.displayName)
			if gotOK != tc.wantOK || gotTenant != tc.wantTenant || gotRole != tc.wantRole {
				t.Fatalf("parseGroupRole(%q) = (%q,%q,%v), want (%q,%q,%v)",
					tc.displayName, gotTenant, gotRole, gotOK, tc.wantTenant, tc.wantRole, tc.wantOK)
			}
		})
	}
}

func TestGroupRoleReconcileOps(t *testing.T) {
	t.Parallel()
	const gid = "99999999-9999-9999-9999-999999999999"
	adminName := roleName(tenantUUID, "admin")
	viewerName := roleName(tenantUUID, "viewer")
	otherTenantAdmin := roleName(tenantUUID2, "admin")

	t.Run("adopt on create", func(t *testing.T) {
		ops := groupRoleReconcileOps("", adminName, gid)
		if len(ops) != 1 {
			t.Fatalf("want 1 op, got %+v", ops)
		}
		wantRoleTupleOp(t, ops[0], true, tenantUUID, "admin", gid)
	})
	t.Run("revoke on delete", func(t *testing.T) {
		ops := groupRoleReconcileOps(adminName, "", gid)
		if len(ops) != 1 {
			t.Fatalf("want 1 op, got %+v", ops)
		}
		wantRoleTupleOp(t, ops[0], false, tenantUUID, "admin", gid)
	})
	t.Run("re-point grants new before revoking old", func(t *testing.T) {
		ops := groupRoleReconcileOps(adminName, viewerName, gid)
		if len(ops) != 2 {
			t.Fatalf("want 2 ops, got %+v", ops)
		}
		wantRoleTupleOp(t, ops[0], true, tenantUUID, "viewer", gid)
		wantRoleTupleOp(t, ops[1], false, tenantUUID, "admin", gid)
	})
	t.Run("re-point across tenants", func(t *testing.T) {
		ops := groupRoleReconcileOps(adminName, otherTenantAdmin, gid)
		if len(ops) != 2 {
			t.Fatalf("want 2 ops, got %+v", ops)
		}
		wantRoleTupleOp(t, ops[0], true, tenantUUID2, "admin", gid)
		wantRoleTupleOp(t, ops[1], false, tenantUUID, "admin", gid)
	})
	t.Run("unchanged binding is a no-op", func(t *testing.T) {
		if ops := groupRoleReconcileOps(adminName, adminName, gid); ops != nil {
			t.Fatalf("want no ops, got %+v", ops)
		}
	})
	t.Run("non-matching to non-matching is a no-op", func(t *testing.T) {
		if ops := groupRoleReconcileOps("eng", "platform", gid); ops != nil {
			t.Fatalf("want no ops, got %+v", ops)
		}
	})
	t.Run("drop convention revokes", func(t *testing.T) {
		ops := groupRoleReconcileOps(adminName, "eng", gid)
		if len(ops) != 1 {
			t.Fatalf("want 1 op, got %+v", ops)
		}
		wantRoleTupleOp(t, ops[0], false, tenantUUID, "admin", gid)
	})
	t.Run("adopt convention grants", func(t *testing.T) {
		ops := groupRoleReconcileOps("eng", adminName, gid)
		if len(ops) != 1 {
			t.Fatalf("want 1 op, got %+v", ops)
		}
		wantRoleTupleOp(t, ops[0], true, tenantUUID, "admin", gid)
	})
	t.Run("owner name produces no binding", func(t *testing.T) {
		if ops := groupRoleReconcileOps("", roleName(tenantUUID, "owner"), gid); ops != nil {
			t.Fatalf("owner must not bind, got %+v", ops)
		}
	})
}

func TestSCIMGroupRoleBindingGrantedOnCreate(t *testing.T) {
	t.Parallel()
	fc := &seqChecker{}
	h := New(fc).SCIMRoutes()
	gid := createGroup(t, h, `{"displayName":"`+roleName(tenantUUID, "admin")+`","members":[{"value":"`+memberA+`"}]}`)

	roles := roleTuples(fc.granted)
	if len(roles) != 1 {
		t.Fatalf("want 1 role-binding grant, got %+v (all=%+v)", roles, fc.granted)
	}
	wantRoleTuple(t, roles[0], tenantUUID, "admin", gid)
	if len(roleTuples(fc.revoked)) != 0 {
		t.Fatalf("unexpected role revokes: %+v", fc.revoked)
	}
}

func TestSCIMGroupRoleBindingRenameRepoints(t *testing.T) {
	t.Parallel()
	fc := &seqChecker{}
	h := New(fc).SCIMRoutes()
	gid := createGroup(t, h, `{"displayName":"`+roleName(tenantUUID, "admin")+`","members":[{"value":"`+memberA+`"}]}`)
	fc.granted, fc.revoked = nil, nil

	// Rename the DisplayName only; members are identical, so the only
	// reconciliation is the role-binding re-point.
	rec := scimDo(t, h, http.MethodPut, "/Groups/"+gid,
		`{"displayName":"`+roleName(tenantUUID, "viewer")+`","members":[{"value":"`+memberA+`"}]}`)
	if rec.Code != http.StatusOK {
		t.Fatalf("replace code = %d body=%s", rec.Code, rec.Body.String())
	}
	if len(fc.granted) != 1 || len(fc.revoked) != 1 {
		t.Fatalf("want exactly 1 grant + 1 revoke on a pure rename, got grant=%+v revoke=%+v", fc.granted, fc.revoked)
	}
	wantRoleTuple(t, fc.granted[0], tenantUUID, "viewer", gid)
	wantRoleTuple(t, fc.revoked[0], tenantUUID, "admin", gid)
}

func TestSCIMGroupRoleBindingRevokedOnDelete(t *testing.T) {
	t.Parallel()
	fc := &seqChecker{}
	h := New(fc).SCIMRoutes()
	gid := createGroup(t, h, `{"displayName":"`+roleName(tenantUUID, "editor")+`","members":[{"value":"`+memberA+`"}]}`)
	fc.granted, fc.revoked = nil, nil

	rec := scimDo(t, h, http.MethodDelete, "/Groups/"+gid, "")
	if rec.Code != http.StatusNoContent {
		t.Fatalf("delete code = %d", rec.Code)
	}
	roles := roleTuples(fc.revoked)
	if len(roles) != 1 {
		t.Fatalf("want 1 role-binding revoke, got %+v", fc.revoked)
	}
	wantRoleTuple(t, roles[0], tenantUUID, "editor", gid)
}

func TestSCIMGroupNonMatchingNameNoBinding(t *testing.T) {
	t.Parallel()
	fc := &seqChecker{}
	h := New(fc).SCIMRoutes()
	createGroup(t, h, `{"displayName":"engineering","members":[{"value":"`+memberA+`"}]}`)
	if len(roleTuples(fc.granted)) != 0 || len(roleTuples(fc.revoked)) != 0 {
		t.Fatalf("non-matching DisplayName must not bind a role: g=%+v r=%+v", fc.granted, fc.revoked)
	}
}

func TestSCIMGroupOwnerOrInvalidRoleNoBinding(t *testing.T) {
	t.Parallel()
	for _, name := range []string{
		roleName(tenantUUID, "owner"),
		roleName(tenantUUID, "superuser"),
		"knowledge:tenant:not-a-uuid:admin",
	} {
		fc := &seqChecker{}
		h := New(fc).SCIMRoutes()
		createGroup(t, h, `{"displayName":"`+name+`","members":[{"value":"`+memberA+`"}]}`)
		if len(roleTuples(fc.granted)) != 0 {
			t.Fatalf("DisplayName %q must not bind a role, got %+v", name, fc.granted)
		}
	}
}
