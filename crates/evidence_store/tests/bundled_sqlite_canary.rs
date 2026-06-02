//! Canary tests pinning the SQLite version bundled by
//! `libsqlite3-sys` (via rusqlite's `bundled-sqlcipher-vendored-openssl`
//! feature) and the FTS5 tokeniser behaviours the multilingual lexical
//! lane depends on.
//!
//! ## Why this file exists
//!
//! The substrate links against SQLCipher's vendored copy of SQLite,
//! not the system SQLite, so any rusqlite / libsqlite3-sys bump can
//! silently move the bundled SQLite version. That matters because
//! the multilingual lexical lane is built on FTS5 tokenisers
//! (`unicode61`, `trigram`, and a CJK-aware dual-table design); a
//! point-release of SQLite has, historically, shipped subtle
//! tokeniser-behaviour fixes that would change recall against the
//! cross-lingual benchmark corpus.
//!
//! The existing `fts5_*` tests in `store_integration.rs` and the
//! `cross_lingual_recall_benchmark.rs` integration test do catch
//! observable recall regressions, but they don't tell a reviewer
//! *which* SQLite version is in play, so they can't tell whether a
//! Dependabot rusqlite bump moved the bundle or kept it pinned. This
//! file fills that gap: it asserts on the literal version string so
//! every rusqlite bump that moves the bundled SQLite forces a
//! deliberate maintainer ack via a literal change here, with a
//! pointer to the multilingual canary suite that must be re-run.
//!
//! ## What to do when these assertions fail
//!
//! 1. Run the full `evidence_store` test suite — every `fts5_*` test
//!    must still pass against the new bundled SQLite.
//! 2. Run `cross_lingual_recall_benchmark.rs` (the canary)
//!    — mean `recall@12 ≥ 0.99` and `hit-rate@{1,3} ≥ 0.95` must hold.
//! 3. Check the upstream SQLite release notes for changes to the
//!    `unicode61` and `trigram` tokenisers between the pinned version
//!    and the new version (https://www.sqlite.org/changes.html).
//! 4. If all of the above pass and you've reviewed the FTS5
//!    tokeniser diff, update the literal versions here AND add a
//!    CHANGELOG entry under "Changed — Dependencies" documenting the
//!    bundled-SQLite version transition.
//!
//! Tracking literal version strings in tests is brittle on purpose:
//! the brittleness is the feature, not a bug.

use rusqlite::Connection;

/// The exact bundled SQLite version we ship with rusqlite
/// `0.36.0` / libsqlite3-sys `0.34.0` / SQLCipher's vendored
/// fork (SQLCipher 4.6.1). Update only on a deliberate, audited
/// SQLite bundle bump per the module-level docs above.
const EXPECTED_SQLITE_VERSION: &str = "3.46.1";

/// SQLite "source ID" — the upstream build fingerprint, more
/// specific than the dotted version (it includes the build
/// timestamp and a tree-hash prefix). A change here without a
/// corresponding `EXPECTED_SQLITE_VERSION` bump indicates the
/// vendor (SQLCipher) rebuilt the same dotted version against a
/// patched tree — also worth a maintainer ack.
///
/// SQLCipher 4.6.1 vendored fork of upstream SQLite 3.46.1.
const EXPECTED_SQLITE_SOURCE_PREFIX: &str = "2024-08-13 09:16:08";

#[test]
fn bundled_sqlite_version_matches_pin() {
    let conn = Connection::open_in_memory().expect("open in-memory db");
    let actual: String = conn
        .query_row("SELECT sqlite_version()", [], |row| row.get(0))
        .expect("SELECT sqlite_version()");
    assert_eq!(
        actual, EXPECTED_SQLITE_VERSION,
        "Bundled SQLite version moved from the pinned {EXPECTED_SQLITE_VERSION} \
         to {actual}. This usually means a rusqlite or libsqlite3-sys bump \
         changed which SQLite the substrate ships with. Before updating the \
         literal in `bundled_sqlite_canary.rs`, follow the steps in the \
         module-level docs of this file: re-run all `fts5_*` tests, re-run \
         the  cross-lingual recall benchmark, and audit the \
         upstream SQLite release notes for unicode61 / trigram tokeniser \
         changes between {EXPECTED_SQLITE_VERSION} and {actual}."
    );
}

