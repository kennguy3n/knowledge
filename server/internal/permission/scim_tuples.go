package permission

import (
	"context"
	"strings"

	"github.com/google/uuid"
	"go.uber.org/zap"

	"github.com/kennguy3n/knowledge/server/internal/substrate"
)

// SCIM membership → tuple mapping policy.
//
// A SCIM Group whose members reference users by their SCIM user id is
// joined to the authorization substrate by mapping each membership to a
// Zanzibar relation tuple:
//
//	group:<groupID> # member @ user:<userID>
//
// Group-based authorization is then expressed via userset rewrites
// (e.g. a channel viewer check of the form
// `channel:<id># viewer @ group:<gid># member`).
//
// Two invariants govern the mapping:
//
//   - The substrate keys subjects by UUID, so only members whose value
//     is a syntactically valid user id are mapped. Other member entries
//     are retained in the directory but not joined to the tuple store
//     (they cannot name a substrate subject).
//   - A membership tuple exists in the substrate iff the user is a
//     member of the group AND the user is active. Suspending a user
//     therefore removes the group-derived access without dropping the
//     directory record, and reactivating restores it. Users absent from
//     the directory are treated as active, since membership may be
//     provisioned before the user resource.
const (
	groupObjectType     = "group"
	userSubjectType     = "user"
	groupMemberRelation = "member"

	// tenantObjectType is the object side of a group → tenant-role
	// binding tuple (tenant:<id># <role> @ group:<gid># member).
	tenantObjectType = "tenant"

	// groupRoleNamespace is the first segment of a role-binding
	// DisplayName: knowledge:tenant:<tenantUUID>:<role>.
	groupRoleNamespace = "knowledge"
)

// groupRoleBindings is the set of tenant relations a SCIM group may be
// bound to through its DisplayName. owner is intentionally excluded — the
// tenant root must not be bootstrappable from an IdP-controlled group
// name — as are synthesizer and proposer, which are not tenant-hierarchy
// roles. An unrecognised role leaves the group membership-only.
var groupRoleBindings = map[string]struct{}{
	"admin":  {},
	"editor": {},
	"member": {},
	"viewer": {},
}

// groupMemberTuple builds the relation tuple for a (group, user)
// membership.
func groupMemberTuple(groupID, userID string) substrate.RelationTuple {
	return substrate.RelationTuple{
		Object:   substrate.ObjectRef{ObjectType: groupObjectType, ObjectID: groupID},
		Relation: groupMemberRelation,
		Subject:  substrate.SubjectRef{SubjectType: userSubjectType, SubjectID: userID},
	}
}

// parseGroupRole decodes a tenant role binding from a SCIM group's
// DisplayName. A bound group's DisplayName has the exact, colon-delimited
// form
//
//	knowledge:tenant:<tenantUUID>:<role>
//
// Splitting on ':' is unambiguous because a UUID never contains a colon,
// so a match is exactly four segments. Validation is structural: the
// tenant segment must parse as a UUID and the role must be one of the
// allowed tenant relations (see groupRoleBindings). A DisplayName that
// does not match — the common case — yields ok=false and leaves the group
// membership-only, so existing groups are unaffected. Tenant existence is
// deliberately not checked: the directory service holds no tenant store,
// and a binding to an absent tenant grants nothing real (no route
// resolves to it).
func parseGroupRole(displayName string) (tenantID, role string, ok bool) {
	parts := strings.Split(displayName, ":")
	if len(parts) != 4 {
		return "", "", false
	}
	if parts[0] != groupRoleNamespace || parts[1] != tenantObjectType {
		return "", "", false
	}
	if _, err := uuid.Parse(parts[2]); err != nil {
		return "", "", false
	}
	if _, allowed := groupRoleBindings[parts[3]]; !allowed {
		return "", "", false
	}
	return parts[2], parts[3], true
}

