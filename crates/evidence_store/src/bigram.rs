//! CJK / Thai bigram precomputation used by the schema v15 FTS5
//! bigram lane (custom bigram tokeniser).
//!
//! The earlier multilingual rollout added the
//! [`crate::script::contains_cjk_or_thai`] routing predicate and a
//! trigram-tokenised companion table `evidence_fts_cjk`. SQLite
//! FTS5's built-in `trigram` tokeniser requires query terms to be
//! **at least three codepoints** — a 2-codepoint CJK query like
//! `天気` (Japanese "weather") returns `Ok(vec![])` because the
//! tokeniser produces no trigrams for the query side and rejects
//! the MATCH with an empty result set (or a swallowed error on
//! some SQLite builds). The trigram lane documented this as a
//! known limitation and noted "a future iteration can register a
//! Rust-side custom FTS5 bigram tokeniser via the `fts5_api` FFI
//! to close that gap".
//!
//! This module implements that gap-closing recall lane **without
//! reaching for `fts5_api`** — the idiomatic SQLite approach is
//! to pre-compute overlapping 2-codepoint windows over the
//! CJK / Thai portion of the body at write time and store the
//! result as whitespace-separated tokens in a parallel virtual
//! table that uses the same `unicode61 remove_diacritics 2`
//! tokeniser as `evidence_fts`. unicode61 *does* respect
//! whitespace word boundaries, so a string like
//! `"天気 気予 予報"` tokenises as the three independent tokens
//! `天気`, `気予`, `予報`. A 2-codepoint query `天気` then matches
//! every row whose bigram-precomputed body contains the token
//! `天気` — exactly the recall lane deferred.
//!
//! Why precomputed bigrams instead of a custom FTS5 tokeniser:
//!
//! * No `unsafe` and no FFI to SQLite's `fts5_api`. The whole
//!   codepath is pure-Rust and runs on every bundled SQLCipher
//!   build the substrate cares about.
//! * Same purge / `REBUILD` semantics as the trigram lane — the
//!   precomputed bigram string lives in the table's `content`
//!   column so `INSERT INTO ...(...) VALUES('rebuild')` can
//!   re-tokenise it from the stored content. This is critical
//!   for the cryptographic-forgetting guarantee (REBUILD must
//!   wipe residual plaintext tokens from a purged scope).
//! * Tokeniser swaps are FTS5 table options, not column changes,
//!   so the bigram lane is additive in the same shape used by
//!   the trigram lane — purge / rebuild / search all fan out
//!   across the new table without any restructure of the
//!   existing two.
//!
//! The bigram column is purely additive recall. The unicode61
//! `evidence_fts` table remains the source of truth for query
//! validity and `evidence_fts_cjk` (trigram) remains the
//! source of truth for 3+ codepoint CJK / Thai substring search;
//! the bigram lane only fills the 2-codepoint floor.
//!
//! The pre-tokenisation cost is O(n) over the body's codepoints
//! and the storage cost on a CJK body is ~3x the original text
//! (each kept codepoint emits a 2-char bigram + a space
//! separator), which is the same order of magnitude as the
//! trigram lane's storage overhead.

/// Compute the overlapping 2-codepoint windows over the
/// CJK / Thai portion of `text` and return them as a single
/// whitespace-separated string suitable for indexing under the
/// `unicode61 remove_diacritics 2` tokeniser.
///
/// Non-CJK codepoints in `text` are skipped *before* the
/// windowing so we never emit a boundary-crossing bigram (e.g.
/// "今日 Apple 天気" yields `"今日 日天 天気"` rather than a
/// bigram that pairs `日` with the space or with `A`). The
/// routing predicate is the same [`crate::script::is_cjk_or_thai_codepoint`]
/// used by the v14 trigram lane so the bigram and trigram
/// indexes agree on which codepoints contribute to recall.
///
/// Returns the empty string when fewer than two CJK / Thai
/// codepoints survive the filter — the bigram lane cannot
/// produce any recall in that case and writers MUST skip the
/// INSERT to avoid storing empty content rows that would still
/// allocate FTS5 docids and inflate the `evidence_fts_bigram_docsize`
/// shadow.
///
/// # Examples
///
/// ```ignore
/// use knowledge_evidence_store::bigram::compute_cjk_bigrams;
///
/// assert_eq!(compute_cjk_bigrams("天気予報"), "天気 気予 予報");
/// assert_eq!(compute_cjk_bigrams("Apple 天気 today"), "天気");
/// assert_eq!(compute_cjk_bigrams("Today is sunny"), "");
/// assert_eq!(compute_cjk_bigrams("天"), ""); // single codepoint
/// ```
pub fn compute_cjk_bigrams(text: &str) -> String {
    // Filter to CJK/Thai codepoints only. We MUST drop non-CJK
    // chars before windowing — keeping them would produce
    // boundary-crossing bigrams that would never appear in any
    // query (queries are bigram-tokenised by the same predicate).
    let kept: Vec<char> = text
        .chars()
        .filter(|c| crate::script::is_cjk_or_thai_codepoint(*c))
        .collect();
    if kept.len() < 2 {
        return String::new();
    }
    // Pre-size the output: each bigram is two codepoints
    // (≤ 4 bytes each, typically 3 in the CJK BMP) plus a
    // space separator. `kept.len() - 1` bigrams.
    let bigram_count = kept.len() - 1;
    let mut out = String::with_capacity(bigram_count * 8);
    for i in 0..bigram_count {
        if i > 0 {
            out.push(' ');
        }
        out.push(kept[i]);
        out.push(kept[i + 1]);
    }
    out
}

