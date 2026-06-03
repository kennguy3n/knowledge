// Package logging provides a zap-based structured logger with
// built-in PII redaction. Scope IDs and other opaque identifiers are
// safe to log; message bodies and other free-text content are not and
// must be passed through [Body] (which emits only a length + hash
// hint, never the plaintext).
package logging

import (
	"crypto/sha256"
	"encoding/hex"
	"os"

	"go.uber.org/zap"
	"go.uber.org/zap/zapcore"
)

// New builds a production JSON logger. The level is taken from the
// KNOWLEDGE_LOG_LEVEL env var (debug|info|warn|error), defaulting to
// info. The returned logger writes to stderr.
func New() *zap.Logger {
	level := zapcore.InfoLevel
	if lvl := os.Getenv("KNOWLEDGE_LOG_LEVEL"); lvl != "" {
		_ = level.UnmarshalText([]byte(lvl))
	}
	cfg := zap.NewProductionEncoderConfig()
	cfg.TimeKey = "ts"
	cfg.EncodeTime = zapcore.ISO8601TimeEncoder
	core := zapcore.NewCore(
		zapcore.NewJSONEncoder(cfg),
		zapcore.Lock(os.Stderr),
		level,
	)
	return zap.New(core, zap.AddCaller())
}

// NewNop returns a no-op logger for tests.
func NewNop() *zap.Logger { return zap.NewNop() }

// Body returns a zap field that describes a sensitive body WITHOUT
// revealing its contents: it emits the byte length and a short
// truncated SHA-256 fingerprint so logs can correlate identical
// payloads without ever storing PII.
//
// Never log message bodies directly — always route them through this
// helper.
func Body(key string, content []byte) zap.Field {
	sum := sha256.Sum256(content)
	return zap.Object(key, redactedBody{
		length: len(content),
		hash:   hex.EncodeToString(sum[:8]),
	})
}

type redactedBody struct {
	length int
	hash   string
}

// MarshalLogObject implements zapcore.ObjectMarshaler.
func (b redactedBody) MarshalLogObject(enc zapcore.ObjectEncoder) error {
	enc.AddInt("length", b.length)
	enc.AddString("sha256_8", b.hash)
	enc.AddBool("redacted", true)
	return nil
}