// groupRoleTuple builds the relation tuple that binds a tenant role to a
// group's members via a userset rewrite:
//
//	tenant:<tenantID> # <role> @ group:<groupID> # member
//
// Granting it gives every member of the group the role on the tenant —
// the check engine resolves the `# member` rewrite — so role assignment
// tracks group membership with no per-user grants.
func groupRoleTuple(tenantID, role, groupID string) substrate.RelationTuple {
	member := groupMemberRelation
	return substrate.RelationTuple{
		Object:   substrate.ObjectRef{ObjectType: tenantObjectType, ObjectID: tenantID},
		Relation: role,
		Subject: substrate.SubjectRef{
			SubjectType:     groupObjectType,
			SubjectID:       groupID,
			SubjectRelation: &member,
		},
	}
}

// validUserID reports whether a SCIM member value can name a substrate
// subject. Substrate subjects are UUID-keyed, so a member is mappable
// iff its value parses as a UUID. This is deliberately a user-id check
// in its own right (not borrowed from scope-id validation) so it stays
// correct if scope ids ever gain extra constraints.
func validUserID(value string) bool {
	_, err := uuid.Parse(value)
	return err == nil
}

// mappableMemberIDs returns the deduplicated set of member values that
// can name a substrate subject (i.e. are valid user ids).
func mappableMemberIDs(members []groupMember) map[string]struct{} {
	out := make(map[string]struct{}, len(members))
	for _, m := range members {
		if validUserID(m.Value) {
			out[m.Value] = struct{}{}
		}
	}
	return out
}

// tupleOp is a single grant or revoke against the substrate.
type tupleOp struct {
	grant bool
	tuple substrate.RelationTuple
}

// applyTupleOps performs the operations in order. If any operation
// fails, the operations already applied are undone (in reverse) so the
// substrate is never left partially reconciled, and the original error
// is returned. Rollback is best-effort; an undo failure is logged.
func (s *Service) applyTupleOps(ctx context.Context, ops []tupleOp) error {
	applied := make([]tupleOp, 0, len(ops))
	for _, op := range ops {
		if err := s.runTupleOp(ctx, op); err != nil {
			s.rollbackTupleOps(ctx, applied)
			return err
		}
		applied = append(applied, op)
	}
	return nil
}

func (s *Service) runTupleOp(ctx context.Context, op tupleOp) error {
	if op.grant {
		return s.sub.PermissionGrant(ctx, op.tuple)
	}
	return s.sub.PermissionRevoke(ctx, op.tuple)
}

// compensateGrants best-effort revokes every tuple that the given ops
// granted. It undoes a reconciliation whose target resource was found to
// have been concurrently deleted: tuples we added are removed, while
// tuples we revoked are intentionally left gone. Failures are logged.
func (s *Service) compensateGrants(ctx context.Context, ops []tupleOp) {
	for _, op := range ops {
		if !op.grant {
			continue
		}
		if err := s.sub.PermissionRevoke(ctx, op.tuple); err != nil {
			s.log.Warn("scim: compensating revoke failed",
				zap.String("object_id", op.tuple.Object.ObjectID),
				zap.String("subject_id", op.tuple.Subject.SubjectID),
				zap.Error(err))
		}
	}
}

func (s *Service) rollbackTupleOps(ctx context.Context, applied []tupleOp) {
	for i := len(applied) - 1; i >= 0; i-- {
		inverse := tupleOp{grant: !applied[i].grant, tuple: applied[i].tuple}
		if err := s.runTupleOp(ctx, inverse); err != nil {
			s.log.Warn("scim: tuple reconciliation rollback failed",
				zap.Bool("undo_of_grant", applied[i].grant),
				zap.String("object_id", inverse.tuple.Object.ObjectID),
				zap.String("subject_id", inverse.tuple.Subject.SubjectID),
				zap.Error(err))
		}
	}
}

// userActive reports a user's active flag. Users absent from the
// directory are treated as active. The caller must not hold s.dir.mu.
func (s *Service) userActive(userID string) bool {
	s.dir.mu.RLock()
	defer s.dir.mu.RUnlock()
	u, ok := s.dir.users[userID]
	if !ok {
		return true
	}
	return u.Active
}

