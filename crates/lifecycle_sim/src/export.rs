//! CSV / Excel export for simulation validation.
//!
//! This module converts a [`SimReport`] into a set of CSV files that can
//! be opened directly in Excel for human review. One directory is
//! created per run, containing multiple sheets:
//!
//! * `turns.csv` — every turn with message, importance, language, scenario
//! * `observations.csv` — every observation extracted per turn
//! * `assertions.csv` — every assertion result, pass/fail, with detail
//! * `failures.csv` — only failed assertions
//! * `memory_states.csv` — lifecycle state of memory objects at end of run
//! * `retention_scores.csv` — per-object retention components
//! * `synthesis_windows.csv` — synthesis windows opened/completed per scope
//! * `summary.csv` — run summary and aggregate metrics
//!
//! The CSV output is intentionally flat and human-readable. Columns are
//! named for Excel import and use RFC 4180 quoting.

use std::fs;
use std::io::{BufWriter, Write};
use std::path::Path;

use crate::replay::SimReport;

/// Write all validation CSVs for `report` into `out_dir`.
///
/// `out_dir` is created if it does not exist. Existing files are overwritten.
/// Returns the number of bytes written across all files.
pub fn write_csv_export(out_dir: &Path, report: &SimReport) -> Result<usize, String> {
    fs::create_dir_all(out_dir).map_err(|e| format!("create export dir: {e}"))?;
    let mut total = 0usize;

    total += write_summary_csv(out_dir, report)?;
    total += write_turns_csv(out_dir, report)?;
    total += write_assertions_csv(out_dir, report)?;
    total += write_failures_csv(out_dir, report)?;

    // Write captured raw data (observations, memory states, retention
    // scores, synthesis windows) if it was populated during replay.
    total += write_observations_csv(out_dir, &report.export_data.observations)?;
    total += write_memory_states_csv(out_dir, &report.export_data.memory_states)?;
    total += write_retention_scores_csv(out_dir, &report.export_data.retention_scores)?;
    total += write_synthesis_windows_csv(out_dir, &report.export_data.synthesis_windows)?;

    Ok(total)
}

/// Write the summary sheet.
///
/// The sheet uses a single, consistent header: `section,metric,value1,value2,value3`.
/// * `scalar` rows use `metric,value1` (run-level metrics).
/// * `tenant` rows use `tenant_id,turns,pass_rate`.
/// * `language` rows use `language,messages,obs_per_msg,pass_rate`.
/// * `scenario` rows use `scenario,instances,pass_rate`.
fn write_summary_csv(out_dir: &Path, report: &SimReport) -> Result<usize, String> {
    let path = out_dir.join("summary.csv");
    let file = fs::File::create(&path).map_err(|e| format!("create summary.csv: {e}"))?;
    let mut w = BufWriter::new(file);
    let mut bytes = 0usize;

    writeln!(
        w,
        "section,metric,value1,value2,value3"
    )
    .map_err(|e| format!("write summary.csv header: {e}"))?;

    let mut scalar = |m: &str, v: &str| -> Result<(), String> {
        writeln!(w, "scalar,{m},{v},,")
            .map_err(|e| format!("write summary.csv scalar: {e}"))
    };

    scalar("preset", &report.run_config.preset)?;
    scalar("scale", &report.run_config.scale)?;
    scalar("driver", &report.run_config.driver)?;
    scalar("seed", &report.run_config.seed.to_string())?;
    scalar("total_turns", &report.summary.total_turns.to_string())?;
    scalar("total_tenants", &report.summary.total_tenants.to_string())?;
    scalar("total_users", &report.summary.total_users.to_string())?;
    scalar("total_scopes", &report.summary.total_scopes.to_string())?;
    scalar("pass_rate", &format!("{:.6}", report.summary.pass_rate))?;
    scalar("failed_assertions", &report.summary.failed_assertions.to_string())?;
    scalar("total_assertions", &report.summary.total_assertions.to_string())?;
    scalar("duration_secs", &format!("{:.6}", report.summary.duration_secs))?;

    for t in &report.per_tenant {
        writeln!(
            w,
            "tenant,{},{},{:.6},",
            t.tenant_id, t.turns, t.pass_rate
        )
        .map_err(|e| format!("write summary.csv tenant: {e}"))?;
    }
    for l in &report.per_language {
        writeln!(
            w,
            "language,{},{},{:.6},{:.6}",
            csv_escape(&l.language),
            l.messages,
            l.obs_per_msg,
            l.pass_rate
        )
        .map_err(|e| format!("write summary.csv language: {e}"))?;
    }
    for s in &report.per_scenario {
        writeln!(
            w,
            "scenario,{},{},{:.6},",
            csv_escape(&s.scenario),
            s.instances,
            s.pass_rate
        )
        .map_err(|e| format!("write summary.csv scenario: {e}"))?;
    }

    bytes += w.into_inner()
        .map_err(|e| format!("flush summary.csv: {e}"))?
        .metadata()
        .map_err(|e| format!("metadata summary.csv: {e}"))?
        .len() as usize;
    Ok(bytes)
}

