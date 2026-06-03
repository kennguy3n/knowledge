package logging

import (
	"strings"
	"testing"

	"go.uber.org/zap"
	"go.uber.org/zap/zapcore"
	"go.uber.org/zap/zaptest/observer"
)

func TestBodyNeverLeaksPlaintext(t *testing.T) {
	t.Parallel()
	core, logs := observer.New(zapcore.InfoLevel)
	log := zap.New(core)

	secret := "this is sensitive PII that must never appear in logs"
	log.Info("ingested", Body("body", []byte(secret)))

	entries := logs.All()
	if len(entries) != 1 {
		t.Fatalf("expected 1 log entry, got %d", len(entries))
	}
	enc := zapcore.NewMapObjectEncoder()
	for _, f := range entries[0].Context {
		f.AddTo(enc)
	}
	bodyField, ok := enc.Fields["body"].(map[string]any)
	if !ok {
		t.Fatalf("body field missing or wrong type: %#v", enc.Fields["body"])
	}
	if bodyField["redacted"] != true {
		t.Errorf("body not marked redacted: %#v", bodyField)
	}
	if l, _ := bodyField["length"].(int); l != len(secret) {
		t.Errorf("length = %v (%T), want %d", bodyField["length"], bodyField["length"], len(secret))
	}
	// The plaintext must not appear anywhere in the rendered fields.
	for k, v := range bodyField {
		if s, ok := v.(string); ok && strings.Contains(s, "sensitive") {
			t.Errorf("plaintext leaked in field %q: %q", k, s)
		}
	}
}

func TestNew(t *testing.T) {
	t.Parallel()
	if New() == nil {
		t.Fatal("New returned nil")
	}
	if NewNop() == nil {
		t.Fatal("NewNop returned nil")
	}
}
