// Package logging provides a structured zap logger with PII redaction.
package logging

import (
	"strings"

	"go.uber.org/zap"
	"go.uber.org/zap/zapcore"
)

// piiFields are field keys whose values are redacted in log output.
var piiFields = map[string]bool{
	"email":        true,
	"password":     true,
	"token":        true,
	"secret":       true,
	"api_key":      true,
	"master_key":   true,
	"private_key":  true,
	"access_token": true,
	"body":         true,
}

// piiRedactor wraps a zapcore.Core and redacts sensitive fields.
type piiRedactor struct {
	zapcore.Core
}

// With adds fields after redacting PII.
func (r *piiRedactor) With(fields []zapcore.Field) zapcore.Core {
	redacted := make([]zapcore.Field, len(fields))
	for i, f := range fields {
		if isPII(f.Key) {
			redacted[i] = zap.String(f.Key, "[REDACTED]")
		} else {
			redacted[i] = f
		}
	}
	return &piiRedactor{Core: r.Core.With(redacted)}
}

// Check delegates to the inner core.
func (r *piiRedactor) Check(entry zapcore.Entry, ce *zapcore.CheckedEntry) *zapcore.CheckedEntry {
	return r.Core.Check(entry, ce)
}

// Write redacts PII from fields before writing.
func (r *piiRedactor) Write(entry zapcore.Entry, fields []zapcore.Field) error {
	redacted := make([]zapcore.Field, len(fields))
	for i, f := range fields {
		if isPII(f.Key) {
			redacted[i] = zap.String(f.Key, "[REDACTED]")
		} else {
			redacted[i] = f
		}
	}
	return r.Core.Write(entry, redacted)
}

func isPII(key string) bool {
	lower := strings.ToLower(key)
	return piiFields[lower]
}

// New creates a production-ready zap logger with PII redaction.
func New() (*zap.Logger, error) {
	cfg := zap.NewProductionConfig()
	cfg.EncoderConfig.TimeKey = "ts"
	cfg.EncoderConfig.EncodeTime = zapcore.ISO8601TimeEncoder

	base, err := cfg.Build()
	if err != nil {
		return nil, err
	}

	redacted := base.WithOptions(zap.WrapCore(func(c zapcore.Core) zapcore.Core {
		return &piiRedactor{Core: c}
	}))

	return redacted, nil
}