// groupsContainingUser returns the ids of groups whose member list
// includes userID. The caller must not hold s.dir.mu.
func (s *Service) groupsContainingUser(userID string) []string {
	s.dir.mu.RLock()
	defer s.dir.mu.RUnlock()
	var ids []string
	for gid, g := range s.dir.groups {
		for _, m := range g.Members {
			if m.Value == userID {
				ids = append(ids, gid)
				break
			}
		}
	}
	return ids
}

// groupReconcileOps computes the tuple operations to move a group's
// membership from prev to next. Added active members are granted;
// removed active members are revoked (a tuple only exists for an active
// member, and the substrate's revoke is not idempotent). Inactive
// members are skipped — their tuple is (re)granted on reactivation.
func (s *Service) groupReconcileOps(groupID string, prev, next []groupMember) []tupleOp {
	prevSet := mappableMemberIDs(prev)
	nextSet := mappableMemberIDs(next)
	var ops []tupleOp
	for uid := range nextSet {
		if _, ok := prevSet[uid]; ok {
			continue
		}
		if s.userActive(uid) {
			ops = append(ops, tupleOp{grant: true, tuple: groupMemberTuple(groupID, uid)})
		}
	}
	for uid := range prevSet {
		if _, ok := nextSet[uid]; ok {
			continue
		}
		if s.userActive(uid) {
			ops = append(ops, tupleOp{grant: false, tuple: groupMemberTuple(groupID, uid)})
		}
	}
	return ops
}

// groupRoleReconcileOps computes the tuple operations to move a group's
// tenant role binding from the binding encoded in prevDisplayName to the
// one encoded in nextDisplayName (see parseGroupRole). A rename that
// re-points the binding grants the new tuple before revoking the old, so
// the group's members never lose the role mid-update; dropping the
// convention revokes the binding; adopting it grants the binding; an
// unchanged binding produces no operations. As with membership, the
// binding tuple exists iff prevDisplayName encoded a valid binding, so a
// revoke only ever targets a tuple this layer previously granted.
func groupRoleReconcileOps(prevDisplayName, nextDisplayName, groupID string) []tupleOp {
	prevTenant, prevRole, prevOK := parseGroupRole(prevDisplayName)
	nextTenant, nextRole, nextOK := parseGroupRole(nextDisplayName)
	if prevOK && nextOK && prevTenant == nextTenant && prevRole == nextRole {
		return nil
	}
	var ops []tupleOp
	if nextOK {
		ops = append(ops, tupleOp{grant: true, tuple: groupRoleTuple(nextTenant, nextRole, groupID)})
	}
	if prevOK {
		ops = append(ops, tupleOp{grant: false, tuple: groupRoleTuple(prevTenant, prevRole, groupID)})
	}
	return ops
}

// userActiveToggleOps computes the operations needed when a user's
// active flag flips: every group the user belongs to gains (grant) or
// loses (revoke) the membership tuple.
func (s *Service) userActiveToggleOps(userID string, nowActive bool) []tupleOp {
	gids := s.groupsContainingUser(userID)
	ops := make([]tupleOp, 0, len(gids))
	for _, gid := range gids {
		ops = append(ops, tupleOp{grant: nowActive, tuple: groupMemberTuple(gid, userID)})
	}
	return ops
}

// userRemovalOps computes the revokes needed when an active user is
// deleted: its membership tuple is dropped from every group it belongs
// to. An inactive user has no membership tuples, so nothing is revoked.
func (s *Service) userRemovalOps(userID string) []tupleOp {
	if !s.userActive(userID) {
		return nil
	}
	gids := s.groupsContainingUser(userID)
	ops := make([]tupleOp, 0, len(gids))
	for _, gid := range gids {
		ops = append(ops, tupleOp{grant: false, tuple: groupMemberTuple(gid, userID)})
	}
	return ops
}

// removeMemberValue returns members with any entry whose value equals
// userID removed, and whether a change occurred.
func removeMemberValue(members []groupMember, userID string) ([]groupMember, bool) {
	out := members[:0:0]
	changed := false
	for _, m := range members {
		if m.Value == userID {
			changed = true
			continue
		}
		out = append(out, m)
	}
	return out, changed
}
