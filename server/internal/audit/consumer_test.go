package audit

import (
	"context"
	"testing"
	"time"

	natsserver "github.com/nats-io/nats-server/v2/server"
	"github.com/nats-io/nats.go"
	"go.uber.org/zap"
)

// startEmbeddedNATS runs an in-process JetStream-enabled NATS server and
// returns a client connection. It needs no Docker, so it always runs in
// CI.
func startEmbeddedNATS(t *testing.T) *nats.Conn {
	t.Helper()
	opts := &natsserver.Options{
		Host:      "127.0.0.1",
		Port:      -1, // random free port
		JetStream: true,
		StoreDir:  t.TempDir(),
	}
	srv, err := natsserver.NewServer(opts)
	if err != nil {
		t.Fatalf("nats server: %v", err)
	}
	go srv.Start()
	if !srv.ReadyForConnections(10 * time.Second) {
		t.Fatal("nats server not ready")
	}
	t.Cleanup(srv.Shutdown)

	nc, err := nats.Connect(srv.ClientURL())
	if err != nil {
		t.Fatalf("nats connect: %v", err)
	}
	t.Cleanup(nc.Close)
	return nc
}

func TestConsumerPersistsPublishedEvents(t *testing.T) {
	nc := startEmbeddedNATS(t)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	store := NewMemoryStore()
	consumer := NewConsumer(store, zap.NewNop())
	go func() { _ = consumer.Run(ctx, nc) }()

	pub, err := NewPublisher(ctx, nc)
	if err != nil {
		t.Fatal(err)
	}
	if err := pub.Publish(ctx, Event{
		ID: "e1", TenantID: "t1", Action: "export", Actor: "a1", CreatedAt: time.Now().UTC(),
	}); err != nil {
		t.Fatal(err)
	}
	// Malformed payload is terminated, not persisted.
	if _, err := pub.js.Publish(ctx, SubjectEvents, []byte("{not json")); err != nil {
		t.Fatal(err)
	}

	deadline := time.After(15 * time.Second)
	for {
		got, _ := store.Query(ctx, Filter{TenantID: "t1"})
		if len(got) == 1 {
			break
		}
		select {
		case <-deadline:
			t.Fatal("event was not persisted within timeout")
		case <-time.After(50 * time.Millisecond):
		}
	}
}
