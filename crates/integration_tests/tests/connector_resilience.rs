//! Cursor-preservation chaos tests for the connector sync path.
//!
//! These tests drive the real [`GitHubConnector`] through a
//! [`MockHttpTransport`] (no network) while injecting failures
//! mid-sync — a server error on a later page, repeated outages, and a
//! brand-new record arriving *at the watermark instant during* an
//! outage. They assert the resilience contract that the
//! [`WatermarkCursor`](connector_framework::WatermarkCursor) exists to
//! guarantee:
//!
//! 1. A sync run is **atomic**: when any page fails the connector
//!    returns `Err` and emits *no* events, so the host never advances
//!    the cursor on a partial pull.
//! 2. The cursor is **preserved** across the failure (the host keeps
//!    the prior cursor on `mark_failed`).
//! 3. A resumed run re-queries inclusively (`since = watermark`) and
//!    the cursor's boundary-id set drops only the ids already emitted,
//!    so the dedup invariant holds — **no evidence is lost or
//!    duplicated** regardless of where the interruption landed or how
//!    many times the run was retried.
//!
//! The "evidence" is modelled concretely: every emitted event is
//! ingested into a real [`EvidenceStore`] keyed by a `github:<number>`
//! source ref, and an ingest-count map is the authoritative dedup
//! signal. Reuses the `test-support` helpers (`open_store`,
//! `padded_body`) per the crate's integration-test conventions.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    AuthKind, Connector, ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId,
    ConnectorKind, HttpMethod, HttpTransport, MockHttpTransport, MockResponse, OAuth2CodeExchange,
    OAuth2Token, Result as ConnectorResult, SyncMode, SyncState, WatermarkCursor,
};
use connectors::github::{GitHubConnector, DEFAULT_PAGE_SIZE};
use evidence_store::{EvidenceStore, ImportanceClass, ScopeId};
use integration_tests::test_helpers::{open_store, padded_body};
use tempfile::TempDir;
use uuid::Uuid;

const BASE_URL: &str = "https://api.test/github";
const REPO: &str = "owner/test-repo";

/// Watermark instant of the prior (successful) run. Issue #1 was
/// observed at exactly this second and is recorded in the cursor's
/// boundary set, so a re-query that re-sees it must drop it.
const WATERMARK: &str = "2024-03-01T00:00:00+00:00";
/// A strictly-newer instant used for genuinely new updates.
const NEWER: &str = "2024-06-01T00:00:00+00:00";

/// Fixed-token OAuth exchange — mirrors the harness in
/// `connector_sync_cycle.rs` so the connector authenticates without a
/// browser hop.
struct FixedOAuth;

impl OAuth2CodeExchange for FixedOAuth {
    fn exchange_code(
        &self,
        _config: &ConnectorConfig,
        _code: &str,
    ) -> ConnectorResult<OAuth2Token> {
        Ok(OAuth2Token::new(
            "gh-access",
            "gh-refresh",
            Utc::now() + chrono::Duration::hours(1),
            "repo",
        ))
    }
}

fn oauth() -> Arc<dyn OAuth2CodeExchange> {
    Arc::new(FixedOAuth)
}

fn cfg() -> ConnectorConfig {
    ConnectorConfig::new(ConnectorKind::GitHub, AuthKind::OAuth2, ScopeId::new_v4())
        .with_auth_config(serde_json::json!({
            "authorization_code": "demo-code",
            "api_base_url": BASE_URL,
            "repository": REPO,
        }))
}

fn connector(transport: MockHttpTransport) -> GitHubConnector {
    let transport: Arc<dyn HttpTransport> = Arc::new(transport);
    GitHubConnector::new(ConnectorInstanceId::new_v4(), transport, oauth())
}

/// One issue as the Issues API serialises it. Only the fields the
/// connector decodes are populated.
fn issue_json(number: u64, state: &str, updated: &str) -> serde_json::Value {
    serde_json::json!({
        "number": number,
        "id": number,
        "title": format!("Issue #{number}"),
        "state": state,
        "created_at": updated,
        "updated_at": updated,
    })
}

