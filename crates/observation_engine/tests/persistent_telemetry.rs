//! Integration tests for [`observation_engine::persistent_telemetry`].
//!
//! Unit tests in the same module pin the in-memory shape of
//! [`capture`] / [`delta`].  These integration tests pin the
//! disk-IO contract end-to-end:
//!
//! * **Round-trip parity** — `capture` + `write_envelope` +
//!   `read_snapshot` reproduces the in-memory envelope bit-for-bit.
//! * **Atomicity** — a write that races against a concurrent read
//!   never observes a half-written file (the file content is
//!   always either the prior envelope or the new envelope).
//! * **Pretty-printed JSON** — the on-disk file is human-readable
//!   (contains newlines + indentation, parses with `serde_json` as
//!   a generic `Value`).
//! * **Forward compat** — an on-disk envelope missing a field
//!   (e.g. a counter added in a later release) deserialises with
//!   the missing field defaulted to `0` (the additive-forward-
//!   compat rule documented on
//!   [`RetrievalMetricsSnapshot`](observation_engine::retrieval_telemetry::RetrievalMetricsSnapshot)).
//! * **Schema mismatch** — an on-disk envelope tagged with a
//!   `schema_version` other than the current
//!   [`PersistentRetrievalSnapshot::SCHEMA_VERSION`] surfaces a
//!   typed error rather than silently dropping fields.

use std::path::Path;

use observation_engine::persistent_telemetry::{
    capture, delta, read_snapshot, write_envelope, write_snapshot, PersistError,
    PersistentRetrievalSnapshot,
};
use tempfile::TempDir;

/// Bit-for-bit round-trip: capture → write → read should produce
/// an envelope equal to the original (modulo timestamp drift,
/// which we override before the write to keep the comparison
/// exact).
#[test]
fn write_then_read_round_trips_envelope_exactly() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("evidence_metrics.json");

    // Capture once, then write a copy of that envelope to disk
    // via `write_envelope` so the comparison is exact (no clock
    // drift between the capture and the disk-write capture).
    let mut envelope = capture();
    envelope.captured_at_unix_ms = 1_234_567;
    envelope.retrieval_metrics.fts.unicode61_lane_queries_total = 99;
    envelope.retrieval_metrics.vector.query_embeddings_total = 17;
    envelope.retrieval_metrics.lexicon.hits_ja = 5;

    write_envelope(&path, &envelope).expect("write_envelope");

    let read_back = read_snapshot(&path).expect("read_snapshot");
    assert_eq!(envelope, read_back);
}

/// `write_snapshot` (the capture-then-write convenience) produces
/// a readable envelope tagged with the current schema version.
#[test]
fn write_snapshot_uses_current_schema_version() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("evidence_metrics.json");

    let written = write_snapshot(&path).expect("write_snapshot");
    assert_eq!(
        written.schema_version,
        PersistentRetrievalSnapshot::SCHEMA_VERSION
    );

    let read_back = read_snapshot(&path).expect("read_snapshot");
    assert_eq!(written, read_back);
}

/// The persisted JSON is pretty-printed (contains newlines +
/// indentation), so an operator can `cat` the file and read it
/// without piping through `jq`.
#[test]
fn persisted_json_is_pretty_printed_and_parses_as_value() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("evidence_metrics.json");
    write_snapshot(&path).expect("write_snapshot");

    let bytes = std::fs::read(&path).expect("read bytes");
    let text = std::str::from_utf8(&bytes).expect("utf8");

    // Pretty-print invariants — multi-line + indented + has the
    // outer keys we expect.
    assert!(
        text.contains('\n'),
        "expected pretty-printed JSON (multi-line), got: {text}"
    );
    assert!(text.contains("\"schema_version\""));
    assert!(text.contains("\"captured_at_unix_ms\""));
    assert!(text.contains("\"retrieval_metrics\""));

    // Also parses as a generic JSON value (no malformed bytes).
    let _: serde_json::Value = serde_json::from_str(text).expect("envelope is valid JSON");
}