/// Write the turns sheet.
fn write_turns_csv(out_dir: &Path, report: &SimReport) -> Result<usize, String> {
    let path = out_dir.join("turns.csv");
    let file = fs::File::create(&path).map_err(|e| format!("create turns.csv: {e}"))?;
    let mut w = BufWriter::new(file);

    writeln!(
        w,
        "turn_idx,scope_id,scenario_id,language,passed,assertion_count,failed_count"
    )
    .map_err(|e| format!("write turns.csv header: {e}"))?;

    for tv in &report.turn_verifications {
        let failed = tv.assertions.iter().filter(|a| !a.passed).count();
        writeln!(
            w,
            "{},{},{},{},{},{},{}",
            tv.turn_idx,
            tv.scope_id,
            csv_escape(&tv.scenario_id),
            tv.language,
            tv.passed,
            tv.assertions.len(),
            failed
        )
        .map_err(|e| format!("write turns.csv row: {e}"))?;
    }

    let bytes = w.into_inner()
        .map_err(|e| format!("flush turns.csv: {e}"))?
        .metadata()
        .map_err(|e| format!("metadata turns.csv: {e}"))?
        .len() as usize;
    Ok(bytes)
}

/// Write every assertion as its own row.
fn write_assertions_csv(out_dir: &Path, report: &SimReport) -> Result<usize, String> {
    let path = out_dir.join("assertions.csv");
    let file = fs::File::create(&path).map_err(|e| format!("create assertions.csv: {e}"))?;
    let mut w = BufWriter::new(file);

    writeln!(
        w,
        "turn_idx,scope_id,scenario_id,language,assertion_name,passed,detail"
    )
    .map_err(|e| format!("write assertions.csv header: {e}"))?;

    for tv in &report.turn_verifications {
        for a in &tv.assertions {
            writeln!(
                w,
                "{},{},{},{},{},{},{}",
                tv.turn_idx,
                tv.scope_id,
                csv_escape(&tv.scenario_id),
                tv.language,
                csv_escape(&a.name),
                a.passed,
                csv_escape(a.detail.as_deref().unwrap_or(""))
            )
            .map_err(|e| format!("write assertions.csv row: {e}"))?;
        }
    }

    let bytes = w.into_inner()
        .map_err(|e| format!("flush assertions.csv: {e}"))?
        .metadata()
        .map_err(|e| format!("metadata assertions.csv: {e}"))?
        .len() as usize;
    Ok(bytes)
}

/// Write only failed assertions.
fn write_failures_csv(out_dir: &Path, report: &SimReport) -> Result<usize, String> {
    let path = out_dir.join("failures.csv");
    let file = fs::File::create(&path).map_err(|e| format!("create failures.csv: {e}"))?;
    let mut w = BufWriter::new(file);

    writeln!(
        w,
        "turn_idx,scope_id,scenario_id,language,assertion_name,detail"
    )
    .map_err(|e| format!("write failures.csv header: {e}"))?;

    for tv in &report.turn_verifications {
        for a in &tv.assertions {
            if !a.passed {
                writeln!(
                    w,
                    "{},{},{},{},{},{}",
                    tv.turn_idx,
                    tv.scope_id,
                    csv_escape(&tv.scenario_id),
                    tv.language,
                    csv_escape(&a.name),
                    csv_escape(a.detail.as_deref().unwrap_or(""))
                )
                .map_err(|e| format!("write failures.csv row: {e}"))?;
            }
        }
    }

    let bytes = w.into_inner()
        .map_err(|e| format!("flush failures.csv: {e}"))?
        .metadata()
        .map_err(|e| format!("metadata failures.csv: {e}"))?
        .len() as usize;
    Ok(bytes)
}

