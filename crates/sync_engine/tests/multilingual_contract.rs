//!  multilingual contract tests for [`sync_engine`].
//!
//! The CRDT machinery in this crate (`AddWinsSet`, `OpLog`,
//! `SyncEngine`, the [`delta`](sync_engine::delta) wire envelope,
//! and the [`persist::PersistentSyncEngine`](sync_engine::persist)
//! SQLCipher layer) is parameterised over an arbitrary
//! `T: Eq + Hash + Clone`. Element identity is therefore whatever
//! `Hash`/`Eq` derive on `T`, and the CRDT never inspects the
//! contents of `T` beyond hash and equality. The "Multilingual
//! contract" section of the crate-level docs
//! (`crates/sync_engine/src/lib.rs`) spells out the consequences
//! for text-bearing `T` — this file pins those consequences as
//! executable, regression-detecting invariants.
//!
//! ## What this suite proves
//!
//! 1. **Wire / persistence round-trip is byte-clean across five
//!    representative Unicode scripts** — ASCII, Japanese
//!    (Hiragana + Katakana + CJK ideographs), Arabic (RTL with
//!    combining marks), Hebrew (RTL with niqqud), and Devanagari
//!    (Indic with combining marks + virama). Every test feeds
//!    distinct script payloads through one of four end-to-end
//!    paths (add/merge, delta encode/decode, snapshot/restore,
//!    SQLCipher persist/reload) and asserts the receiver's
//!    materialised state contains the **exact same UTF-8 bytes**
//!    the authoring replica fed in.
//!
//! 2. **NFC and NFD inputs map to distinct CRDT identities.**
//!    The byte-exact-identity contract is what guarantees the
//!    add-wins property holds across replicas — but it also
//!    means a caller that normalises inconsistently across
//!    replicas will silently split a logical element into two
//!    physical ones. This test makes that explicit so the
//!    contract is visible in code: any future "smart"
//!    normalisation helper that quietly folds NFC ↔ NFD on
//!    insert would fail this test.
//!
//! 3. **Bidi-control and zero-width marks are preserved verbatim
//!    through merge.** `U+202E RIGHT-TO-LEFT OVERRIDE` /
//!    `U+200B ZERO WIDTH SPACE` participate in identity. Two
//!    strings that render identically but carry different
//!    invisible control marks remain distinct CRDT elements
//!    after a cross-replica merge.
//!
//! 4. **Compatibility decomposition (NFKC vs NFC) is also
//!    distinct.** Full-width `"Ａ"` (`U+FF21`) is *not* the same
//!    element as ASCII `"A"` — folding them is a caller-level
//!    decision (NFKC normalisation), not a CRDT-level one.

use std::collections::HashSet;

use sync_engine::delta::{apply_delta, encode_delta_since};
use sync_engine::persist::PersistentSyncEngine;
use sync_engine::{SyncEngine, SyncScopeId};
use tempfile::tempdir;
use uuid::Uuid;

/// Deterministic test master key. Sync-engine's persistence
/// layer derives its SQLCipher page key and per-scope AEAD key
/// from this via HKDF, so the same seed reproduces the same
/// on-disk encryption across runs (sufficient for tests; the
/// master key never leaves the test process).
fn test_master_key() -> crypto::MasterKey {
    let mut k: crypto::MasterKey = [0u8; crypto::MASTER_KEY_LEN];
    for (i, slot) in k.iter_mut().enumerate() {
        // `i` is bounded by MASTER_KEY_LEN = 32 < 256, so masking
        // to a byte never truncates the meaningful bits.
        #[allow(clippy::cast_possible_truncation,
            reason = "deterministic test key seed; i < MASTER_KEY_LEN < 256"
        )]
        let byte = (i & 0xff) as u8;
        *slot = byte.wrapping_mul(13).wrapping_add(29);
    }
    k
}

