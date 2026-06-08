//! Fuzz target: CJK/Thai bigram FTS5 tokenizer.
//!
//! Feeds arbitrary UTF-8 (CJK, Thai, Arabic, combining marks, surrogate
//! edge cases via lossy decoding, …) through the bigram tokenizer used
//! for the `evidence_fts_bigram` lane and its query-side counterpart,
//! checking that:
//! 1. Neither `compute_cjk_bigrams` (index side) nor
//!    `compute_cjk_bigram_query` (query side) panics on any input.
//! 2. Both sides agree on how many bigrams a string produces: the index
//!    side emits `n-1` space-separated 2-codepoint tokens and the query
//!    side emits `n-1` AND-conjoined quoted terms, where `n` is the
//!    number of CJK/Thai codepoints. Fewer than two CJK/Thai codepoints
//!    yields an empty index string and a `None` query.
//! 3. `script::contains_cjk_or_thai` agrees with the tokenizer's own
//!    routing predicate (non-empty output iff at least two kept chars).

#![no_main]

use libfuzzer_sys::fuzz_target;

use evidence_store::bigram::{compute_cjk_bigram_query, compute_cjk_bigrams};
use evidence_store::script::{contains_cjk_or_thai, is_cjk_or_thai_codepoint};

fuzz_target!(|data: &[u8]| {
    // Arbitrary bytes -> valid UTF-8 (invalid sequences become U+FFFD,
    // which is itself a useful non-CJK edge case for the router).
    let text = String::from_utf8_lossy(data);

    let kept = text.chars().filter(|c| is_cjk_or_thai_codepoint(*c)).count();

    // Index side: must not panic.
    let index = compute_cjk_bigrams(&text);
    // Query side: must not panic.
    let query = compute_cjk_bigram_query(&text);

    if kept < 2 {
        assert!(
            index.is_empty(),
            "fewer than two CJK/Thai codepoints must yield no index bigrams"
        );
        assert!(
            query.is_none(),
            "fewer than two CJK/Thai codepoints must yield no query clause"
        );
        return;
    }

    let expected_bigrams = kept - 1;

    // Index side emits `expected_bigrams` space-separated tokens.
    let index_terms = index.split(' ').filter(|s| !s.is_empty()).count();
    assert_eq!(
        index_terms, expected_bigrams,
        "index bigram count must be (CJK/Thai codepoints - 1)"
    );

    // Query side emits `expected_bigrams` AND-conjoined quoted terms.
    let clause = query.expect(">=2 CJK/Thai codepoints must yield a query clause");
    let query_terms = clause.matches('"').count() / 2;
    assert_eq!(
        query_terms, expected_bigrams,
        "query term count must match index bigram count"
    );

    // The script router must agree there is CJK/Thai content.
    assert!(
        contains_cjk_or_thai(&text),
        "contains_cjk_or_thai must be true when bigrams were produced"
    );
});