/// Write the observations sheet.
fn write_observations_csv(out_dir: &Path, rows: &[ObservationRow]) -> Result<usize, String> {
    let path = out_dir.join("observations.csv");
    let file = fs::File::create(&path).map_err(|e| format!("create observations.csv: {e}"))?;
    let mut w = BufWriter::new(file);

    writeln!(
        w,
        "turn_idx,scope_id,scenario_id,language,evidence_id,obs_type,content,expected_obs_type,match"
    )
    .map_err(|e| format!("write observations.csv header: {e}"))?;

    for r in rows {
        writeln!(
            w,
            "{},{},{},{},{},{},{},{},{}",
            r.turn_idx,
            r.scope_id,
            csv_escape(&r.scenario_id),
            r.language,
            r.evidence_id,
            csv_escape(&r.obs_type),
            csv_escape(&r.content),
            csv_escape(&r.expected_obs_type),
            r.type_match
        )
        .map_err(|e| format!("write observations.csv row: {e}"))?;
    }

    let bytes = w.into_inner()
        .map_err(|e| format!("flush observations.csv: {e}"))?
        .metadata()
        .map_err(|e| format!("metadata observations.csv: {e}"))?
        .len() as usize;
    Ok(bytes)
}

/// Write the memory state sheet.
fn write_memory_states_csv(out_dir: &Path, rows: &[MemoryStateRow]) -> Result<usize, String> {
    let path = out_dir.join("memory_states.csv");
    let file = fs::File::create(&path).map_err(|e| format!("create memory_states.csv: {e}"))?;
    let mut w = BufWriter::new(file);

    writeln!(
        w,
        "scope_id,memory_id,state,superseded_by,pin_count,retrieval_count,corroboration_count,sensitivity_class,content,archivable_under_policy"
    )
    .map_err(|e| format!("write memory_states.csv header: {e}"))?;

    for r in rows {
        writeln!(
            w,
            "{},{},{},{},{},{},{},{},{},{}",
            r.scope_id,
            r.memory_id,
            r.state,
            r.superseded_by.as_deref().unwrap_or(""),
            r.pin_count,
            r.retrieval_count,
            r.corroboration_count,
            csv_escape(&r.sensitivity_class),
            csv_escape(&r.content),
            r.archivable
        )
        .map_err(|e| format!("write memory_states.csv row: {e}"))?;
    }

    let bytes = w.into_inner()
        .map_err(|e| format!("flush memory_states.csv: {e}"))?
        .metadata()
        .map_err(|e| format!("metadata memory_states.csv: {e}"))?
        .len() as usize;
    Ok(bytes)
}

/// Write the retention score sheet.
fn write_retention_scores_csv(out_dir: &Path, rows: &[RetentionScoreRow]) -> Result<usize, String> {
    let path = out_dir.join("retention_scores.csv");
    let file = fs::File::create(&path).map_err(|e| format!("create retention_scores.csv: {e}"))?;
    let mut w = BufWriter::new(file);

    writeln!(
        w,
        "scope_id,memory_id,total,pinning,retrieval_frequency,corroboration,contradiction,age,non_use"
    )
    .map_err(|e| format!("write retention_scores.csv header: {e}"))?;

    for r in rows {
        writeln!(
            w,
            "{},{},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6}",
            r.scope_id,
            r.memory_id,
            r.total,
            r.pinning,
            r.retrieval_frequency,
            r.corroboration,
            r.contradiction,
            r.age,
            r.non_use
        )
        .map_err(|e| format!("write retention_scores.csv row: {e}"))?;
    }

    let bytes = w.into_inner()
        .map_err(|e| format!("flush retention_scores.csv: {e}"))?
        .metadata()
        .map_err(|e| format!("metadata retention_scores.csv: {e}"))?
        .len() as usize;
    Ok(bytes)
}