/// Five representative script payloads used across every
/// multilingual contract test. Each is a (label, body) pair where
/// the body is a non-trivial sentence in that script — long
/// enough to include script-typical features (combining marks,
/// bidirectionality, ideograph density) rather than a single
/// code-point smoke test.
///
/// We deliberately use NFC-normalised forms throughout so the
/// "wire / persistence is byte-clean" tests can assert exact
/// byte preservation without depending on the caller's
/// normalisation discipline. The NFC-vs-NFD distinctness pin
/// uses its own dedicated payload below.
fn multilingual_payloads() -> Vec<(&'static str, String)> {
    vec![
        ("ascii",
            "the quick brown fox jumps over the lazy dog".to_string(),
        ),
        // Japanese: hiragana + katakana + CJK ideographs.
        ("ja",
            "東京の電車は時間通りに到着します。カタカナとひらがなを混ぜた文章。".to_string(),
        ),
        // Arabic: RTL, includes combining diacritics (fatha,
        // kasra, damma) — `يَكْتُبُ` is a single grapheme cluster
        // composed of multiple code points.
        ("ar", "اللُّغَةُ العَرَبِيَّةُ مِنْ أَكْثَرِ اللُّغَاتِ انْتِشَارًا".to_string()),
        // Hebrew: RTL, includes niqqud (vowel pointing marks) —
        // `שָׁלוֹם` carries a kamatz + shin-dot + holam.
        ("he", "שָׁלוֹם עוֹלָם, זוֹהִי בְּדִיקַת תַּמִיכָה רַב־לְשׁוֹנִית".to_string()),
        // Devanagari: virama-joined consonant clusters and
        // combining vowel signs — `नमस्ते` is `न + म + स + ् + त + े`.
        ("hi", "नमस्ते दुनिया, यह बहु-भाषी समर्थन का परीक्षण है".to_string()),
    ]
}

#[test]
fn add_merge_round_trip_preserves_multilingual_bytes() {
    // Sender ingests all five script payloads; receiver merges
    // the sender's op log and must materialise the exact same
    // five UTF-8 byte sequences.
    let mut sender: SyncEngine<String> = SyncEngine::new();
    let payloads = multilingual_payloads();
    for (_label, body) in &payloads {
        sender.add(body.clone());
    }

    let mut receiver: SyncEngine<String> = SyncEngine::new();
    receiver.merge(&sender);

    let (state, _supers) = receiver.state().unwrap();
    let observed: HashSet<&String> = state.elements().collect();
    for (label, body) in &payloads {
        assert!(observed.contains(&body),
            "receiver did not observe `{label}` payload after merge",
        );
    }
    // Defensive: every payload is byte-distinct so the receiver
    // must surface exactly five elements (no accidental folding).
    assert_eq!(observed.len(),
        payloads.len(),
        "receiver's element count diverged from sender's: {observed:?}",
    );
}

#[test]
fn delta_encode_decode_preserves_multilingual_bytes() {
    // Sender authors multilingual ops; receiver applies the
    // wire-format delta and must reproduce the exact same UTF-8
    // bodies. Exercises serde_json + the DeltaEnvelope encoding
    // end-to-end across every script.
    let mut sender: SyncEngine<String> = SyncEngine::new();
    let payloads = multilingual_payloads();
    for (_label, body) in &payloads {
        sender.add(body.clone());
    }
    let delta_bytes = encode_delta_since(sender.op_log(), 0).unwrap();

    let mut receiver: SyncEngine<String> = SyncEngine::new();
    let absorbed = apply_delta(&mut receiver, &delta_bytes).unwrap();
    assert_eq!(absorbed, payloads.len());

    let (state, _supers) = receiver.state().unwrap();
    for (label, body) in &payloads {
        assert!(state.contains(body),
            "receiver missing `{label}` payload after delta apply",
        );
    }
}

#[test]
fn snapshot_restore_preserves_multilingual_bytes() {
    // SyncEngine::snapshot serialises the op log + materialised
    // set via serde_json. Restoring must yield bit-identical
    // state — including the exact UTF-8 bytes of every payload.
    let mut author: SyncEngine<String> = SyncEngine::new();
    let payloads = multilingual_payloads();
    for (_label, body) in &payloads {
        author.add(body.clone());
    }
    let snap = author.snapshot().unwrap();

    let restored: SyncEngine<String> = SyncEngine::restore_snapshot(&snap).unwrap();
    let (state, _supers) = restored.state().unwrap();
    for (label, body) in &payloads {
        assert!(state.contains(body),
            "restored engine missing `{label}` payload",
        );
    }
}

#[test]
fn sqlcipher_persist_reload_preserves_multilingual_bytes() {
    // PersistentSyncEngine AEAD-seals serde_json-encoded SyncOps
    // into a SQLCipher blob column. The full path is:
    //
    //   add(String) → engine log → serde_json bytes → AEAD seal
    //     → SQLCipher blob → (close) → AEAD open → serde_json
    //     decode → engine log → materialised set
    //
    // and every step must be byte-clean for the receiver to
    // observe the original UTF-8 payloads.
    let dir = tempdir().unwrap();
    let path = dir.path().join("multilingual_sync.sqlite");
    let scope = SyncScopeId::new_v4();
    let replica = Uuid::new_v4();
    let mk = test_master_key();
    let payloads = multilingual_payloads();

    {
        let mut p = PersistentSyncEngine::<String>::open(&path, scope, replica, &mk).unwrap();
        for (_label, body) in &payloads {
            p.add(body.clone()).unwrap();
        }
    }

    // Re-open the same on-disk database with the same master key
    // and verify the AEAD-sealed payloads decrypt to the exact
    // same UTF-8 bytes.
    let p2 = PersistentSyncEngine::<String>::open(&path, scope, replica, &mk).unwrap();
    let (state, _supers) = p2.engine().state().unwrap();
    for (label, body) in &payloads {
        assert!(state.contains(body),
            "persisted DB missing `{label}` payload after reload",
        );
    }
}

