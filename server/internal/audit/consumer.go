package audit

import (
	"context"
	"encoding/json"
	"fmt"

	"github.com/nats-io/nats.go"
	"github.com/nats-io/nats.go/jetstream"
	"go.uber.org/zap"
)

// JetStream stream/subject/consumer identifiers for audit events.
const (
	StreamName    = "AUDIT"
	SubjectAll    = "audit.>"
	SubjectEvents = "audit.events"
	DurableName   = "audit-persist"
)

// Publisher emits audit events onto JetStream.
type Publisher struct {
	js jetstream.JetStream
}

// NewPublisher builds a JetStream publisher and ensures the audit
// stream exists.
func NewPublisher(ctx context.Context, nc *nats.Conn) (*Publisher, error) {
	js, err := jetstream.New(nc)
	if err != nil {
		return nil, fmt.Errorf("audit: jetstream: %w", err)
	}
	if _, err := js.CreateOrUpdateStream(ctx, jetstream.StreamConfig{
		Name:     StreamName,
		Subjects: []string{SubjectAll},
	}); err != nil {
		return nil, fmt.Errorf("audit: ensure stream: %w", err)
	}
	return &Publisher{js: js}, nil
}

// Publish marshals and publishes an event to the audit subject.
func (p *Publisher) Publish(ctx context.Context, e Event) error {
	data, err := json.Marshal(e)
	if err != nil {
		return fmt.Errorf("audit: marshal event: %w", err)
	}
	if _, err := p.js.Publish(ctx, SubjectEvents, data); err != nil {
		return fmt.Errorf("audit: publish: %w", err)
	}
	return nil
}

// Consumer persists audit events from JetStream into the store.
type Consumer struct {
	store Store
	log   *zap.Logger
}

// NewConsumer builds a JetStream consumer writing to store.
func NewConsumer(store Store, log *zap.Logger) *Consumer {
	if log == nil {
		log = zap.NewNop()
	}
	return &Consumer{store: store, log: log}
}

// Run starts consuming audit events and blocks until ctx is cancelled.
// It creates the stream and a durable pull consumer, persisting each
// event and acking only after a successful store write so failures are
// redelivered.
func (c *Consumer) Run(ctx context.Context, nc *nats.Conn) error {
	js, err := jetstream.New(nc)
	if err != nil {
		return fmt.Errorf("audit: jetstream: %w", err)
	}
	stream, err := js.CreateOrUpdateStream(ctx, jetstream.StreamConfig{
		Name:     StreamName,
		Subjects: []string{SubjectAll},
	})
	if err != nil {
		return fmt.Errorf("audit: ensure stream: %w", err)
	}
	cons, err := stream.CreateOrUpdateConsumer(ctx, jetstream.ConsumerConfig{
		Durable:       DurableName,
		AckPolicy:     jetstream.AckExplicitPolicy,
		FilterSubject: SubjectEvents,
	})
	if err != nil {
		return fmt.Errorf("audit: ensure consumer: %w", err)
	}

	cc, err := cons.Consume(func(msg jetstream.Msg) {
		var e Event
		if err := json.Unmarshal(msg.Data(), &e); err != nil {
			c.log.Warn("audit: drop malformed event", zap.Error(err))
			_ = msg.Term() // unparseable; never redeliver
			return
		}
		if _, err := c.persist(ctx, e); err != nil {
			c.log.Warn("audit: persist failed; will redeliver", zap.Error(err))
			_ = msg.Nak()
			return
		}
		_ = msg.Ack()
	})
	if err != nil {
		return fmt.Errorf("audit: consume: %w", err)
	}
	defer cc.Stop()

	<-ctx.Done()
	return nil
}

// persist normalises and stores an event.
func (c *Consumer) persist(ctx context.Context, e Event) (Event, error) {
	svc := Service{store: c.store}
	return svc.Record(ctx, e)
}