/// An on-disk envelope missing a counter field (e.g. an older
/// emitter that pre-dates the addition of a new counter)
/// deserialises with the missing field defaulted to `0`.  This is
/// the additive-forward-compat rule that
/// [`RetrievalMetricsSnapshot`] is derived with `#[serde(default)]`
/// to enforce — the integration test pins it end-to-end through
/// the file-IO path so a future regression that drops the
/// `#[serde(default)]` derive (e.g. on a new field) is caught
/// here rather than in production.
#[test]
fn missing_counter_field_defaults_to_zero_on_read() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("evidence_metrics.json");

    // Hand-write a minimal envelope JSON that omits every
    // sub-snapshot field — every field should deserialise as the
    // default `0` because of the `#[serde(default)]` derive.
    let minimal = serde_json::json!({
        "schema_version": PersistentRetrievalSnapshot::SCHEMA_VERSION,
        "captured_at_unix_ms": 42,
        "retrieval_metrics": {
            "fts": {},
            "lexicon": {},
            "vector": {}
        }
    });
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&minimal).expect("serialize minimal"),
    )
    .expect("write minimal");

    let envelope = read_snapshot(&path).expect("read_snapshot");
    assert_eq!(envelope.captured_at_unix_ms, 42);
    assert_eq!(
        envelope.retrieval_metrics.fts.unicode61_lane_queries_total,
        0
    );
    assert_eq!(envelope.retrieval_metrics.lexicon.hits_ja, 0);
    assert_eq!(envelope.retrieval_metrics.vector.query_embeddings_total, 0);
    // The entire sub-snapshot equals `Default::default()` even
    // though the on-disk JSON spelled out only `{}`.
    assert_eq!(
        envelope.retrieval_metrics.fts,
        evidence_store::fts_telemetry::FtsTelemetrySnapshot::default()
    );
}

/// An on-disk envelope tagged with a different schema version
/// surfaces a [`PersistError::SchemaVersionMismatch`] rather than
/// silently dropping fields.  The error includes both the
/// expected and the found version so the caller can decide
/// whether to migrate the on-disk file or fall back to a fresh
/// capture.
#[test]
fn schema_version_mismatch_returns_typed_error() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("evidence_metrics.json");

    // Forge an envelope with a far-future schema version.
    let forged_version: u32 = PersistentRetrievalSnapshot::SCHEMA_VERSION + 999;
    let forged = serde_json::json!({
        "schema_version": forged_version,
        "captured_at_unix_ms": 0,
        "retrieval_metrics": {
            "fts": {},
            "lexicon": {},
            "vector": {}
        }
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&forged).unwrap()).unwrap();

    let err = read_snapshot(&path).expect_err("expected schema mismatch error");
    match err {
        PersistError::SchemaVersionMismatch { expected, found } => {
            assert_eq!(expected, PersistentRetrievalSnapshot::SCHEMA_VERSION);
            assert_eq!(found, forged_version);
        }
        other => panic!("expected SchemaVersionMismatch, got {other:?}"),
    }
}

/// `write_envelope` to a non-existent directory surfaces an
/// [`io::Error`] via [`PersistError::Io`] rather than panicking
/// or silently swallowing.
#[test]
fn write_to_nonexistent_directory_returns_io_error() {
    let dir = TempDir::new().expect("tempdir");
    let bad = dir
        .path()
        .join("definitely-does-not-exist-subdir")
        .join("evidence_metrics.json");

    let envelope = capture();
    let err = write_envelope(&bad, &envelope).expect_err("expected io error");
    assert!(
        matches!(err, PersistError::Io(_)),
        "expected PersistError::Io for missing parent dir, got: {err:?}"
    );
    // And no orphaned `.tmp` siblings left in the missing-parent
    // path's grandparent (the tempdir root) — the tempfile crate
    // takes care of this via the staging-file `Drop`.
    assert!(
        !bad.exists(),
        "target path must not exist after failed write"
    );
}