#[test]
fn two_replica_merge_with_multilingual_payloads_is_commutative() {
    // Replica A authors two payloads; replica B authors three
    // different payloads (different scripts). Both merge each
    // other's log via merge_logs / merge() and must converge on
    // the same set of five elements, byte-for-byte identical.
    // This is the multilingual-payload analogue of the
    // `merge_is_commutative` unit test in
    // `crates/sync_engine/src/crdt.rs`, with the same
    // direction-of-merge invariant (A ∪ B == B ∪ A) — naming
    // matches the crdt.rs convention.
    let payloads = multilingual_payloads();
    let (a_payloads, b_payloads) = payloads.split_at(2);

    // Build a' = A merged with B, b' = B merged with A using
    // fresh engines rebuilt from the source op logs. SyncEngine
    // does not implement Clone (RefCell-backed materialised-state
    // cache makes a meaningful Clone non-trivial), so we round-
    // trip via the underlying OpLog (which IS Clone) — equivalent
    // to bootstrapping a fresh peer with the same op stream.
    let mut a: SyncEngine<String> = SyncEngine::new();
    for (_label, body) in a_payloads {
        a.add(body.clone());
    }
    let mut b: SyncEngine<String> = SyncEngine::new();
    for (_label, body) in b_payloads {
        b.add(body.clone());
    }

    let mut a_merged: SyncEngine<String> = SyncEngine::from_log(a.replica_id(), a.op_log().clone());
    a_merged.merge(&b);
    let mut b_merged: SyncEngine<String> = SyncEngine::from_log(b.replica_id(), b.op_log().clone());
    b_merged.merge(&a);

    let (a_state, _) = a_merged.state().unwrap();
    let (b_state, _) = b_merged.state().unwrap();
    let a_set: HashSet<&String> = a_state.elements().collect();
    let b_set: HashSet<&String> = b_state.elements().collect();

    // Convergence: both replicas land on the same set of
    // byte-exact UTF-8 elements regardless of merge order.
    assert_eq!(a_set, b_set,
        "two-replica merge did not converge for multilingual payloads",
    );
    for (label, body) in &payloads {
        assert!(a_set.contains(&body),
            "merged state missing `{label}` payload",
        );
    }
}

#[test]
fn nfc_and_nfd_inputs_are_distinct_crdt_identities() {
    // Contract pin: identity is byte-exact, NOT
    // Unicode-normalisation-equivalent.
    //
    // `"café"` can be encoded two ways:
    //
    //   * NFC — `caf\u{00e9}` (4 code points, 5 bytes)
    //   * NFD — `cafe\u{0301}` (5 code points, 6 bytes)
    //
    // Both render identically. The CRDT MUST treat them as
    // distinct elements: a caller that normalises inconsistently
    // across replicas would otherwise silently break the
    // add-wins property on cross-replica merge.
    let nfc = "caf\u{00e9}".to_string();
    let nfd = "cafe\u{0301}".to_string();
    assert_ne!(nfc, nfd, "test inputs must differ in bytes");
    assert_eq!(nfc.chars().count(), 4);
    assert_eq!(nfd.chars().count(), 5);

    let mut replica_a: SyncEngine<String> = SyncEngine::new();
    replica_a.add(nfc.clone());
    let mut replica_b: SyncEngine<String> = SyncEngine::new();
    replica_b.add(nfd.clone());

    // Merge B into A in place — A's state then reflects both
    // replicas' contributions.
    let mut merged: SyncEngine<String> =
        SyncEngine::from_log(replica_a.replica_id(), replica_a.op_log().clone());
    merged.merge(&replica_b);

    let (state, _supers) = merged.state().unwrap();
    assert!(state.contains(&nfc),
        "NFC element must remain present after merge",
    );
    assert!(state.contains(&nfd),
        "NFD element must remain present after merge",
    );

    // Two byte-distinct elements, not one — pinning the contract
    // so a future "smart" normaliser cannot quietly fold these.
    let live: HashSet<&String> = state.elements().collect();
    assert_eq!(live.len(),
        2,
        "NFC and NFD must be distinct CRDT identities; got {live:?}",
    );
}