/// The page-1 issues-list URL for an incremental pull, matching
/// `GitHubConnector::issues_page_url` byte-for-byte (the `since` value
/// is percent-encoded exactly as the connector encodes it).
fn incremental_page1_url(cursor: &str) -> String {
    let since = WatermarkCursor::parse(Some(cursor))
        .query_since()
        .expect("prior cursor must carry a watermark");
    let enc = connector_framework::percent_encode_path_component(&since);
    format!(
        "{BASE_URL}/repos/{REPO}/issues?state=all&sort=updated&direction=asc\
         &per_page={DEFAULT_PAGE_SIZE}&page=1&since={enc}"
    )
}

/// An opaque page-2 cursor URL. The connector follows the
/// `Link: rel="next"` URL verbatim, so the exact string is ours to
/// choose — which side-steps any dependence on `since`-encoding for
/// pages past the first.
fn page2_url() -> String {
    format!("{BASE_URL}/repos/{REPO}/issues?page=2&_cursor=opaque")
}

/// A 200 OK JSON page carrying a `Link: rel="next"` header so the
/// connector walks on to `next`.
fn page_with_next(body: Vec<u8>, next: &str) -> MockResponse {
    MockResponse {
        status: 200,
        headers: vec![
            ("content-type".into(), "application/json".into()),
            ("link".into(), format!("<{next}>; rel=\"next\"")),
        ],
        body,
    }
}

fn issues_body(issues: &[serde_json::Value]) -> Vec<u8> {
    serde_json::to_vec(issues).expect("serialise issues page")
}

/// The substrate side of the host loop: a real encrypted store plus an
/// ingest-count map keyed by source ref. The map is the dedup oracle —
/// any id ingested more than once is a duplication bug.
struct Substrate {
    // Held so the encrypted store's backing file is removed when the
    // test ends, matching the crate's `tempfile::tempdir()` convention.
    _dir: TempDir,
    store: EvidenceStore,
    scope: ScopeId,
    ingests: BTreeMap<String, usize>,
}

impl Substrate {
    fn open() -> Self {
        let dir = tempfile::tempdir().expect("create tempdir");
        let store = open_store(&dir.path().join("evidence.db"));
        Self {
            _dir: dir,
            store,
            scope: ScopeId::from_uuid(Uuid::new_v4()),
            ingests: BTreeMap::new(),
        }
    }

    /// Ingest one emitted event, recording the source ref so duplicate
    /// emissions are observable.
    fn ingest_event(&mut self, event: &ConnectorEvent) {
        let id = event.document_id().as_str();
        let source_ref = format!("github:{id}");
        let body = padded_body(&format!("issue {id} body"));
        self.store
            .ingest(
                self.scope,
                &body,
                Some(&source_ref),
                ImportanceClass::Useful,
            )
            .expect("ingest emitted evidence");
        *self.ingests.entry(source_ref).or_insert(0) += 1;
    }

    /// Apply a successful sync result: ingest every emitted event.
    fn apply(&mut self, events: &[ConnectorEvent]) {
        for event in events {
            self.ingest_event(event);
        }
    }
}

/// A prior cursor whose watermark is [`WATERMARK`] with issue #1
/// already recorded in the boundary set.
fn prior_cursor() -> String {
    let mut cursor = WatermarkCursor::empty();
    let w: DateTime<Utc> = DateTime::parse_from_rfc3339(WATERMARK)
        .unwrap()
        .with_timezone(&Utc);
    cursor.observe(w, "1");
    cursor.to_cursor_string().expect("prior cursor serialises")
}

/// Seed `state` for an incremental resume from [`prior_cursor`].
fn incremental_state(cursor: &str) -> SyncState {
    let mut state = SyncState::new(ConnectorInstanceId::new_v4());
    state.mark_in_progress();
    state.mark_succeeded(Some(cursor.to_string()), Utc::now());
    assert_eq!(state.mode, SyncMode::Incremental);
    state
}

