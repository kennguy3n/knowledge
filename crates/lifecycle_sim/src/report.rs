//! JSON + markdown report generation.

use std::path::Path;

use crate::replay::SimReport;

/// Report output paths.
pub struct ReportOutput {
    /// Path to the JSON report.
    pub json_path: String,
    /// Path to the markdown report.
    pub md_path: String,
}

/// Write both JSON and markdown reports to `dir`.
pub fn write_reports(report: &SimReport, dir: &str) -> Result<ReportOutput, String> {
    let dir_path = Path::new(dir);
    std::fs::create_dir_all(dir_path).map_err(|e| format!("mkdir error: {e}"))?;

    let json_path = dir_path.join("lifecycle_report.json");
    let md_path = dir_path.join("lifecycle_report.md");

    // Write JSON.
    let json = serde_json::to_string_pretty(report)
        .map_err(|e| format!("json serialize error: {e}"))?;
    std::fs::write(&json_path, json).map_err(|e| format!("json write error: {e}"))?;

    // Write markdown.
    let md = generate_markdown(report);
    std::fs::write(&md_path, md).map_err(|e| format!("md write error: {e}"))?;

    Ok(ReportOutput {
        json_path: json_path.to_string_lossy().to_string(),
        md_path: md_path.to_string_lossy().to_string(),
    })
}

