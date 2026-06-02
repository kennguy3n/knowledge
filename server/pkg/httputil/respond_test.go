package httputil

import (
	"encoding/json"
	"net/http/httptest"
	"testing"
)

func TestJSON(t *testing.T) {
	rec := httptest.NewRecorder()
	JSON(rec, 200, map[string]string{"key": "value"})

	if rec.Code != 200 {
		t.Errorf("status = %d, want 200", rec.Code)
	}
	if ct := rec.Header().Get("Content-Type"); ct != "application/json; charset=utf-8" {
		t.Errorf("content-type = %q", ct)
	}

	var body map[string]string
	if err := json.Unmarshal(rec.Body.Bytes(), &body); err != nil {
		t.Fatalf("decode error: %v", err)
	}
	if body["key"] != "value" {
		t.Errorf("body[key] = %q, want value", body["key"])
	}
}

func TestJSON_Nil(t *testing.T) {
	rec := httptest.NewRecorder()
	JSON(rec, 204, nil)
	if rec.Code != 204 {
		t.Errorf("status = %d, want 204", rec.Code)
	}
}

func TestError(t *testing.T) {
	rec := httptest.NewRecorder()
	Error(rec, 400, "bad request")

	if rec.Code != 400 {
		t.Errorf("status = %d, want 400", rec.Code)
	}

	var body map[string]string
	if err := json.Unmarshal(rec.Body.Bytes(), &body); err != nil {
		t.Fatalf("decode error: %v", err)
	}
	if body["error"] != "bad request" {
		t.Errorf("error = %q, want 'bad request'", body["error"])
	}
}
