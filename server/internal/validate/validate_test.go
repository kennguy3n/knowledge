package validate

import (
	"errors"
	"testing"
)

func TestScopeID(t *testing.T) {
	t.Parallel()
	cases := []struct {
		name    string
		in      string
		want    string
		wantErr error
	}{
		{"valid lowercase", "3f2504e0-4f89-41d3-9a0c-0305e82c3301", "3f2504e0-4f89-41d3-9a0c-0305e82c3301", nil},
		{"valid uppercase canonicalised", "3F2504E0-4F89-41D3-9A0C-0305E82C3301", "3f2504e0-4f89-41d3-9a0c-0305e82c3301", nil},
		{"empty", "", "", ErrEmpty},
		{"not a uuid", "not-a-uuid", "", ErrBadScopeID},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			got, err := ScopeID(tc.in)
			if !errors.Is(err, tc.wantErr) {
				t.Fatalf("err = %v, want %v", err, tc.wantErr)
			}
			if got != tc.want {
				t.Fatalf("got %q, want %q", got, tc.want)
			}
		})
	}
}

func TestUTF8(t *testing.T) {
	t.Parallel()
	if err := UTF8("hello \u4e16\u754c"); err != nil {
		t.Fatalf("valid utf8 rejected: %v", err)
	}
	if err := UTF8(string([]byte{0xff, 0xfe})); !errors.Is(err, ErrNotUTF8) {
		t.Fatalf("invalid utf8 accepted: %v", err)
	}
}

func TestNonEmptyUTF8(t *testing.T) {
	t.Parallel()
	if err := NonEmptyUTF8(""); !errors.Is(err, ErrEmpty) {
		t.Fatalf("empty accepted: %v", err)
	}
	if err := NonEmptyUTF8("ok"); err != nil {
		t.Fatalf("valid rejected: %v", err)
	}
	if err := NonEmptyUTF8(string([]byte{0xff})); !errors.Is(err, ErrNotUTF8) {
		t.Fatalf("invalid utf8 accepted: %v", err)
	}
}