fn generate_markdown(report: &SimReport) -> String {
    let mut md = String::new();

    md.push_str("# Lifecycle Simulation Report\n\n");
    md.push_str(&format!(
        "**Preset:** {} ({}) | **Driver:** {} | **Seed:** {} | **Duration:** {:.1}s\n\n",
        report.run_config.preset,
        report.run_config.scale,
        report.run_config.driver,
        report.run_config.seed,
        report.summary.duration_secs,
    ));

    // Summary table.
    md.push_str("## Summary\n\n");
    md.push_str("| Metric | Value |\n");
    md.push_str("|--------|-------|\n");
    md.push_str(&format!("| Total turns | {} |\n", report.summary.total_turns));
    md.push_str(&format!("| Pass rate | {:.2}% |\n", report.summary.pass_rate * 100.0));
    md.push_str(&format!(
        "| Failed assertions | {} |\n",
        report.summary.failed_assertions
    ));
    md.push_str(&format!(
        "| Total assertions | {} |\n",
        report.summary.total_assertions
    ));
    md.push_str(&format!("| Tenants | {} |\n", report.summary.total_tenants));
    md.push_str(&format!("| Users | {} |\n", report.summary.total_users));
    md.push_str(&format!("| Scopes | {} |\n", report.summary.total_scopes));
    md.push_str(&format!(
        "| Assertions/turn | {:.1} |\n",
        if report.summary.total_turns > 0 {
            report.summary.total_assertions as f64 / report.summary.total_turns as f64
        } else {
            0.0
        }
    ));
    md.push_str(&format!(
        "| Throughput (turns/s) | {:.1} |\n",
        if report.summary.duration_secs > 0.0 {
            report.summary.total_turns as f64 / report.summary.duration_secs
        } else {
            0.0
        }
    ));
    md.push('\n');

    // Assertion breakdown by type.
    md.push_str("## Assertion Breakdown\n\n");
    md.push_str("| Assertion Type | Count | Passed | Failed |\n");
    md.push_str("|----------------|-------|--------|--------|\n");
    let mut assertion_stats: std::collections::HashMap<String, (usize, usize)> =
        std::collections::HashMap::new();
    for tv in &report.turn_verifications {
        for a in &tv.assertions {
            let entry = assertion_stats.entry(a.name.clone()).or_insert((0, 0));
            entry.0 += 1;
            if a.passed {
                entry.1 += 1;
            }
        }
    }
    let mut sorted_assertions: Vec<_> = assertion_stats.iter().collect();
    sorted_assertions.sort_by(|a, b| b.1.0.cmp(&a.1.0));
    for (name, (total, passed)) in &sorted_assertions {
        let failed = total - passed;
        md.push_str(&format!("| {} | {} | {} | {} |\n", name, total, passed, failed));
    }
    md.push('\n');

    // Per-tenant results.
    if !report.per_tenant.is_empty() {
        md.push_str("## Per-Tenant Results\n\n");
        md.push_str("| Tenant | Turns | Pass Rate |\n");
        md.push_str("|--------|-------|-----------|\n");
        for t in &report.per_tenant {
            md.push_str(&format!(
                "| {} | {} | {:.2}% |\n",
                t.tenant_id,
                t.turns,
                t.pass_rate * 100.0
            ));
        }
        md.push('\n');
    }

    // Per-language results.
    if !report.per_language.is_empty() {
        md.push_str("## Per-Language Results\n\n");
        md.push_str("| Language | Messages | Obs/Msg | Pass Rate |\n");
        md.push_str("|----------|----------|---------|-----------|\n");
        for l in &report.per_language {
            md.push_str(&format!(
                "| {} | {} | {:.2} | {:.2}% |\n",
                l.language,
                l.messages,
                l.obs_per_msg,
                l.pass_rate * 100.0
            ));
        }
        md.push('\n');
    }

    // Per-scenario results.
    if !report.per_scenario.is_empty() {
        md.push_str("## Per-Scenario Results\n\n");
        md.push_str("| Scenario | Turns | Pass Rate |\n");
        md.push_str("|----------|-------|-----------|\n");
        for s in &report.per_scenario {
            md.push_str(&format!(
                "| {} | {} | {:.2}% |\n",
                s.scenario,
                s.instances,
                s.pass_rate * 100.0
            ));
        }
        md.push('\n');
    }

    // Phase 4 metrics section: synthesis, memory, reasoning, health check.
    md.push_str("## Lifecycle Metrics\n\n");
    let synthesis_assertions: usize = report
        .turn_verifications
        .iter()
        .flat_map(|tv| tv.assertions.iter())
        .filter(|a| a.name.starts_with("synthesis_"))
        .count();
    let memory_assertions: usize = report
        .turn_verifications
        .iter()
        .flat_map(|tv| tv.assertions.iter())
        .filter(|a| a.name.starts_with("memory_"))
        .count();
    let reasoning_assertions: usize = report
        .turn_verifications
        .iter()
        .flat_map(|tv| tv.assertions.iter())
        .filter(|a| a.name.starts_with("reasoning_"))
        .count();
    let health_assertions: usize = report
        .turn_verifications
        .iter()
        .flat_map(|tv| tv.assertions.iter())
        .filter(|a| a.name.starts_with("health_check_"))
        .count();
    let language_assertions: usize = report
        .turn_verifications
        .iter()
        .flat_map(|tv| tv.assertions.iter())
        .filter(|a| a.name.starts_with("language_"))
        .count();
    let concept_graph_assertions: usize = report
        .turn_verifications
        .iter()
        .flat_map(|tv| tv.assertions.iter())
        .filter(|a| a.name.starts_with("concept_graph_"))
        .count();
    let checkpoint_assertions: usize = report
        .turn_verifications
        .iter()
        .flat_map(|tv| tv.assertions.iter())
        .filter(|a| a.name.starts_with("checkpoint_"))
        .count();
    let other_tenant_assertions: usize = report
        .turn_verifications
        .iter()
        .flat_map(|tv| tv.assertions.iter())
        .filter(|a| a.name.starts_with("other_tenants_"))
        .count();

    md.push_str("| Category | Assertions |\n");
    md.push_str("|----------|------------|\n");
    md.push_str(&format!("| Synthesis | {} |\n", synthesis_assertions));
    md.push_str(&format!("| Memory lifecycle | {} |\n", memory_assertions));
    md.push_str(&format!("| Reasoning | {} |\n", reasoning_assertions));
    md.push_str(&format!("| Language tags | {} |\n", language_assertions));
    md.push_str(&format!("| Concept graph | {} |\n", concept_graph_assertions));
    md.push_str(&format!("| Health check | {} |\n", health_assertions));
    md.push_str(&format!("| Checkpoint/restore | {} |\n", checkpoint_assertions));
    md.push_str(&format!("| Other tenants unaffected | {} |\n", other_tenant_assertions));
    md.push('\n');

    // Failures.
    if !report.failures.is_empty() {
        md.push_str("## Failures (top 100)\n\n");
        md.push_str("| Turn | Scope | Assertion | Detail |\n");
        md.push_str("|------|-------|-----------|--------|\n");
        for f in &report.failures {
            md.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                f.turn, f.scope, f.assertion, f.actual
            ));
        }
        md.push('\n');
    } else {
        md.push_str("## Failures\n\nNo failures detected. All assertions passed.\n\n");
    }

    md
}