#[test]
fn bundled_sqlite_source_id_matches_pin() {
    let conn = Connection::open_in_memory().expect("open in-memory db");
    let actual: String = conn
        .query_row("SELECT sqlite_source_id()", [], |row| row.get(0))
        .expect("SELECT sqlite_source_id()");
    // sqlite_source_id() format is e.g.
    // "2024-08-13 09:16:08 c9c2ab54ba1f5f46360f1b4f35d849cd3f080e6fc2b6c60e91b16c63f69aalt1"
    // — a timestamp followed by a 64-char tree hash. We pin only the
    // timestamp prefix because SQLCipher's tree hash differs from the
    // upstream SQLite hash for the same dotted version (the "alt1"
    // suffix is SQLCipher's marker), so a tree-hash-anchored check
    // would be too aggressive.
    assert!(
        actual.starts_with(EXPECTED_SQLITE_SOURCE_PREFIX),
        "Bundled SQLite source ID prefix moved from \
         '{EXPECTED_SQLITE_SOURCE_PREFIX}' to '{actual}'. See the \
         module-level docs in `bundled_sqlite_canary.rs` for the audit \
         procedure before updating this constant."
    );
}

/// Pin the `unicode61` tokeniser as the substrate sees it. The
/// multilingual lexical lane delegates Latin / Greek / Cyrillic /
/// Devanagari / Arabic / Hebrew / etc. to this tokeniser via
/// `tokenize = 'unicode61 remove_diacritics 2'` (see
/// `crates/evidence_store/src/schema.rs:314` and `:356`), so any
/// change to its token boundaries or case-folding rules would
/// propagate directly to recall@k.
///
/// `remove_diacritics 2` is the more aggressive level that strips
/// diacritics across the entire combining-sequence space (level 1
/// only strips a fixed legacy set inherited from SQLite 3.x's
/// original implementation); the substrate uses level 2 so
/// queries against bodies with combining marks (Vietnamese tone
/// marks, Arabic harakat, Hebrew niqqud, etc.) still round-trip
/// when the query is unmarked.
#[test]
fn unicode61_tokeniser_emits_expected_tokens_for_canary_corpus() {
    let conn = Connection::open_in_memory().expect("open in-memory db");
    // Set up an FTS5 vtable with the exact tokeniser config we use
    // in production. The `tokenize` clause here mirrors what the
    // substrate's schema emits at `schema.rs:314` and `schema.rs:356`.
    conn.execute_batch(
        "CREATE VIRTUAL TABLE canary USING fts5(body,
            tokenize = 'unicode61 remove_diacritics 2'
        );",
    )
    .expect("create unicode61 fts5 vtable");

    // A small canary corpus exercising each behaviour the
    // substrate's multilingual lane relies on:
    //   * Latin diacritic-stripping: 'café' → 'cafe'
    //   * Latin case-folding: 'CAFE' → 'cafe'
    //   * Mixed-script splitting on punctuation: 'rust;sqlite'
    //   * Devanagari tokenisation: 'नमस्ते'
    //   * Cyrillic tokenisation: 'Привет'
    //   * Hebrew tokenisation: 'שלום'
    let corpus = [
        (1i64, "café CAFE Café"),
        (2, "rust;sqlite rust,sqlite rust.sqlite"),
        (3, "नमस्ते दुनिया"),
        (4, "Привет мир"),
        (5, "שלום עולם"),
    ];
    for (rowid, text) in corpus {
        conn.execute(
            "INSERT INTO canary(rowid, body) VALUES (?1, ?2)",
            (rowid, text),
        )
        .expect("insert canary row");
    }

    // Each MATCH query is a recall invariant. If unicode61's
    // tokeniser-level behaviour changes (case-folding, diacritic
    // stripping, ASCII-vs-Unicode word boundaries, or script
    // segmentation), one of these queries will start missing.
    let queries: &[(&str, &[i64])] = &[
        // Latin diacritic + case round-trips (rows 1).
        ("cafe", &[1]),
        ("CAFE", &[1]),
        ("café", &[1]),
        // Latin punctuation segmentation (row 2).
        ("rust", &[2]),
        ("sqlite", &[2]),
        // Devanagari (row 3).
        ("नमस्ते", &[3]),
        // Cyrillic case-folding (row 4).
        ("привет", &[4]),
        ("ПРИВЕТ", &[4]),
        // Hebrew (row 5). Hebrew has no case, but unicode61 must
        // still tokenise it into a searchable token.
        ("שלום", &[5]),
    ];
    for (query, expected) in queries {
        let mut stmt = conn
            .prepare("SELECT rowid FROM canary WHERE body MATCH ?1 ORDER BY rowid")
            .expect("prepare match");
        let actual: Vec<i64> = stmt
            .query_map([query], |row| row.get::<_, i64>(0))
            .expect("query_map")
            .map(|r| r.expect("row"))
            .collect();
        assert_eq!(
            &actual, *expected,
            "unicode61 tokeniser behaviour drift: query {query:?} matched \
             rows {actual:?} but the canary expected {expected:?}. If a \
             rusqlite/libsqlite3-sys bump caused this, see the audit \
             procedure in the module-level docs of `bundled_sqlite_canary.rs`."
        );
    }
}