/// `write_snapshot` is atomic with respect to concurrent reads —
/// a `read_snapshot` interleaved with a `write_snapshot` always
/// observes either the prior envelope or the new envelope, never
/// a half-written file.
///
/// Implementation: spawn N reader threads tight-looping
/// `read_snapshot` while the main thread runs M writes.  Every
/// successful read must produce a fully-parsed envelope.  No
/// half-written files allowed.
#[test]
fn concurrent_reads_during_writes_never_observe_partial_file() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("evidence_metrics.json");

    // Seed the file with an initial envelope so readers don't
    // race the very first write.
    write_snapshot(&path).expect("seed");

    let stop = Arc::new(AtomicBool::new(false));
    let path_arc = Arc::new(path.clone());

    let mut handles = Vec::new();
    for _ in 0..4 {
        let stop = Arc::clone(&stop);
        let path = Arc::clone(&path_arc);
        handles.push(thread::spawn(move || -> Result<(), String> {
            while !stop.load(Ordering::Relaxed) {
                // Every read must succeed.  A half-written file
                // would surface as PersistError::Json (truncated
                // JSON) or PersistError::Io.
                match read_snapshot(&path) {
                    Ok(env) => {
                        assert_eq!(
                            env.schema_version,
                            PersistentRetrievalSnapshot::SCHEMA_VERSION
                        );
                    }
                    Err(PersistError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                        // POSIX `rename(2)` over an existing
                        // target is atomic — a concurrent read
                        // on Linux/macOS should always see
                        // either the prior or the new file and
                        // never trip this arm.  On Windows
                        // however, `tempfile::NamedTempFile::
                        // persist` may have to delete the target
                        // before renaming on some filesystems,
                        // which produces a brief `NotFound`
                        // window.  Tolerate `NotFound` for
                        // cross-platform robustness; reject
                        // every other error kind (including
                        // truncated-JSON `PersistError::Json`).
                    }
                    Err(other) => {
                        return Err(format!("unexpected read error: {other:?}"));
                    }
                }
            }
            Ok(())
        }));
    }

    // Write a few hundred times to maximise interleaving.
    for _ in 0..200 {
        write_snapshot(&path).expect("write under load");
    }
    stop.store(true, Ordering::Relaxed);

    for h in handles {
        h.join().expect("thread join").expect("no read errors");
    }
}

/// `delta` of two on-disk envelopes (one written before some
/// in-process counter increments, one written after) produces
/// the obvious per-counter diff.  End-to-end pin of the
/// "operator dashboard" use case.
#[test]
fn delta_between_two_written_envelopes_matches_in_memory_delta() {
    let dir = TempDir::new().expect("tempdir");
    let prior_path: &Path = &dir.path().join("prior.json");
    let latest_path: &Path = &dir.path().join("latest.json");

    // Hand-craft envelopes so the delta is deterministic — this
    // is the dashboard-rate use case, not a real-counter
    // exercise.
    let mut prior = capture();
    prior.captured_at_unix_ms = 10_000;
    prior.retrieval_metrics.fts.unicode61_lane_queries_total = 100;
    write_envelope(prior_path, &prior).expect("write prior");

    let mut latest = capture();
    latest.captured_at_unix_ms = 14_500;
    latest.retrieval_metrics = prior.retrieval_metrics.clone();
    latest.retrieval_metrics.fts.unicode61_lane_queries_total = 175;
    write_envelope(latest_path, &latest).expect("write latest");

    let prior_read = read_snapshot(prior_path).expect("read prior");
    let latest_read = read_snapshot(latest_path).expect("read latest");
    let d = delta(&prior_read, &latest_read);

    assert_eq!(d.elapsed_ms, 4_500);
    assert_eq!(d.retrieval_metrics.fts.unicode61_lane_queries_total, 75);
}
