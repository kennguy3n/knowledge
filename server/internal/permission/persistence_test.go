package permission

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"sync"
	"testing"
)

// memDirStore is an in-process [DirectoryStore] used to assert
// write-through and rehydration behaviour. The *Err fields, when set,
// force the corresponding operation to fail.
type memDirStore struct {
	mu        sync.Mutex
	users     map[string]User
	groups    map[string]Group
	saveUErr  error
	saveGErr  error
	delUErr   error
	delGErr   error
	listUErr  error
	listGErr  error
	saveUsers int
	saveGrps  int
}

func newMemDirStore() *memDirStore {
	return &memDirStore{users: make(map[string]User), groups: make(map[string]Group)}
}

func (m *memDirStore) SaveUser(_ context.Context, u User) error {
	if m.saveUErr != nil {
		return m.saveUErr
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	m.users[u.ID] = u
	m.saveUsers++
	return nil
}

func (m *memDirStore) SaveGroup(_ context.Context, g Group) error {
	if m.saveGErr != nil {
		return m.saveGErr
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	m.groups[g.ID] = g
	m.saveGrps++
	return nil
}

func (m *memDirStore) DeleteUser(_ context.Context, userID string, updated []Group) error {
	if m.delUErr != nil {
		return m.delUErr
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	delete(m.users, userID)
	for _, g := range updated {
		m.groups[g.ID] = g
	}
	return nil
}

func (m *memDirStore) DeleteGroup(_ context.Context, groupID string) error {
	if m.delGErr != nil {
		return m.delGErr
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	delete(m.groups, groupID)
	return nil
}

func (m *memDirStore) ListUsers(context.Context) ([]User, error) {
	if m.listUErr != nil {
		return nil, m.listUErr
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	out := make([]User, 0, len(m.users))
	for _, u := range m.users {
		out = append(out, u)
	}
	return out, nil
}

func (m *memDirStore) ListGroups(context.Context) ([]Group, error) {
	if m.listGErr != nil {
		return nil, m.listGErr
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	out := make([]Group, 0, len(m.groups))
	for _, g := range m.groups {
		out = append(out, g)
	}
	return out, nil
}

func (m *memDirStore) hasUser(id string) bool {
	m.mu.Lock()
	defer m.mu.Unlock()
	_, ok := m.users[id]
	return ok
}

func (m *memDirStore) hasGroup(id string) bool {
	m.mu.Lock()
	defer m.mu.Unlock()
	_, ok := m.groups[id]
	return ok
}

// svcWithDir builds a Service with a fake substrate and a durable store.
func svcWithDir(sub checker, ds DirectoryStore) *Service {
	return New(sub).WithDirectoryStore(ds)
}

func TestCreateUserPersistsThroughStore(t *testing.T) {
	t.Parallel()
	ds := newMemDirStore()
	h := svcWithDir(&seqChecker{}, ds).SCIMRoutes()

	id := createUser(t, h, "alice")
	if !ds.hasUser(id) {
		t.Fatal("user not persisted through directory store on create")
	}
}

func TestCreateGroupPersistsThroughStore(t *testing.T) {
	t.Parallel()
	ds := newMemDirStore()
	h := svcWithDir(&seqChecker{}, ds).SCIMRoutes()

	gid := createGroup(t, h, `{"displayName":"eng"}`)
	if !ds.hasGroup(gid) {
		t.Fatal("group not persisted through directory store on create")
	}
}

func TestCreateUserPersistFailureReturns500(t *testing.T) {
	t.Parallel()
	ds := newMemDirStore()
	ds.saveUErr = errors.New("db down")
	svc := svcWithDir(&seqChecker{}, ds)
	h := svc.SCIMRoutes()

	rec := scimDo(t, h, http.MethodPost, "/Users", `{"userName":"bob"}`)
	if rec.Code != http.StatusInternalServerError {
		t.Fatalf("create code = %d, want 500", rec.Code)
	}
	// The cache must not retain a user whose durable write failed.
	svc.dir.mu.RLock()
	n := len(svc.dir.users)
	svc.dir.mu.RUnlock()
	if n != 0 {
		t.Fatalf("cache has %d users after persist failure, want 0", n)
	}
}

func TestCreateGroupPersistFailureCompensatesTuples(t *testing.T) {
	t.Parallel()
	ds := newMemDirStore()
	ds.saveGErr = errors.New("db down")
	fc := &seqChecker{}
	h := svcWithDir(fc, ds).SCIMRoutes()

	body := `{"displayName":"eng","members":[{"value":"` + memberA + `"}]}`
	rec := scimDo(t, h, http.MethodPost, "/Groups", body)
	if rec.Code != http.StatusInternalServerError {
		t.Fatalf("create group code = %d, want 500", rec.Code)
	}
	// The membership grant applied before the persist attempt must be
	// compensated (revoked) so no orphaned tuple survives the failure.
	if len(fc.granted) != 1 {
		t.Fatalf("granted = %d, want 1", len(fc.granted))
	}
	if len(fc.revoked) != 1 {
		t.Fatalf("compensating revoked = %d, want 1", len(fc.revoked))
	}
}

// TestDeleteUserPersistFailureRestoresTuples verifies the revoked
// membership tuples are re-granted when the durable delete fails, so the
// substrate stays consistent with the still-present user.
func TestDeleteUserPersistFailureRestoresTuples(t *testing.T) {
	t.Parallel()
	ds := newMemDirStore()
	fc := &seqChecker{}
	h := svcWithDir(fc, ds).SCIMRoutes()

	uid := createUser(t, h, "erin")
	createGroup(t, h, `{"displayName":"eng","members":[{"value":"`+uid+`"}]}`) // grant #1
	ds.delUErr = errors.New("db down")

	rec := scimDo(t, h, http.MethodDelete, "/Users/"+uid, "")
	if rec.Code != http.StatusInternalServerError {
		t.Fatalf("delete code = %d, want 500", rec.Code)
	}
	// One revoke (removal) then one compensating re-grant => 2 grants total.
	if len(fc.revoked) != 1 {
		t.Fatalf("revoked = %d, want 1", len(fc.revoked))
	}
	if len(fc.granted) != 2 {
		t.Fatalf("granted = %d, want 2 (initial + compensating re-grant)", len(fc.granted))
	}
}

// TestDeleteGroupPersistFailureRestoresTuples verifies the revoked
// membership tuples are re-granted when the durable group delete fails.
func TestDeleteGroupPersistFailureRestoresTuples(t *testing.T) {
	t.Parallel()
	ds := newMemDirStore()
	fc := &seqChecker{}
	h := svcWithDir(fc, ds).SCIMRoutes()

	gid := createGroup(t, h, `{"displayName":"eng","members":[{"value":"`+memberA+`"}]}`) // grant #1
	ds.delGErr = errors.New("db down")

	rec := scimDo(t, h, http.MethodDelete, "/Groups/"+gid, "")
	if rec.Code != http.StatusInternalServerError {
		t.Fatalf("delete code = %d, want 500", rec.Code)
	}
	if len(fc.revoked) != 1 {
		t.Fatalf("revoked = %d, want 1", len(fc.revoked))
	}
	if len(fc.granted) != 2 {
		t.Fatalf("granted = %d, want 2 (initial + compensating re-grant)", len(fc.granted))
	}
}

// TestReplaceUserDeactivatePersistFailureRestoresTuples verifies that a
// deactivation toggle (a set of revokes) is fully restored when the
// durable write fails — compensateGrants alone would leave it gone.
func TestReplaceUserDeactivatePersistFailureRestoresTuples(t *testing.T) {
	t.Parallel()
	ds := newMemDirStore()
	fc := &seqChecker{}
	h := svcWithDir(fc, ds).SCIMRoutes()

	uid := createUser(t, h, "frank")
	createGroup(t, h, `{"displayName":"eng","members":[{"value":"`+uid+`"}]}`) // grant #1
	ds.saveUErr = errors.New("db down")

	body := `{"userName":"frank","active":false}`
	rec := scimDo(t, h, http.MethodPut, "/Users/"+uid, body)
	if rec.Code != http.StatusInternalServerError {
		t.Fatalf("replace code = %d, want 500", rec.Code)
	}
	// Deactivation revokes the membership tuple (#1 revoke); the persist
	// failure must re-grant it (#2 grant total).
	if len(fc.revoked) != 1 {
		t.Fatalf("revoked = %d, want 1", len(fc.revoked))
	}
	if len(fc.granted) != 2 {
		t.Fatalf("granted = %d, want 2 (initial + restored toggle)", len(fc.granted))
	}
}

func TestDeleteUserPersistsRemoval(t *testing.T) {
	t.Parallel()
	ds := newMemDirStore()
	h := svcWithDir(&seqChecker{}, ds).SCIMRoutes()

	uid := createUser(t, h, "carol")
	gid := createGroup(t, h, `{"displayName":"eng","members":[{"value":"`+uid+`"}]}`)

	rec := scimDo(t, h, http.MethodDelete, "/Users/"+uid, "")
	if rec.Code != http.StatusNoContent {
		t.Fatalf("delete code = %d, want 204", rec.Code)
	}
	if ds.hasUser(uid) {
		t.Fatal("user still persisted after delete")
	}
	// The user must also be stripped from the persisted group membership.
	ds.mu.Lock()
	g := ds.groups[gid]
	ds.mu.Unlock()
	for _, m := range g.Members {
		if m.Value == uid {
			t.Fatal("deleted user still present in persisted group members")
		}
	}
}

func TestDeleteGroupPersistsRemoval(t *testing.T) {
	t.Parallel()
	ds := newMemDirStore()
	h := svcWithDir(&seqChecker{}, ds).SCIMRoutes()

	gid := createGroup(t, h, `{"displayName":"eng"}`)
	rec := scimDo(t, h, http.MethodDelete, "/Groups/"+gid, "")
	if rec.Code != http.StatusNoContent {
		t.Fatalf("delete code = %d, want 204", rec.Code)
	}
	if ds.hasGroup(gid) {
		t.Fatal("group still persisted after delete")
	}
}

// TestRehydrateRestoresDirectory simulates a restart: a fresh Service
// backed by the same durable store must serve the users and groups the
// prior process persisted.
func TestRehydrateRestoresDirectory(t *testing.T) {
	t.Parallel()
	ds := newMemDirStore()
	first := svcWithDir(&seqChecker{}, ds)
	h1 := first.SCIMRoutes()

	uid := createUser(t, h1, "dave")
	gid := createGroup(t, h1, `{"displayName":"eng","members":[{"value":"`+uid+`"}]}`)

	// New process: empty cache, same durable store.
	second := svcWithDir(&seqChecker{}, ds)
	if err := second.Rehydrate(context.Background()); err != nil {
		t.Fatalf("rehydrate: %v", err)
	}
	h2 := second.SCIMRoutes()

	if rec := scimDo(t, h2, http.MethodGet, "/Users/"+uid, ""); rec.Code != http.StatusOK {
		t.Fatalf("get user after rehydrate = %d, want 200", rec.Code)
	}
	rec := scimDo(t, h2, http.MethodGet, "/Groups/"+gid, "")
	if rec.Code != http.StatusOK {
		t.Fatalf("get group after rehydrate = %d, want 200", rec.Code)
	}
	var g Group
	if err := json.Unmarshal(rec.Body.Bytes(), &g); err != nil {
		t.Fatal(err)
	}
	if len(g.Members) != 1 || g.Members[0].Value != uid {
		t.Fatalf("rehydrated group members = %+v, want [%s]", g.Members, uid)
	}
}

func TestRehydratePropagatesListError(t *testing.T) {
	t.Parallel()
	ds := newMemDirStore()
	ds.listUErr = errors.New("db down")
	svc := svcWithDir(&seqChecker{}, ds)
	if err := svc.Rehydrate(context.Background()); err == nil {
		t.Fatal("expected rehydrate to propagate list error")
	}
}

func TestNoopDirectoryStoreRehydrateEmpty(t *testing.T) {
	t.Parallel()
	svc := New(&seqChecker{}) // defaults to noop store
	if err := svc.Rehydrate(context.Background()); err != nil {
		t.Fatalf("noop rehydrate: %v", err)
	}
	rec := scimDo(t, svc.SCIMRoutes(), http.MethodGet, "/Users", "")
	if rec.Code != http.StatusOK {
		t.Fatalf("list users = %d, want 200", rec.Code)
	}
}

// TestConcurrentUserWritesPersist exercises the write-through path under
// the race detector with concurrent creates.
func TestConcurrentUserWritesPersist(t *testing.T) {
	t.Parallel()
	ds := newMemDirStore()
	h := svcWithDir(&seqChecker{}, ds).SCIMRoutes()

	const n = 25
	var wg sync.WaitGroup
	wg.Add(n)
	for i := 0; i < n; i++ {
		go func(i int) {
			defer wg.Done()
			body := `{"userName":"u` + string(rune('a'+i%26)) + `-` + itoa(i) + `"}`
			scimDo(t, h, http.MethodPost, "/Users", body)
		}(i)
	}
	wg.Wait()

	ds.mu.Lock()
	got := len(ds.users)
	ds.mu.Unlock()
	if got != n {
		t.Fatalf("persisted users = %d, want %d", got, n)
	}
}

func itoa(i int) string {
	if i == 0 {
		return "0"
	}
	var b []byte
	for i > 0 {
		b = append([]byte{byte('0' + i%10)}, b...)
		i /= 10
	}
	return string(b)
}
