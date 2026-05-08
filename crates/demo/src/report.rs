//! Markdown report rendering for the demo run.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::assertions::{AssertionLog, AssertionRecord};

#[derive(Debug, Clone)]
pub struct PhaseReport {
    pub name: String,
    pub timing: Duration,
    pub stats: Vec<(String, String)>,
    pub notes: Vec<String>,
}

impl PhaseReport {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            timing: Duration::default(),
            stats: Vec::new(),
            notes: Vec::new(),
        }
    }

    pub fn stat(&mut self, label: impl Into<String>, value: impl Into<String>) {
        self.stats.push((label.into(), value.into()));
    }

    pub fn note(&mut self, line: impl Into<String>) {
        self.notes.push(line.into());
    }
}

#[derive(Debug, Clone)]
pub struct BenchmarkRow {
    pub label: String,
    pub n: u64,
    pub total: Duration,
}

impl BenchmarkRow {
    pub fn per_op(&self) -> Duration {
        if self.n == 0 {
            Duration::ZERO
        } else {
            // div_f64 keeps full u64 precision for `n`; a u32 cast
            // here would silently truncate when `n` exceeds 2^32 and
            // could even bypass the `n == 0` guard above for
            // multiples of 2^32.
            self.total.div_f64(self.n as f64)
        }
    }
}

#[derive(Debug, Default)]
pub struct DemoReport {
    pub started_at: Option<DateTime<Utc>>,
    pub total_wall_clock: Duration,
    pub dataset_size: usize,
    pub phases: Vec<PhaseReport>,
    pub benchmarks: Vec<BenchmarkRow>,
    pub summary_counts: BTreeMap<String, u64>,
    pub assertion_records: Vec<AssertionRecord>,
    pub passed: usize,
    pub failed: usize,
}

impl DemoReport {
    pub fn new() -> Self {
        Self {
            started_at: Some(Utc::now()),
            ..Self::default()
        }
    }

    pub fn add_phase(&mut self, phase: PhaseReport) {
        self.phases.push(phase);
    }

    pub fn add_benchmark(&mut self, label: impl Into<String>, n: u64, total: Duration) {
        self.benchmarks.push(BenchmarkRow {
            label: label.into(),
            n,
            total,
        });
    }

    pub fn count(&mut self, key: impl Into<String>, value: u64) {
        self.summary_counts.insert(key.into(), value);
    }

    pub fn attach_assertions(&mut self, log: &AssertionLog) {
        self.assertion_records = log.records().to_vec();
        self.passed = log.passed_count();
        self.failed = log.failed_count();
    }

    pub fn render_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# Knowledge Substrate End-to-End Demo Results\n\n");
        if let Some(started) = self.started_at {
            let _ = writeln!(out, "- Run started: {}", started.to_rfc3339());
        }
        let _ = writeln!(
            out,
            "- Total wall-clock: {}",
            format_duration(self.total_wall_clock)
        );
        let _ = writeln!(out, "- Synthetic messages: {}", self.dataset_size);
        let pass_rate = if self.passed + self.failed == 0 {
            100.0
        } else {
            100.0 * self.passed as f64 / (self.passed + self.failed) as f64
        };
        let _ = writeln!(
            out,
            "- Assertions: {} passed / {} failed (pass rate {:.1}%)\n",
            self.passed, self.failed, pass_rate,
        );

        out.push_str("## Summary statistics\n\n");
        out.push_str("| Metric | Value |\n|---|---|\n");
        for (k, v) in &self.summary_counts {
            let _ = writeln!(out, "| {k} | {v} |");
        }
        out.push('\n');

        out.push_str("## Phases\n\n");
        for phase in &self.phases {
            let _ = writeln!(
                out,
                "### {} ({})\n",
                phase.name,
                format_duration(phase.timing)
            );
            if !phase.stats.is_empty() {
                out.push_str("| Stat | Value |\n|---|---|\n");
                for (k, v) in &phase.stats {
                    let _ = writeln!(out, "| {k} | {v} |");
                }
                out.push('\n');
            }
            for note in &phase.notes {
                let _ = writeln!(out, "- {note}");
            }
            out.push('\n');
        }

        out.push_str("## Benchmarks (per-operation timings)\n\n");
        out.push_str("| Operation | N | Total | Per-op |\n|---|---|---|---|\n");
        for row in &self.benchmarks {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} |",
                row.label,
                row.n,
                format_duration(row.total),
                format_duration(row.per_op()),
            );
        }
        out.push('\n');

        out.push_str("## Assertions\n\n");
        out.push_str("| Phase | Assertion | Status | Detail |\n|---|---|---|---|\n");
        for r in &self.assertion_records {
            let status = if r.passed { "PASS" } else { "FAIL" };
            let detail = r.detail.clone().unwrap_or_default();
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} |",
                r.phase,
                escape_md_pipe(&r.label),
                status,
                escape_md_pipe(&detail),
            );
        }
        out.push('\n');

        out
    }
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs >= 1.0 {
        format!("{secs:.3}s")
    } else if secs >= 0.001 {
        format!("{:.3}ms", secs * 1_000.0)
    } else {
        format!("{:.1}\u{00b5}s", secs * 1_000_000.0)
    }
}

fn escape_md_pipe(s: &str) -> String {
    s.replace('|', "\\|")
}
