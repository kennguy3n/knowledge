//! Prometheus text-exposition rendering for the FFI metrics snapshot.
//!
//! `ffi::metrics::snapshot()` returns a flat-ish struct of `u64`
//! counters/gauges plus a nested `errors_by_kind` block. Rather than
//! hand-enumerate every field (which would drift the moment a new
//! counter is added to `MetricsSnapshot`), we serialise the snapshot
//! to JSON and walk the object, emitting one Prometheus line per
//! numeric leaf. Nested objects (e.g. `errors_by_kind`) contribute a
//! `by_kind` label. This keeps the exposition additive: new FFI
//! counters appear automatically.

use ffi::MetricsSnapshot;
use serde_json::Value;

/// Metric name prefix for every exported series.
const PREFIX: &str = "knowledge";

/// Render a [`MetricsSnapshot`] as Prometheus text exposition.
///
/// The output is `text/plain; version=0.0.4` compatible: one
/// `# TYPE` line followed by one sample line per counter. Counters
/// whose name ends in `_total` are typed `counter`; everything else
/// (gauges such as `open_handles`) is typed `gauge`.
#[must_use]
pub fn render(snapshot: &MetricsSnapshot) -> String {
    // Serialising a `#[derive(Serialize)]` struct of primitives is
    // infallible in practice; fall back to an empty object on the
    // impossible error path rather than panicking.
    let value = serde_json::to_value(snapshot).unwrap_or(Value::Null);
    let mut out = String::new();
    if let Value::Object(map) = value {
        for (key, leaf) in map {
            match leaf {
                Value::Number(n) => emit(&mut out, &key, &[], &n),
                Value::Object(inner) => {
                    for (sub, subleaf) in inner {
                        if let Value::Number(n) = subleaf {
                            emit(&mut out, &key, &[("by_kind", &sub)], &n);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Emit a single `# TYPE` + sample pair for one metric.
fn emit(out: &mut String, name: &str, labels: &[(&str, &str)], n: &serde_json::Number) {
    use std::fmt::Write as _;

    let metric = format!("{PREFIX}_{name}");
    let kind = if name.ends_with("_total") {
        "counter"
    } else {
        "gauge"
    };
    // Writing into a `String` via `fmt::Write` is infallible, so the
    // `Result`s are discarded.
    let _ = writeln!(out, "# TYPE {metric} {kind}");
    if labels.is_empty() {
        let _ = writeln!(out, "{metric} {n}");
    } else {
        let rendered: Vec<String> = labels.iter().map(|(k, v)| format!("{k}=\"{v}\"")).collect();
        let _ = writeln!(out, "{metric}{{{}}} {n}", rendered.join(","));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_counters_and_error_labels() {
        // `prime()` then snapshot to get a real, fully-populated
        // MetricsSnapshot with the expected field set.
        ffi::metrics::prime();
        let snap = ffi::metrics_snapshot();
        let text = render(&snap);
        assert!(text.contains("knowledge_ingest_total"));
        assert!(text.contains("# TYPE knowledge_query_total counter"));
        // Nested error counters carry the `by_kind` label.
        assert!(text.contains("knowledge_errors_by_kind{by_kind=\"not_found\"}"));
    }

    #[test]
    fn gauge_fields_are_typed_gauge() {
        ffi::metrics::prime();
        let snap = ffi::metrics_snapshot();
        let text = render(&snap);
        assert!(text.contains("# TYPE knowledge_open_handles gauge"));
    }
}
