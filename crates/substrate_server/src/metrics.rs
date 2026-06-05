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

use std::fmt::Write as _;

use ffi::{HistogramView, MetricsSnapshot, SlmDispatchHistogram};
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

/// Render the substrate's latency histograms as Prometheus text
/// exposition, appended after the counter/gauge block from [`render`].
///
/// Emits two `histogram`-typed metrics:
///
/// * `knowledge_open_store_duration_seconds` — process-global
///   `open_store` wall-clock latency (no labels).
/// * `knowledge_slm_dispatch_duration_seconds` — per-`(task, adapter)`
///   SLM dispatch latency, one series per pair.
///
/// Each metric emits a single `# TYPE … histogram` line followed by
/// its `_bucket` / `_sum` / `_count` samples, per the Prometheus
/// exposition format.
#[must_use]
pub fn render_histograms(open_store: &HistogramView, slm: &[SlmDispatchHistogram]) -> String {
    let mut out = String::new();

    let open_metric = format!("{PREFIX}_open_store_duration_seconds");
    let _ = writeln!(out, "# TYPE {open_metric} histogram");
    emit_histogram_series(&mut out, &open_metric, &[], &open_store.buckets);
    let _ = writeln!(out, "{open_metric}_sum {}", open_store.sum_seconds);
    let _ = writeln!(out, "{open_metric}_count {}", open_store.count);

    // Single `# TYPE` line for the SLM metric, then one bucket/sum/count
    // block per `(task, adapter)` label set. Prometheus requires the
    // `# TYPE` line to appear exactly once per metric name even when
    // many label sets follow.
    let slm_metric = format!("{PREFIX}_slm_dispatch_duration_seconds");
    let _ = writeln!(out, "# TYPE {slm_metric} histogram");
    for series in slm {
        let labels = [
            ("task", series.task.as_str()),
            ("adapter", series.adapter.as_str()),
        ];
        emit_histogram_series(&mut out, &slm_metric, &labels, &series.buckets);
        let label_str = render_labels(&labels);
        let _ = writeln!(
            out,
            "{slm_metric}_sum{{{label_str}}} {}",
            series.sum_seconds
        );
        let _ = writeln!(out, "{slm_metric}_count{{{label_str}}} {}", series.count);
    }

    out
}

/// Emit the `_bucket` sample lines for one histogram series. The `le`
/// label is appended to any caller-supplied labels; the `+Inf` bucket
/// is rendered as `le="+Inf"`.
fn emit_histogram_series(
    out: &mut String,
    metric: &str,
    labels: &[(&str, &str)],
    buckets: &[(f64, u64)],
) {
    for (le, cumulative) in buckets {
        let le_str = if le.is_finite() {
            format_le(*le)
        } else {
            "+Inf".to_string()
        };
        let mut pairs: Vec<(&str, &str)> = labels.to_vec();
        pairs.push(("le", &le_str));
        let label_str = render_labels(&pairs);
        let _ = writeln!(out, "{metric}_bucket{{{label_str}}} {cumulative}");
    }
}

/// Render a label set as `k="v",k="v"` (no surrounding braces).
fn render_labels(labels: &[(&str, &str)]) -> String {
    labels
        .iter()
        .map(|(k, v)| format!("{k}=\"{v}\""))
        .collect::<Vec<_>>()
        .join(",")
}

/// Format a finite bucket boundary for the `le` label. Uses the
/// shortest round-tripping decimal representation (`{}` on `f64`),
/// which renders the substrate's bucket bounds as `0.001`, `0.025`,
/// `1`, `10`, etc.
fn format_le(le: f64) -> String {
    format!("{le}")
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
