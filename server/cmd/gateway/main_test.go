package main

import (
	"context"
	"testing"

	"github.com/kennguy3n/knowledge/server/internal/substrate"
	"github.com/kennguy3n/knowledge/server/internal/tenant"
)

func TestTenantListerAndResolver(t *testing.T) {
	t.Parallel()
	store := tenant.NewMemoryStore()
	keys := stubKeys{}
	svc := tenant.New(store, keys)

	tn, err := svc.Create(context.Background(), tenant.CreateRequest{Name: "acme"})
	if err != nil {
		t.Fatal(err)
	}

	lister := tenantLister{store: store}
	ids, err := lister.TenantIDs(context.Background())
	if err != nil || len(ids) != 1 || ids[0] != tn.ID {
		t.Fatalf("TenantIDs = %v err=%v", ids, err)
	}

	res := retentionResolver{store: store}
	days, ok := res.RetentionDays(context.Background(), tn.ID)
	if !ok || days != tn.Config.RetentionDays {
		t.Fatalf("RetentionDays = %d ok=%v, want %d", days, ok, tn.Config.RetentionDays)
	}
	if _, ok := res.RetentionDays(context.Background(), "missing"); ok {
		t.Fatal("missing tenant should resolve ok=false")
	}
}

// stubKeys satisfies the tenant key minter without contacting the substrate.
type stubKeys struct{}

func (stubKeys) HybridKeypair(context.Context) (substrate.HybridKeypair, error) {
	return substrate.HybridKeypair{Algorithm: "x25519-kyber768", PublicKeyHex: "ab"}, nil
}