/// Build the FTS5 MATCH clause used by the bigram lane for the
/// given user-supplied `query`, or `None` if the lane cannot
/// produce any results for the query (fewer than two CJK / Thai
/// codepoints after the routing filter).
///
/// The strategy is to extract the CJK / Thai codepoints from the
/// query, compute their overlapping 2-codepoint windows, and
/// AND them together as quoted phrase terms — the same shape the
/// trigram lane uses for 3+ codepoint queries. For a 2-codepoint
/// query like `天気` the resulting clause is `"天気"`; for a
/// 4-codepoint query like `良い天気` it is
/// `"良い" AND "い天" AND "天気"`. Bigrams are emitted in source
/// order; the AND chain has the same semantics regardless of
/// term order so deterministic emission is purely for test
/// stability.
///
/// Non-CJK codepoints in the query are **ignored** — the bigram
/// lane is responsible only for the CJK / Thai portion of recall.
/// A query like `Apple 天気` returns `Some("\"天気\"")` so the
/// lane contributes any row containing the 天気 bigram; the Latin
/// `Apple` AND-conjunct is left to the unicode61 source-of-truth
/// branch, which already handles Latin tokens correctly. The
/// post-merge dedupe ([`crate::store::merged_fts_search`]'s
/// `MIN(rank)` HashMap) consolidates the two lanes by
/// `evidence_id` so a row hit by both branches appears once.
///
/// FTS5 MATCH terms are wrapped in double quotes so any embedded
/// punctuation in the bigram (extremely unlikely with the
/// CJK / Thai filter but defensive) cannot be reinterpreted as a
/// MATCH operator. A bigram cannot contain a literal `"` because
/// quotation marks are not in any routed Unicode block.
///
/// Returns `None` when the query has fewer than two CJK / Thai
/// codepoints — in that case the bigram lane has nothing to
/// contribute and the caller should skip the branch entirely
/// rather than prepare a no-op statement.
pub fn compute_cjk_bigram_query(query: &str) -> Option<String> {
    let kept: Vec<char> = query
        .chars()
        .filter(|c| crate::script::is_cjk_or_thai_codepoint(*c))
        .collect();
    if kept.len() < 2 {
        return None;
    }
    // Pre-size: each AND-conjoined term is `"AB" ` (≤ 1+8+1+5
    // bytes worst-case for two 4-byte codepoints, the space, the
    // two surrounding quotes, and ` AND `).
    let term_count = kept.len() - 1;
    let mut out = String::with_capacity(term_count * 16);
    for i in 0..term_count {
        if i > 0 {
            out.push_str(" AND ");
        }
        out.push('"');
        out.push(kept[i]);
        out.push(kept[i + 1]);
        out.push('"');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_yields_no_bigrams() {
        assert_eq!(compute_cjk_bigrams(""), "");
    }

    #[test]
    fn single_cjk_codepoint_yields_no_bigrams() {
        assert_eq!(compute_cjk_bigrams("天"), "");
        assert_eq!(compute_cjk_bigrams("今"), "");
    }

    #[test]
    fn two_cjk_codepoints_yield_single_bigram() {
        assert_eq!(compute_cjk_bigrams("天気"), "天気");
        assert_eq!(compute_cjk_bigrams("良い"), "良い");
    }

    #[test]
    fn four_cjk_codepoints_yield_three_overlapping_bigrams() {
        assert_eq!(compute_cjk_bigrams("天気予報"), "天気 気予 予報");
    }

    #[test]
    fn japanese_kanji_sentence_yields_full_bigram_window() {
        // 9-codepoint sentence (今 日 は 良 い 天 気 で す) ->
        // 8 overlapping bigrams.
        let bigrams = compute_cjk_bigrams("今日は良い天気です");
        assert_eq!(bigrams, "今日 日は は良 良い い天 天気 気で です");
        assert_eq!(bigrams.split_whitespace().count(), 8);
    }

    #[test]
    fn chinese_simplified_yields_full_bigram_window() {
        let bigrams = compute_cjk_bigrams("今天天气很好");
        assert_eq!(bigrams, "今天 天天 天气 气很 很好");
        assert_eq!(bigrams.split_whitespace().count(), 5);
    }

    #[test]
    fn pure_latin_yields_no_bigrams() {
        for s in [
            "Today is sunny",
            "Hello, world!",
            "Café",
            "naïve façade",
            "Подъезд",          // Cyrillic
            "Καλημέρα",         // Greek
            "السلام عليكم",      // Arabic
            "नमस्ते",             // Devanagari (Hindi)
            "안녕하세요",       // Korean Hangul — whitespace-segmented
            "Triển khai dự án", // Vietnamese Latin
        ] {
            assert_eq!(
                compute_cjk_bigrams(s),
                "",
                "{s:?} unexpectedly produced bigrams"
            );
        }
    }

    #[test]
    fn mixed_latin_and_cjk_drops_latin_before_windowing() {
        // The two CJK chars are adjacent in source so they form
        // one bigram. The Latin "Project" / "review" are dropped
        // entirely — no boundary-crossing bigram is emitted.
        assert_eq!(compute_cjk_bigrams("Project 計画 review"), "計画");
    }

    #[test]
    fn non_adjacent_cjk_chars_collapse_into_continuous_bigrams() {
        // Documents like "天 today 気" produce only the bigrams
        // that the kept-only stream produces, which is `天気` —
        // a recall improvement over today's invisibility but
        // does NOT promise the bigram appeared contiguously in
        // the source. This matches the trigram lane's "contains
        // every trigram" recall semantics: a precision /
        // recall trade-off the merge layer accepts as additive.
        assert_eq!(compute_cjk_bigrams("天 today 気"), "天気");
    }

    #[test]
    fn thai_yields_overlapping_bigrams() {
        // Thai is whitespace-less, like CJK. Five codepoints ->
        // four bigrams.
        let s = "อากาศ";
        let bigrams = compute_cjk_bigrams(s);
        assert_eq!(bigrams.split_whitespace().count(), 4);
        assert!(bigrams.contains("อา"));
        assert!(bigrams.contains("กา"));
    }

    #[test]
    fn halfwidth_katakana_routes_through_bigrams() {
        // ｱｲｳｴｵ — five halfwidth katakana codepoints -> 4 bigrams.
        let bigrams = compute_cjk_bigrams("ｱｲｳｴｵ");
        assert_eq!(bigrams.split_whitespace().count(), 4);
    }

    #[test]
    fn compute_cjk_bigram_query_none_for_pure_latin() {
        assert_eq!(compute_cjk_bigram_query(""), None);
        assert_eq!(compute_cjk_bigram_query("Today"), None);
        assert_eq!(compute_cjk_bigram_query("Apple"), None);
    }

    #[test]
    fn compute_cjk_bigram_query_none_for_single_cjk_codepoint() {
        assert_eq!(compute_cjk_bigram_query("天"), None);
    }

    #[test]
    fn compute_cjk_bigram_query_single_term_for_two_codepoints() {
        assert_eq!(
            compute_cjk_bigram_query("天気"),
            Some("\"天気\"".to_string())
        );
    }

    #[test]
    fn compute_cjk_bigram_query_and_chain_for_longer_queries() {
        assert_eq!(
            compute_cjk_bigram_query("天気予報"),
            Some("\"天気\" AND \"気予\" AND \"予報\"".to_string())
        );
    }

    #[test]
    fn compute_cjk_bigram_query_ignores_latin_chars_in_mixed_query() {
        assert_eq!(
            compute_cjk_bigram_query("Apple 天気"),
            Some("\"天気\"".to_string())
        );
        assert_eq!(
            compute_cjk_bigram_query("Project 計画書 review"),
            Some("\"計画\" AND \"画書\"".to_string())
        );
    }

    #[test]
    fn compute_cjk_bigrams_and_query_use_identical_routing_predicate() {
        // The two helpers MUST agree on which codepoints survive
        // the filter. If they ever diverge, writes would index a
        // codepoint that queries can never recall (or vice versa)
        // — a silent dead-letter recall gap.
        for s in [
            "今日は良い天気です",
            "天気予報",
            "今天天气很好",
            "อากาศวันนี้ดี",
            "Apple 天気",
            "Hello world",
            "計画書",
            "ｱｲｳ",
        ] {
            let written = compute_cjk_bigrams(s);
            match compute_cjk_bigram_query(s) {
                None => {
                    assert!(
                        written.is_empty(),
                        "writer produced {written:?} but reader would skip for {s:?}"
                    );
                }
                Some(_) => {
                    assert!(
                        !written.is_empty(),
                        "writer produced empty content but reader would query for {s:?}"
                    );
                }
            }
        }
    }
}