/// Write the synthesis window sheet.
fn write_synthesis_windows_csv(out_dir: &Path, rows: &[SynthesisWindowRow]) -> Result<usize, String> {
    let path = out_dir.join("synthesis_windows.csv");
    let file = fs::File::create(&path).map_err(|e| format!("create synthesis_windows.csv: {e}"))?;
    let mut w = BufWriter::new(file);

    writeln!(
        w,
        "scope_id,window_id,status,opened_at,closed_at,recap,decisions,open_questions,active_tasks"
    )
    .map_err(|e| format!("write synthesis_windows.csv header: {e}"))?;

    for r in rows {
        writeln!(
            w,
            "{},{},{},{},{},{},{},{},{}",
            r.scope_id,
            r.window_id,
            r.status,
            r.opened_at.as_deref().unwrap_or(""),
            r.closed_at.as_deref().unwrap_or(""),
            csv_escape(&r.recap),
            csv_escape(&r.decisions),
            csv_escape(&r.open_questions),
            csv_escape(&r.active_tasks)
        )
        .map_err(|e| format!("write synthesis_windows.csv row: {e}"))?;
    }

    let bytes = w.into_inner()
        .map_err(|e| format!("flush synthesis_windows.csv: {e}"))?
        .metadata()
        .map_err(|e| format!("metadata synthesis_windows.csv: {e}"))?
        .len() as usize;
    Ok(bytes)
}

/// All captured data for CSV export, populated during replay.
#[derive(Debug, Clone, Default)]
pub(crate) struct ExportData {
    pub(crate) observations: Vec<ObservationRow>,
    pub(crate) memory_states: Vec<MemoryStateRow>,
    pub(crate) retention_scores: Vec<RetentionScoreRow>,
    pub(crate) synthesis_windows: Vec<SynthesisWindowRow>,
}

/// Escape a CSV field: wrap in quotes if it contains a comma, quote, or
/// newline. Also defuse spreadsheet formula injection by prefixing fields
/// that begin with formula-triggering characters (`=`, `+`, `-`, `@`, tab)
/// with a single quote.
fn csv_escape(s: &str) -> String {
    let needs_quote =
        s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r');
    let escaped = if needs_quote {
        s.replace('"', "\"\"")
    } else {
        s.to_string()
    };

    // Prefix formula-triggering leading characters with a single quote so
    // Excel / LibreOffice treat the cell as text, not a formula.
    let mut chars = escaped.chars();
    let first = chars.next();
    let defused = match first {
        Some('=') | Some('+') | Some('-') | Some('@') | Some('\t') => {
            format!("'{escaped}")
        }
        _ if escaped.starts_with("\r\n") || escaped.starts_with('\r') => {
            format!("'{escaped}")
        }
        _ => escaped,
    };

    if needs_quote {
        format!("\"{defused}\"")
    } else {
        defused
    }
}

/// A single extracted observation for CSV export.
#[derive(Debug, Clone)]
pub(crate) struct ObservationRow {
    pub(crate) turn_idx: usize,
    pub(crate) scope_id: String,
    pub(crate) scenario_id: String,
    pub(crate) language: String,
    pub(crate) evidence_id: String,
    pub(crate) obs_type: String,
    pub(crate) content: String,
    pub(crate) expected_obs_type: String,
    pub(crate) type_match: bool,
}

/// A memory object state for CSV export.
#[derive(Debug, Clone)]
pub(crate) struct MemoryStateRow {
    pub(crate) scope_id: String,
    pub(crate) memory_id: String,
    pub(crate) state: String,
    pub(crate) superseded_by: Option<String>,
    pub(crate) pin_count: u32,
    pub(crate) retrieval_count: u32,
    pub(crate) corroboration_count: u32,
    pub(crate) sensitivity_class: String,
    pub(crate) content: String,
    pub(crate) archivable: bool,
}

/// A per-object retention score for CSV export.
#[derive(Debug, Clone)]
pub(crate) struct RetentionScoreRow {
    pub(crate) scope_id: String,
    pub(crate) memory_id: String,
    pub(crate) total: f64,
    pub(crate) pinning: f64,
    pub(crate) retrieval_frequency: f64,
    pub(crate) corroboration: f64,
    pub(crate) contradiction: f64,
    pub(crate) age: f64,
    pub(crate) non_use: f64,
}

/// A synthesis window for CSV export.
#[derive(Debug, Clone)]
pub(crate) struct SynthesisWindowRow {
    pub(crate) scope_id: String,
    pub(crate) window_id: String,
    pub(crate) status: String,
    pub(crate) opened_at: Option<String>,
    pub(crate) closed_at: Option<String>,
    pub(crate) recap: String,
    pub(crate) decisions: String,
    pub(crate) open_questions: String,
    pub(crate) active_tasks: String,
}
