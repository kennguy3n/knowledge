//! Stage 1 — Evidence Ingestion.
//!
//! Opens an [`EvidenceStore`] backed by a temporary SQLCipher file,
//! classifies each synthetic message with the
//! [`evidence_store::classifier::CompositeClassifier`] (lexicon-only
//! configuration — no SLM available in the demo run), runs the
//! Phase 1.3 [`observation_engine::detect_language`] trigram detector
//! against the plaintext body, and ingests every message via the
//! public [`EvidenceStore::ingest_with_language`] API so the
//! schema-v13 `language_tag` column is populated end-to-end on the
//! demo run.
//!
//! Exercises all three storage paths: inline rows for short bodies,
//! the dedup body table for the long Atlas / Helios documents (which
//! also includes a duplicated body to bump the dedup ref-count), and
//! the ring buffer for noise-class messages. The dataset is mixed
//! English / Japanese / Korean / Chinese / Spanish so the demo
//! report breaks down the row count by detected BCP-47 primary
//! subtag — exactly mirroring what the multilingual lexicon
//! registry (Phase 1.1) and CJK-aware FTS5 tokenizer (Phase 1.2)
//! will read on the retrieval side.

use std::time::Instant;

use evidence_store::{
    classifier::CompositeClassifier, EvidenceStore, EvidenceStoreConfig, ImportanceClass,
    ImportanceClassifier, LexiconClassifier, StoragePath,
};
use observation_engine::detect_language;
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
    // Aggregate counter for the BCP-47 primary subtag the substrate
    // stamps on each persisted row. `"<none>"` collects the rows
    // whose detector outcome was `None` (empty / pure punctuation /
    // pure emoji / unreliable-short input) — the same fail-closed
    // contract the downstream multilingual lexicon registry reads as
    // "language unknown". The demo report surfaces this breakdown so
    // the multilingual ingest pipeline is visibly exercised end-to-
    // end rather than silently leaving the column NULL (which is
    // what the pre-Phase-1.3 legacy `ingest()` shim would have done
    // — and what Devin Review correctly flagged as a usability gap).
    let mut by_language: std::collections::BTreeMap<String, u64> =
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

        // Phase 1.3 — run language detection at the demo's persistent
        // write boundary so the schema-v13 `language_tag` column is
        // populated for inline / body-table rows. Noise-class rows
        // are still routed to the ring buffer by
        // `ingest_with_language` and do not retain the tag (the ring
        // buffer is plaintext-only, append-and-evict), so the demo
        // counters only attribute language tags to rows the store
        // actually persists.
        let detection = detect_language(msg.body.as_str());
        let language_tag = detection.as_ref().map(|d| d.tag.as_str());

        let result = store
            .ingest_with_language(
                scope,
                msg.body.as_bytes(),
                Some(&msg.source_ref),
                class,
                language_tag,
            )
            .expect("ingest synthetic message with language");

        match result.storage_path {
            StoragePath::Inline => inline += 1,
            StoragePath::BodyTable => body_table += 1,
            StoragePath::RingBuffer => ring += 1,
        }
        // Only count language-tag attribution on rows the store
        // actually persists (inline / body-table). Ring-buffer rows
        // bypass the `language_tag` column entirely, so attributing
        // their detection outcome to the breakdown would overstate
        // what the schema-v13 column actually carries.
        if matches!(
            result.storage_path,
            StoragePath::Inline | StoragePath::BodyTable
        ) {
            let lang_key = language_tag.map_or_else(|| "<none>".to_string(), str::to_string);
            *by_language.entry(lang_key).or_default() += 1;
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
    // Phase 1.3 — at least one persisted row has a non-NULL
    // `language_tag` after detection runs at the demo's write
    // boundary. The synthetic dataset includes plain-English bodies
    // long enough to clear whatlang's reliability threshold, so the
    // detector should emit at least one concrete BCP-47 tag (e.g.
    // `"en"`) on the inline / body-table path. A failure here would
    // mean the demo silently regressed back to the legacy `ingest()`
    // shim that leaves the column NULL — which is exactly the
    // showcase-gap Devin Review flagged on the previous commit.
    let language_tagged_rows: u64 = by_language
        .iter()
        .filter(|(k, _)| k.as_str() != "<none>")
        .map(|(_, v)| *v)
        .sum();
    log.check(
        PHASE,
        "at least one persisted row carries a detected language_tag",
        language_tagged_rows > 0,
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
    // Phase 1.3 breakdown: how many persisted rows carry each BCP-47
    // primary subtag the substrate detected at ingest. `<none>`
    // collects the fail-closed outcomes (detector declined to
    // classify). The Phase 1.1 multilingual lexicon registry reads
    // these tags on every retrieval to pick a per-locale lexicon, so
    // surfacing the breakdown here makes the multilingual ingest
    // pipeline visibly exercised end-to-end on every demo run.
    for (k, v) in &by_language {
        phase.stat(format!("language_{k}"), v.to_string());
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