#[test]
fn bidi_control_marks_are_preserved_through_merge() {
    // Contract pin: bidi-control and zero-width code points
    // participate in identity and survive merge byte-exact.
    //
    // The three strings below all render to a viewer as the same
    // word "hello" but carry different invisible marks:
    //
    //   1. plain "hello"
    //   2. "hel\u{202E}lo" — RIGHT-TO-LEFT OVERRIDE in the middle
    //   3. "hel\u{200B}lo" — ZERO WIDTH SPACE in the middle
    //
    // They must remain three distinct CRDT identities after a
    // cross-replica merge.
    let plain = "hello".to_string();
    let with_rlo = "hel\u{202E}lo".to_string();
    let with_zwsp = "hel\u{200B}lo".to_string();
    assert_ne!(plain, with_rlo);
    assert_ne!(plain, with_zwsp);
    assert_ne!(with_rlo, with_zwsp);

    let mut a: SyncEngine<String> = SyncEngine::new();
    a.add(plain.clone());
    a.add(with_rlo.clone());
    let mut b: SyncEngine<String> = SyncEngine::new();
    b.add(with_zwsp.clone());

    let mut merged: SyncEngine<String> = SyncEngine::from_log(a.replica_id(), a.op_log().clone());
    merged.merge(&b);

    let (state, _supers) = merged.state().unwrap();
    let live: HashSet<&String> = state.elements().collect();
    assert_eq!(live.len(),
        3,
        "three byte-distinct strings must remain distinct after merge; got {live:?}",
    );
    assert!(state.contains(&plain));
    assert!(state.contains(&with_rlo));
    assert!(state.contains(&with_zwsp));
}

#[test]
fn compatibility_decomposition_pairs_are_distinct_crdt_identities() {
    // Contract pin: NFKC compatibility-equivalence is NOT folded
    // by the CRDT. Full-width `"Ａ"` (`U+FF21`) and ASCII `"A"`
    // are distinct identities until a caller chooses NFKC.
    //
    // This is the "Roman-numeral / full-width / circled-letter"
    // class of Unicode confusables — important for the audit
    // because folding them would change the cardinality of a
    // merged set and could mask data loss in operator
    // dashboards.
    let full = "Ａ".to_string(); // U+FF21
    let half = "A".to_string(); // U+0041
    assert_ne!(full, half);

    let mut a: SyncEngine<String> = SyncEngine::new();
    a.add(full.clone());
    let mut b: SyncEngine<String> = SyncEngine::new();
    b.add(half.clone());

    let mut merged: SyncEngine<String> = SyncEngine::from_log(a.replica_id(), a.op_log().clone());
    merged.merge(&b);

    let (state, _supers) = merged.state().unwrap();
    let live: HashSet<&String> = state.elements().collect();
    assert_eq!(live.len(),
        2,
        "full-width and ASCII `A` must be distinct CRDT identities; got {live:?}",
    );
}

#[test]
fn multilingual_delta_round_trip_through_sqlcipher_is_byte_clean() {
    // Integration smoke test combining every layer:
    //   1. Author authors multilingual payloads via
    //      PersistentSyncEngine (engine + SQLCipher persist).
    //   2. We pull the *engine* op log out, encode a delta from
    //      it, and feed the delta into a fresh in-memory
    //      receiver.
    //   3. The receiver must materialise the same byte-exact
    //      UTF-8 payloads.
    //
    // This exercises the full pipeline `String -> engine log
    // -> JSON -> AEAD seal -> SQLite blob -> (reload) -> AEAD
    // open -> JSON decode -> engine log -> delta encode (JSON
    // over UTF-8) -> apply_delta -> engine log -> state()` and
    // pins it as byte-clean across all five scripts.
    let dir = tempdir().unwrap();
    let path = dir.path().join("multilingual_delta.sqlite");
    let scope = SyncScopeId::new_v4();
    let replica = Uuid::new_v4();
    let mk = test_master_key();
    let payloads = multilingual_payloads();

    {
        let mut p = PersistentSyncEngine::<String>::open(&path, scope, replica, &mk).unwrap();
        for (_label, body) in &payloads {
            p.add(body.clone()).unwrap();
        }
    }

    // Reload + extract a delta covering every persisted op.
    let p2 = PersistentSyncEngine::<String>::open(&path, scope, replica, &mk).unwrap();
    let delta_bytes = encode_delta_since(p2.engine().op_log(), 0).unwrap();

    let mut receiver: SyncEngine<String> = SyncEngine::new();
    let absorbed = apply_delta(&mut receiver, &delta_bytes).unwrap();
    assert_eq!(absorbed, payloads.len());

    let (state, _supers) = receiver.state().unwrap();
    for (label, body) in &payloads {
        assert!(state.contains(body),
            "receiver missing `{label}` payload after persist-then-delta round-trip",
        );
    }
}
