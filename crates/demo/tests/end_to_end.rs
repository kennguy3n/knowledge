//! Integration tests for the `demo` binary.
//!
//! These tests build the demo binary (via Cargo's `CARGO_BIN_EXE_*`
//! mechanism), run it inside a temporary directory, and verify that
//!
//! 1. it exits cleanly without panicking,
//! 2. every assertion logged by the demo passed (no `FAIL` rows in
//!    `results/demo_results.md`),
//! 3. the results file is written and contains all the expected
//!    sections (per-stage headings, summary statistics, benchmarks),
//! 4. timing data is present and parseable, and
//! 5. the assertion count matches between the stdout summary line and
//!    the in-file count.

use std::process::Command;

use tempfile::TempDir;

/// Run the demo binary inside a temporary directory and return
/// (stdout, results_file_contents) on success.
fn run_demo() -> (String, String) {
    let workdir = TempDir::new().expect("create demo workdir");
    let bin = env!("CARGO_BIN_EXE_demo");

    let output = Command::new(bin)
        .current_dir(workdir.path())
        .output()
        .expect("spawn demo binary");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(output.status.success(),
        "demo exited with non-zero status: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );

    let results_path = workdir.path().join("results").join("demo_results.md");
    assert!(results_path.is_file(),
        "demo did not produce results/demo_results.md (looked at {})",
        results_path.display()
    );
    let contents = std::fs::read_to_string(&results_path).expect("read results file");
    (stdout, contents)
}

#[test]
fn demo_runs_to_completion_without_panics() {
    let (stdout, _contents) = run_demo();
    assert!(stdout.contains("knowledge demo: starting full-pipeline drive"),
        "demo did not print start banner; stdout was:\n{stdout}"
    );
    assert!(stdout.contains("results written to results/demo_results.md"),
        "demo did not print results banner; stdout was:\n{stdout}"
    );
}

#[test]
fn demo_results_file_is_written_and_well_formed() {
    let (_stdout, contents) = run_demo();

    let required_sections = [
        "# Knowledge Substrate End-to-End Demo Results",
        "## Summary statistics",
        "## Stages",
        "## Benchmarks (per-operation timings)",
        "## Assertions",
    ];
    for section in required_sections {
        assert!(contents.contains(section),
            "results file missing required section `{section}`"
        );
    }

    // Run-level metadata lines must be present.
    for prefix in [
        "- Run started:",
        "- Total wall-clock:",
        "- Synthetic messages:",
        "- Assertions:",
    ] {
        assert!(contents.contains(prefix),
            "results file missing metadata line `{prefix}`"
        );
    }

    // Every stage from the 1 -> 12 pipeline must be represented as a
    // `### Stage N: ...` heading.
    let stage_headings = [
        "### Stage 1: Evidence Ingestion",
        "### Stage 2: Observation Extraction",
        "### Stage 3: Memory Manager",
        "### Stage 4: Concept Graph",
        "### Stage 5: Synthesis Pipeline",
        "### Stage 6: Permission Service",
        "### Stage 7: Crypto",
        "### Stage 8: Export Plane",
        "### Stage 9: Agent Contract",
        "### Stage 10: Reasoning Engine",
        "### Stage 11: Connector Framework",
        "### Stage 12: Audit Service",
    ];
    for stage in stage_headings {
        assert!(contents.contains(stage),
            "results file missing per-stage heading for `{stage}`"
        );
    }
}

#[test]
fn demo_records_no_assertion_failures() {
    let (_stdout, contents) = run_demo();

    let fail_lines: Vec<&str> = contents
        .lines()
        .filter(|line| line.contains("| FAIL |"))
        .collect();
    assert!(fail_lines.is_empty(),
        "demo recorded {} failing assertion(s):\n{}",
        fail_lines.len(),
        fail_lines.join("\n")
    );

    // Sanity: the assertion table is actually populated.
    let pass_count = contents.matches("| PASS |").count();
    assert!(pass_count > 100,
        "expected > 100 PASS rows in assertion table, found {pass_count}"
    );
}

#[test]
fn demo_assertion_counts_match_stdout_and_file() {
    let (stdout, contents) = run_demo();

    // Stdout summary line, e.g. "196 of 196 assertions passed".
    let summary_line = stdout
        .lines()
        .find(|l| l.contains("of") && l.contains("assertions passed"))
        .expect("stdout did not include assertion summary");
    let mut tokens = summary_line.split_whitespace();
    let passed_str = tokens.next().expect("missing passed count token");
    let _of = tokens.next();
    let total_str = tokens.next().expect("missing total count token");
    let stdout_passed: usize = passed_str.parse().expect("passed count must be integer");
    let stdout_total: usize = total_str.parse().expect("total count must be integer");

    assert!(stdout_total > 100,
        "expected at least 100 demo assertions, got {stdout_total}"
    );
    assert_eq!(stdout_passed, stdout_total,
        "demo reported {stdout_passed} of {stdout_total} assertions passed; expected all to pass"
    );

    // Results file echoes:
    //   "- Assertions: 196 passed / 0 failed (pass rate 100.0%)"
    let assertions_line = contents
        .lines()
        .find(|line| line.starts_with("- Assertions:"))
        .expect("results file missing - Assertions: line");
    let assertions_tokens: Vec<&str> = assertions_line.split_whitespace().collect();
    let file_passed: usize = assertions_tokens
        .get(2)
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("malformed assertions line: `{assertions_line}`"));
    let file_failed: usize = assertions_tokens
        .get(5)
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("malformed assertions line: `{assertions_line}`"));

    assert_eq!(file_passed, stdout_passed,
        "results file passed ({file_passed}) != stdout passed ({stdout_passed})"
    );
    assert_eq!(file_failed, 0,
        "results file reported {file_failed} failures (line: `{assertions_line}`)"
    );
    assert_eq!(file_passed + file_failed,
        stdout_total,
        "results file passed+failed ({}) != stdout total ({stdout_total})",
        file_passed + file_failed
    );
}

