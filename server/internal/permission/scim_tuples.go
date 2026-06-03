package permission

import (
	"context"

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
)

// groupMemberTuple builds the relation tuple for a (group, user)
// membership.
func groupMemberTuple(groupID, userID string) substrate.RelationTuple {
	return substrate.RelationTuple{
		Object:   substrate.ObjectRef{ObjectType: groupObjectType, ObjectID: groupID},
		Relation: groupMemberRelation,
		Subject:  substrate.SubjectRef{SubjectType: userSubjectType, SubjectID: userID},
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