/// Pin the `trigram` tokeniser. The multilingual lexical lane uses
/// trigram-tokenised tables for the second recall lane on scripts
/// where unicode61 over-segments (CJK / Thai / Tibetan / Khmer /
/// Myanmar / Lao). Trigram-tokeniser
/// behaviour is more sensitive to SQLite point releases than
/// unicode61's because the trigram tokeniser has shipped multiple
/// bugfixes in the 3.4x line.
///
/// Note: the trigram tokeniser intentionally takes **no**
/// arguments — `remove_diacritics` is a `unicode61`-only option
/// and the production schema (`schema.rs:329`) reflects this.
/// SQLCipher 4.6.1 / SQLite 3.46.1 silently accepts unknown
/// options on `trigram` (we verified this empirically; specifying
/// `remove_diacritics N` does not error but is a no-op), so this
/// canary mirrors the production schema exactly to make the
/// no-op-vs-supported distinction unambiguous.
#[test]
fn trigram_tokeniser_recalls_substring_for_canary_corpus() {
    let conn = Connection::open_in_memory().expect("open in-memory db");
    conn.execute_batch(
        // Match production exactly: `tokenize = 'trigram'`, no
        // options. See `crates/evidence_store/src/schema.rs:329`.
        "CREATE VIRTUAL TABLE canary_tri USING fts5(body,
            tokenize = 'trigram'
        );",
    )
    .expect("create trigram fts5 vtable");
    conn.execute(
        "INSERT INTO canary_tri(rowid, body) VALUES (?1, ?2)",
        (1i64, "the quick brown fox jumps over the lazy dog"),
    )
    .expect("insert canary trigram row");
    conn.execute(
        "INSERT INTO canary_tri(rowid, body) VALUES (?1, ?2)",
        (2i64, "ការសិក្សា"), // Khmer "education"
    )
    .expect("insert canary trigram row");

    let queries: &[(&str, &[i64])] = &[
        // Substring recall on Latin (a trigram tokeniser's whole
        // point) — "quick brown" is a 3+ character substring.
        ("quick brown", &[1]),
        ("rown", &[1]),
        // Khmer substring recall.
        ("សិក្ស", &[2]),
    ];
    for (query, expected) in queries {
        let mut stmt = conn
            .prepare("SELECT rowid FROM canary_tri WHERE body MATCH ?1 ORDER BY rowid")
            .expect("prepare match");
        let actual: Vec<i64> = stmt
            .query_map([query], |row| row.get::<_, i64>(0))
            .expect("query_map")
            .map(|r| r.expect("row"))
            .collect();
        assert_eq!(
            &actual, *expected,
            "trigram tokeniser behaviour drift: query {query:?} matched \
             rows {actual:?} but the canary expected {expected:?}. If a \
             rusqlite/libsqlite3-sys bump caused this, see the audit \
             procedure in the module-level docs of `bundled_sqlite_canary.rs`."
        );
    }
}

/// Pin SQLite's behaviour around the trigram tokeniser's
/// option-parsing surface. The doc comment on
/// `trigram_tokeniser_recalls_substring_for_canary_corpus` claims
/// SQLCipher 4.6.1 / SQLite 3.46.1 silently accepts unknown
/// options on `trigram` (treating them as no-ops rather than
/// errors). This test pins that behaviour so a future SQLite
/// version that tightens the parser to error on unknown options
/// surfaces immediately — important because if it ever does, any
/// FTS5 schema in the substrate that mistakenly passes options to
/// a non-unicode61 tokeniser would start failing schema creation
/// at startup, and we'd rather catch that here than at runtime in
/// the field.
///
/// If this assertion fires, the right response is *not* to change
/// what we do in the substrate (we already match production
/// schema), but to update the doc on
/// `trigram_tokeniser_recalls_substring_for_canary_corpus` so
/// future maintainers don't pattern-match the empirical claim.
#[test]
fn trigram_tokeniser_silently_accepts_unknown_options() {
    let conn = Connection::open_in_memory().expect("open in-memory db");
    // The `remove_diacritics` option is unicode61-specific; the
    // trigram tokeniser doesn't recognise it. We expect SQLCipher
    // 4.6.1 to accept the unknown option as a no-op rather than
    // erroring out at CREATE time.
    let result = conn.execute_batch(
        "CREATE VIRTUAL TABLE canary_tri_unknown USING fts5(body,
            tokenize = 'trigram remove_diacritics 1'
        );",
    );
    assert!(
        result.is_ok(),
        "Expected SQLite to silently accept the unknown \
         `remove_diacritics` option on the trigram tokeniser \
         (it's a unicode61-only option), but CREATE failed with: \
         {result:?}. If this assertion fires, SQLite has tightened \
         its FTS5 option parser. Audit any FTS5 schema in the \
         substrate that passes options to non-unicode61 tokenisers, \
         and update the doc on \
         `trigram_tokeniser_recalls_substring_for_canary_corpus`."
    );
}
