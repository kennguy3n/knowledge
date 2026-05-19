//! Stage 1 — Evidence Ingestion.
//!
//! Opens an [`EvidenceStore`] backed by a temporary SQLCipher file,
//! classifies each synthetic message with the
//! [`evidence_store::classifier::CompositeClassifier`] (lexicon-only
//! configuration — no SLM available in the demo run), and ingests
//! every message via the public [`EvidenceStore::ingest`] API.
//!
//! Exercises all three storage paths: inline rows for short bodies,
//! the dedup body table for the long Atlas / Helios documents (which
//! also includes a duplicated body to bump the dedup ref-count), and
//! the ring buffer for noise-class messages.

use std::time::Instant;

use evidence_store::{
    classifier::CompositeClassifier, EvidenceStore, EvidenceStoreConfig, ImportanceClass,
    ImportanceClassifier, LexiconClassifier, StoragePath,
};
use tempfile::TempDir;

use crate::assertions::AssertionLog;
use crate::dataset::{Dataset, ScopeTier, SyntheticMessage};
use crate::phases::runtime::{IngestedRow, RuntimeState};
use crate::report::{DemoReport, PhaseReport};

const PHASE: &str = "evidence";

pub fn run(
    dataset: &Dataset,
    state: &mut RuntimeState,
    report: &mut DemoReport,
    log: &mut AssertionLog,
) {
    let started = Instant::now();
    let mut phase = PhaseReport::new("Stage 1: Evidence Ingestion");

    let temp = TempDir::new().expect("create demo evidence tempdir");
    let db_path = temp.path().join("evidence.sqlcipher");

    let mut store =
        EvidenceStore::open(&db_path, &state.master_key, EvidenceStoreConfig::default())
            .expect("open evidence store");

    let classifier = CompositeClassifier::lexicon_only(LexiconClassifier::english_default());

    let mut inline = 0u64;
    let mut body_table = 0u64;
    let mut ring = 0u64;
    let mut by_class: std::collections::BTreeMap<&'static str, u64> =
        std::collections::BTreeMap::new();
    let mut ingested: Vec<IngestedRow> = Vec::new();

    let bench_started = Instant::now();
    let mut ingest_count: u64 = 0;
    for msg in &dataset.messages {
        let scope = scope_id_for(dataset, msg);
        let class: ImportanceClass = classifier.classify(&msg.body);
        let class_tag = match class {
            ImportanceClass::Critical => "critical",
            ImportanceClass::Important => "important",
            ImportanceClass::Useful => "useful",
            ImportanceClass::Noise => "noise",
        };
        *by_class.entry(class_tag).or_default() += 1;

        let result = store
            .ingest(scope, msg.body.as_bytes(), Some(&msg.source_ref), class)
            .expect("ingest synthetic message");

        match result.storage_path {
            StoragePath::Inline => inline += 1,
            StoragePath::BodyTable => body_table += 1,
            StoragePath::RingBuffer => ring += 1,
        }
        ingest_count += 1;

        if matches!(
            result.storage_path,
            StoragePath::Inline | StoragePath::BodyTable
        ) {
            ingested.push(IngestedRow {
                evidence_id: result.evidence_id,
                scope_id: scope,
                scope_label: msg.scope_label,
                scope_tier: msg.scope_tier,
                source_ref: msg.source_ref.clone(),
                body: msg.body.clone(),
                importance: class,
                occurred_at: msg.occurred_at,
            });
        }
    }
    let bench_total = bench_started.elapsed();

    let evidence_rows = store.evidence_count().expect("count evidence rows");
    let body_rows = store.body_store_count().expect("count body_store rows");
    let ring_size = store.ring_buffer_current_size().expect("ring buffer size") as u64;
    let ring_len = store.ring_buffer_len().expect("ring buffer len") as u64;

    log.check(
        PHASE,
        "evidence rows match inline+body_table ingest count",
        evidence_rows as u64 == inline + body_table,
    );
    log.check(PHASE, "at least one inline row was created", inline > 0);
    log.check(
        PHASE,
        "at least one body-table row was created",
        body_table > 0,
    );
    log.check(PHASE, "at least one ring-buffer row was created", ring > 0);
    log.check(
        PHASE,
        "ring buffer length matches noise count",
        ring_len == ring,
    );
    log.check(
        PHASE,
        "body_store dedup compresses duplicate long bodies",
        body_rows as u64 <= body_table,
    );
    log.check(PHASE, "ring buffer holds bytes", ring_size > 0);
    log.check(
        PHASE,
        "all four scope tiers contributed evidence",
        scope_tier_coverage(&ingested) == 4,
    );

    phase.timing = started.elapsed();
    phase.stat("messages", dataset.messages.len().to_string());
    phase.stat("inline_rows", inline.to_string());
    phase.stat("body_table_rows", body_table.to_string());
    phase.stat("ring_buffer_rows", ring.to_string());
    phase.stat("dedup_body_rows", body_rows.to_string());
    phase.stat("ring_buffer_bytes", ring_size.to_string());
    for (k, v) in &by_class {
        phase.stat(format!("class_{k}"), v.to_string());
    }
    phase.note(format!(
        "Stored evidence at {} (encrypted SQLCipher, master key derived in-process)",
        db_path.display()
    ));

    report.count("evidence_rows_total", evidence_rows as u64);
    report.count("evidence_rows_inline", inline);
    report.count("evidence_rows_body_table", body_table);
    report.count("evidence_rows_ring_buffer", ring);
    report.count("evidence_dedup_bodies", body_rows as u64);
    report.add_phase(phase);
    report.add_benchmark("evidence_ingest_per_message", ingest_count, bench_total);

    state.evidence_temp = Some(temp);
    state.evidence_store = Some(store);
    state.ingested_rows = ingested;
}

fn scope_tier_coverage(rows: &[IngestedRow]) -> usize {
    let mut tiers = std::collections::HashSet::new();
    for r in rows {
        tiers.insert(r.scope_tier);
    }
    tiers.len()
}

fn scope_id_for(dataset: &Dataset, msg: &SyntheticMessage) -> evidence_store::ScopeId {
    match msg.scope_tier {
        ScopeTier::User => dataset.user_scope.id,
        ScopeTier::Domain => dataset.domain_scope.id,
        ScopeTier::Tenant => dataset.tenant_scope.id,
        ScopeTier::Channel => {
            if msg.scope_label == dataset.channel_alt_scope.label {
                dataset.channel_alt_scope.id
            } else {
                dataset.channel_scope.id
            }
        }
    }
}
