//! Lightweight pass/fail assertion log used by the demo phases.

#[derive(Debug, Clone)]
pub struct AssertionRecord {
    pub phase: &'static str,
    pub label: String,
    pub passed: bool,
    pub detail: Option<String>,
}

#[derive(Debug, Default)]
pub struct AssertionLog {
    records: Vec<AssertionRecord>,
}

impl AssertionLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, phase: &'static str, label: impl Into<String>, passed: bool) {
        self.records.push(AssertionRecord {
            phase,
            label: label.into(),
            passed,
            detail: None,
        });
    }

    pub fn check(&mut self, phase: &'static str, label: impl Into<String>, condition: bool) {
        self.record(phase, label, condition);
    }

    pub fn records(&self) -> &[AssertionRecord] {
        &self.records
    }

    pub fn total_count(&self) -> usize {
        self.records.len()
    }

    pub fn passed_count(&self) -> usize {
        self.records.iter().filter(|r| r.passed).count()
    }

    pub fn failed_count(&self) -> usize {
        self.records.iter().filter(|r| !r.passed).count()
    }

    pub fn has_failures(&self) -> bool {
        self.failed_count() > 0
    }
}