/// A server error on a later page must abort the whole run with `Err`,
/// leave the cursor untouched, and emit nothing — then a resume from
/// the preserved cursor must surface exactly the un-seen records once,
/// dropping the boundary record already emitted by the prior run.
#[test]
fn incremental_failure_midway_preserves_cursor_and_dedups_on_resume() {
    let cursor = prior_cursor();
    let mut sub = Substrate::open();

    // The prior successful run already ingested issue #1 at the
    // watermark. Model that so the end-state is verifiable.
    sub.ingest_event(&ConnectorEvent::DocumentUpdated {
        document_id: connector_framework::SourceDocumentId::new("1"),
        occurred_at: DateTime::parse_from_rfc3339(WATERMARK)
            .unwrap()
            .with_timezone(&Utc),
    });

    let page1 = issues_body(&[
        // #1 @ watermark, already seen → must be deduped.
        issue_json(1, "open", WATERMARK),
        // #3 @ watermark, brand-new boundary id → must be surfaced.
        issue_json(3, "open", WATERMARK),
    ]);
    let page2 = issues_body(&[issue_json(2, "open", NEWER)]);

    // ── Round 1: page 1 OK, page 2 explodes with a 500. ──
    let mut state = incremental_state(&cursor);
    {
        let transport = MockHttpTransport::new();
        transport.expect(
            HttpMethod::Get,
            incremental_page1_url(&cursor),
            page_with_next(page1.clone(), &page2_url()),
        );
        transport.expect(
            HttpMethod::Get,
            page2_url(),
            MockResponse::status(500, br#"{"message":"server error"}"#.to_vec()),
        );
        let c = connector(transport);
        let tok = c.authenticate(&cfg()).unwrap();

        state.mark_in_progress();
        let err = c
            .incremental_sync(&cfg(), &tok, &state)
            .expect_err("page-2 failure must abort the sync");
        assert!(
            matches!(err, ConnectorError::Sync(_)),
            "5xx maps to a retriable Sync error, got {err:?}"
        );
        // The host records the failure: the cursor is NOT advanced.
        state.mark_failed(err.to_string());
        assert_eq!(
            state.cursor.as_deref(),
            Some(cursor.as_str()),
            "cursor must survive a mid-sync failure untouched"
        );
    }

    // Nothing beyond the prior run's issue #1 has been ingested.
    assert_eq!(sub.ingests.get("github:3"), None);
    assert_eq!(sub.ingests.get("github:2"), None);

    // ── Round 2: both pages succeed; resume from the preserved cursor. ──
    {
        let transport = MockHttpTransport::new();
        transport.expect(
            HttpMethod::Get,
            incremental_page1_url(&cursor),
            page_with_next(page1, &page2_url()),
        );
        transport.expect(HttpMethod::Get, page2_url(), MockResponse::ok_json(page2));
        let c = connector(transport);
        let tok = c.authenticate(&cfg()).unwrap();

        state.mark_in_progress();
        let res = c
            .incremental_sync(&cfg(), &tok, &state)
            .expect("resumed sync must succeed");

        let ids: Vec<&str> = res
            .events
            .iter()
            .map(|e| e.document_id().as_str())
            .collect();
        // #1 deduped (already seen at the watermark); #3 (new boundary)
        // and #2 (strictly newer) surfaced exactly once, in order.
        assert_eq!(
            ids,
            vec!["3", "2"],
            "dedup invariant: only un-seen ids emit"
        );

        sub.apply(&res.events);
        state.mark_succeeded(res.next_cursor, Utc::now());
    }

    // Every document ingested exactly once across the whole lifecycle —
    // no loss, no duplication.
    assert_eq!(sub.ingests.get("github:1"), Some(&1));
    assert_eq!(sub.ingests.get("github:3"), Some(&1));
    assert_eq!(sub.ingests.get("github:2"), Some(&1));

    // Cursor advanced to the newest instant with issue #2 as the new
    // boundary set; #1/#3 (now below the watermark) are dropped.
    let advanced = WatermarkCursor::parse(state.cursor.as_deref());
    assert_eq!(advanced.watermark(), Some(parse(NEWER)));
    assert!(
        !advanced.should_emit(parse(NEWER), "2"),
        "newest boundary id is recorded"
    );
    assert!(
        !advanced.should_emit(parse(WATERMARK), "3"),
        "below-watermark records never re-emit"
    );
}

/// Repeated outages must not corrupt the cursor: no matter how many
/// times the resumed run fails before finally succeeding, every record
/// is ingested exactly once.
#[test]
fn repeated_interruptions_still_ingest_each_record_once() {
    let cursor = prior_cursor();
    let mut sub = Substrate::open();
    let mut state = incremental_state(&cursor);

    let page = issues_body(&[
        issue_json(1, "open", WATERMARK), // already seen → deduped every attempt
        issue_json(2, "open", NEWER),     // new → must surface once
    ]);

    // Two failed attempts (transport returns 503), then success. The
    // single-page pull fails wholesale each time it 503s.
    for _ in 0..2 {
        let transport = MockHttpTransport::new();
        transport.expect(
            HttpMethod::Get,
            incremental_page1_url(&cursor),
            MockResponse::status(503, br#"{"message":"unavailable"}"#.to_vec()),
        );
        let c = connector(transport);
        let tok = c.authenticate(&cfg()).unwrap();
        state.mark_in_progress();
        let err = c
            .incremental_sync(&cfg(), &tok, &state)
            .expect_err("503 aborts");
        state.mark_failed(err.to_string());
        assert_eq!(state.cursor.as_deref(), Some(cursor.as_str()));
    }

    let transport = MockHttpTransport::new();
    transport.expect(
        HttpMethod::Get,
        incremental_page1_url(&cursor),
        MockResponse::ok_json(page),
    );
    let c = connector(transport);
    let tok = c.authenticate(&cfg()).unwrap();
    state.mark_in_progress();
    let res = c
        .incremental_sync(&cfg(), &tok, &state)
        .expect("eventual success");
    sub.apply(&res.events);
    state.mark_succeeded(res.next_cursor, Utc::now());

    let ids: Vec<&str> = res
        .events
        .iter()
        .map(|e| e.document_id().as_str())
        .collect();
    assert_eq!(
        ids,
        vec!["2"],
        "only the un-seen record surfaces, after any number of retries"
    );
    assert_eq!(sub.ingests.get("github:2"), Some(&1));
    assert_eq!(
        sub.ingests.get("github:1"),
        None,
        "boundary record never re-emitted"
    );
}

/// A brand-new record can land at the *exact* watermark instant while
/// the connector is mid-outage. On resume the inclusive re-query must
/// surface that new boundary record (it was never emitted) while still
/// dropping the boundary record the prior run already emitted.
#[test]
fn new_boundary_record_during_outage_is_not_lost() {
    let cursor = prior_cursor();
    let mut sub = Substrate::open();
    let mut state = incremental_state(&cursor);

    // First attempt: the only page 500s — total failure, cursor intact.
    {
        let transport = MockHttpTransport::new();
        transport.expect(
            HttpMethod::Get,
            incremental_page1_url(&cursor),
            MockResponse::status(500, br#"{"message":"boom"}"#.to_vec()),
        );
        let c = connector(transport);
        let tok = c.authenticate(&cfg()).unwrap();
        state.mark_in_progress();
        let err = c
            .incremental_sync(&cfg(), &tok, &state)
            .expect_err("500 aborts");
        state.mark_failed(err.to_string());
        assert_eq!(state.cursor.as_deref(), Some(cursor.as_str()));
    }

    // During the outage, issue #4 was created at the same watermark
    // second as the already-seen #1. The resumed re-query returns both.
    let page = issues_body(&[
        issue_json(1, "open", WATERMARK), // seen → dedup
        issue_json(4, "open", WATERMARK), // new boundary record → keep
    ]);
    {
        let transport = MockHttpTransport::new();
        transport.expect(
            HttpMethod::Get,
            incremental_page1_url(&cursor),
            MockResponse::ok_json(page),
        );
        let c = connector(transport);
        let tok = c.authenticate(&cfg()).unwrap();
        state.mark_in_progress();
        let res = c
            .incremental_sync(&cfg(), &tok, &state)
            .expect("resume succeeds");
        sub.apply(&res.events);
        state.mark_succeeded(res.next_cursor, Utc::now());

        let ids: Vec<&str> = res
            .events
            .iter()
            .map(|e| e.document_id().as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["4"],
            "new boundary record surfaced; seen record dropped"
        );
    }

    assert_eq!(sub.ingests.get("github:4"), Some(&1));
    assert_eq!(sub.ingests.get("github:1"), None);

    // The advanced cursor remembers BOTH boundary ids at the watermark
    // so a subsequent run drops them both.
    let advanced = WatermarkCursor::parse(state.cursor.as_deref());
    assert_eq!(advanced.watermark(), Some(parse(WATERMARK)));
    assert!(!advanced.should_emit(parse(WATERMARK), "1"));
    assert!(!advanced.should_emit(parse(WATERMARK), "4"));
    assert!(
        advanced.should_emit(parse(WATERMARK), "99"),
        "a further new boundary id still emits"
    );
}

fn parse(ts: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(ts)
        .unwrap()
        .with_timezone(&Utc)
}