#[test]
fn demo_records_timing_data_for_every_phase() {
    let (_stdout, contents) = run_demo();

    // Per-stage headings include a duration in parentheses, e.g.
    // `### Stage 1: Evidence Ingestion (12.345ms)`. Require every
    // stage's heading line to carry a parseable duration suffix.
    let stage_prefixes = [
        "### Stage 1:",
        "### Stage 2:",
        "### Stage 3:",
        "### Stage 4:",
        "### Stage 5:",
        "### Stage 6:",
        "### Stage 7:",
        "### Stage 8:",
        "### Stage 9:",
        "### Stage 10:",
        "### Stage 11:",
        "### Stage 12:",
    ];
    for prefix in stage_prefixes {
        let line = contents
            .lines()
            .find(|line| line.starts_with(prefix))
            .unwrap_or_else(|| panic!("stage heading line for `{prefix}` not found"));
        let has_timing = line.contains("\u{00b5}s") || line.contains("ms") || line.ends_with("s)");
        assert!(has_timing,
            "stage heading `{line}` did not include a parseable timing"
        );
    }

    // Wall-clock line should not be all zeros.
    let wall_line = contents
        .lines()
        .find(|line| line.starts_with("- Total wall-clock:"))
        .expect("wall-clock line missing");
    assert!(!wall_line.contains("0.000ms") && !wall_line.contains("0.0\u{00b5}s"),
        "wall-clock should be > 0, got `{wall_line}`"
    );

    // The benchmarks section must include a populated table with at
    // least one numeric data row beyond the header.
    let bench_section = contents
        .split("## Benchmarks (per-operation timings)")
        .nth(1)
        .expect("benchmarks section absent");
    let table_rows: Vec<&str> = bench_section
        .lines()
        .filter(|line| line.starts_with("| ") && !line.contains("---"))
        .collect();
    assert!(table_rows.len() > 1,
        "benchmarks table missing data rows; section was:\n{bench_section}"
    );
    let data_rows = &table_rows[1..];
    let some_with_timing = data_rows
        .iter()
        .any(|line| line.contains("ms") || line.contains("\u{00b5}s") || line.contains("s |"));
    assert!(some_with_timing,
        "benchmark data rows missing timings: {data_rows:?}"
    );
}
