package connector

import (
	"context"
	"sync"
	"time"
)

// SyncFunc runs one sync for a connector instance. The scheduler
// supplies a per-tick context derived from the scheduler's base
// context.
type SyncFunc func(ctx context.Context, instanceID string)

// Scheduler runs a cron-like periodic sync per connector instance.
// Each instance gets its own goroutine and ticker; schedules can be
// added and removed at runtime and are all torn down on [Scheduler.Stop].
type Scheduler struct {
	syncFn SyncFunc

	mu   sync.Mutex
	base context.Context
	jobs map[string]context.CancelFunc
	wg   sync.WaitGroup
}

// NewScheduler builds a scheduler that calls syncFn on each tick.
func NewScheduler(syncFn SyncFunc) *Scheduler {
	return &Scheduler{
		syncFn: syncFn,
		base:   context.Background(),
		jobs:   make(map[string]context.CancelFunc),
	}
}

// Start binds the scheduler's base context. All scheduled jobs are
// cancelled when ctx is cancelled (or [Scheduler.Stop] is called).
func (s *Scheduler) Start(ctx context.Context) {
	s.mu.Lock()
	s.base = ctx
	s.mu.Unlock()
}

// Schedule (re)registers a connector instance to sync every interval.
// A zero or negative interval is ignored. Re-scheduling an existing
// instance replaces its prior schedule.
func (s *Scheduler) Schedule(instanceID string, interval time.Duration) {
	if interval <= 0 {
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if cancel, ok := s.jobs[instanceID]; ok {
		cancel()
	}
	jobCtx, cancel := context.WithCancel(s.base)
	s.jobs[instanceID] = cancel
	s.wg.Add(1)
	go s.run(jobCtx, instanceID, interval)
}

// Unschedule stops syncing a connector instance.
func (s *Scheduler) Unschedule(instanceID string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if cancel, ok := s.jobs[instanceID]; ok {
		cancel()
		delete(s.jobs, instanceID)
	}
}

// Count returns the number of currently scheduled instances.
func (s *Scheduler) Count() int {
	s.mu.Lock()
	defer s.mu.Unlock()
	return len(s.jobs)
}

// Stop cancels every scheduled job and waits for the goroutines to
// exit.
func (s *Scheduler) Stop() {
	s.mu.Lock()
	for id, cancel := range s.jobs {
		cancel()
		delete(s.jobs, id)
	}
	s.mu.Unlock()
	s.wg.Wait()
}

func (s *Scheduler) run(ctx context.Context, instanceID string, interval time.Duration) {
	defer s.wg.Done()
	t := time.NewTicker(interval)
	defer t.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-t.C:
			s.syncFn(ctx, instanceID)
		}
	}
}
