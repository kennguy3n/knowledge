package audit

import (
	"context"
	"time"

	"go.uber.org/zap"
)

// RetentionResolver yields the retention window (in days) for a tenant.
// ok is false when the tenant is unknown, in which case it is skipped.
type RetentionResolver interface {
	RetentionDays(ctx context.Context, tenantID string) (days int, ok bool)
}

// TenantLister enumerates tenant ids subject to retention sweeps.
type TenantLister interface {
	TenantIDs(ctx context.Context) ([]string, error)
}

// Retention enforces per-tenant audit retention by periodically
// deleting events older than each tenant's configured window.
type Retention struct {
	store    Store
	tenants  TenantLister
	resolver RetentionResolver
	interval time.Duration
	log      *zap.Logger
}

// NewRetention builds a retention enforcer. A non-positive interval
// defaults to one hour.
func NewRetention(store Store, tenants TenantLister, resolver RetentionResolver, interval time.Duration, log *zap.Logger) *Retention {
	if interval <= 0 {
		interval = time.Hour
	}
	if log == nil {
		log = zap.NewNop()
	}
	return &Retention{store: store, tenants: tenants, resolver: resolver, interval: interval, log: log}
}

// Run sweeps on a ticker until ctx is cancelled. It performs one sweep
// immediately on start.
func (r *Retention) Run(ctx context.Context) {
	t := time.NewTicker(r.interval)
	defer t.Stop()
	r.sweep(ctx)
	for {
		select {
		case <-ctx.Done():
			return
		case <-t.C:
			r.sweep(ctx)
		}
	}
}

// Sweep performs a single retention pass and returns the total number
// of events deleted across all tenants.
func (r *Retention) Sweep(ctx context.Context) (int64, error) {
	return r.sweepReport(ctx)
}

func (r *Retention) sweep(ctx context.Context) {
	if _, err := r.sweepReport(ctx); err != nil {
		r.log.Warn("audit: retention sweep failed", zap.Error(err))
	}
}

func (r *Retention) sweepReport(ctx context.Context) (int64, error) {
	ids, err := r.tenants.TenantIDs(ctx)
	if err != nil {
		return 0, err
	}
	now := time.Now().UTC()
	var total int64
	for _, id := range ids {
		days, ok := r.resolver.RetentionDays(ctx, id)
		if !ok || days <= 0 {
			continue
		}
		cutoff := now.AddDate(0, 0, -days)
		n, err := r.store.DeleteOlderThan(ctx, id, cutoff)
		if err != nil {
			return total, err
		}
		total += n
	}
	return total, nil
}
