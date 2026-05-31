//! Script-detection helpers used by the FTS5 write / read routing
//! introduced in schema v14 (Phase 1.2 — CJK-aware FTS5 tokeniser).
//!
//! SQLite FTS5's `unicode61` tokeniser classifies CJK Han, Hiragana,
//! Katakana, and Thai codepoints as non-letter *separators*, so a
//! document composed entirely of those scripts produces zero tokens
//! and is invisible to lexical search. Phase 1.2 adds a parallel
//! `evidence_fts_cjk` virtual table tokenised with `trigram`, and
//! routes each ingest into the CJK table *additionally* iff the body
//! contains any codepoint from one of the affected scripts.
//!
//! The routing decision is deliberately based on the **body** rather
//! than on the row's stored `language_tag` (Phase 1.3): the language
//! tag can be `NULL` (detection refused) or wrong (mixed-language
//! body whose dominant language is Latin), and in both cases the
//! tokeniser-blind text is still in the body. A pure-codepoint
//! membership check is robust to those misses and adds only a single
//! linear pass over the bytes — much cheaper than re-running language
//! detection at the storage layer.
//!
//! Korean (Hangul, `U+AC00..U+D7AF`), Vietnamese (Latin with
//! diacritics), Hindi (Devanagari, whitespace-separated words) and
//! Arabic (whitespace-separated words) are intentionally **not**
//! included: `unicode61` segments those scripts correctly because
//! they either use whitespace word boundaries or fall inside its
//! letter category. Adding them to the CJK routing would write
//! redundant rows into `evidence_fts_cjk` with no recall benefit.

/// `true` iff `text` contains at least one codepoint that
/// SQLite FTS5's `unicode61` tokeniser cannot segment into a useful
/// token, and which is therefore eligible to be additionally
/// indexed in the `evidence_fts_cjk` (trigram) table.
///
/// Returns `false` for the empty string, for any pure-Latin /
/// Cyrillic / Greek / Arabic / Devanagari / Hangul body, and for
/// bodies that contain *only* whitespace, digits, or punctuation
/// from any of those scripts.
///
/// The check is O(n) over the UTF-8 codepoints and short-circuits on
/// the first match. Only the body itself is scanned — no normalisation
/// or NFC composition is required for routing purposes because the
/// script-membership decision is invariant under both.
pub fn contains_cjk_or_thai(text: &str) -> bool {
    text.chars().any(is_cjk_or_thai_codepoint)
}

/// Codepoint-level predicate paired with [`contains_cjk_or_thai`].
///
/// Returns `true` for codepoints in:
///
/// * Hiragana (`U+3040..=U+309F`) — Japanese kana
/// * Katakana (`U+30A0..=U+30FF`) — Japanese kana
/// * CJK Unified Ideographs (`U+4E00..=U+9FFF`) — modern Han
/// * CJK Unified Ideographs Extension A (`U+3400..=U+4DBF`) —
///   historical / rare Han, present in Japanese name registries and
///   classical Chinese corpora
/// * CJK Unified Ideographs Extension B (`U+20000..=U+2A6DF`) —
///   supplementary Han plane used in scholarly / historical text
/// * Thai (`U+0E00..=U+0E7F`)
///
/// The four most common CJK ranges plus Thai cover \> 99% of the
/// real-world content the substrate ingests via KChat / Tessera /
/// connector pipelines. Less-common scripts that *also* lack
/// whitespace word boundaries (Tibetan `U+0F00..=U+0FFF`, Khmer
/// `U+1780..=U+17FF`, Myanmar `U+1000..=U+109F`, Lao
/// `U+0E80..=U+0EFF`) are intentionally excluded from Phase 1.2 to
/// keep the predicate's scope tightly bound to what Phase 1.4's
/// language detector actually produces (`ja`, `zh`, `th` and the
/// auxiliary CJK languages); a future phase can extend the
/// predicate alongside lexicon support for those scripts.
#[inline]
pub fn is_cjk_or_thai_codepoint(c: char) -> bool {
    matches!(
        c,
        '\u{3040}'..='\u{309F}'   // Hiragana
        | '\u{30A0}'..='\u{30FF}' // Katakana
        | '\u{3400}'..='\u{4DBF}' // CJK Unified Ideographs Extension A
        | '\u{4E00}'..='\u{9FFF}' // CJK Unified Ideographs
        | '\u{20000}'..='\u{2A6DF}' // CJK Unified Ideographs Extension B
        | '\u{0E00}'..='\u{0E7F}' // Thai
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_does_not_route_to_cjk() {
        assert!(!contains_cjk_or_thai(""));
    }

    #[test]
    fn pure_latin_does_not_route_to_cjk() {
        for s in [
            "Today is sunny",
            "Hello, world!",
            "Café",
            "naïve façade",
            "Подъезд",          // Russian
            "Καλημέρα",         // Greek
            "السلام عليكم",      // Arabic
            "नमस्ते",             // Devanagari (Hindi)
            "안녕하세요",       // Korean Hangul — uses whitespace word boundaries
            "Triển khai dự án", // Vietnamese Latin
        ] {
            assert!(
                !contains_cjk_or_thai(s),
                "{s:?} unexpectedly classified as CJK/Thai"
            );
        }
    }

    #[test]
    fn japanese_hiragana_routes_to_cjk() {
        assert!(contains_cjk_or_thai("こんにちは"));
    }

    #[test]
    fn japanese_katakana_routes_to_cjk() {
        assert!(contains_cjk_or_thai("コーヒー"));
    }

    #[test]
    fn japanese_kanji_routes_to_cjk() {
        assert!(contains_cjk_or_thai("今日は良い天気です"));
    }

    #[test]
    fn chinese_simplified_routes_to_cjk() {
        assert!(contains_cjk_or_thai("今天天气很好"));
    }

    #[test]
    fn chinese_traditional_routes_to_cjk() {
        assert!(contains_cjk_or_thai("今天天氣很好"));
    }

    #[test]
    fn thai_routes_to_cjk() {
        assert!(contains_cjk_or_thai("อากาศวันนี้ดี"));
    }

    #[test]
    fn cjk_extension_a_routes_to_cjk() {
        // U+3400 is the first Extension A codepoint.
        assert!(contains_cjk_or_thai("\u{3400}"));
        // U+4DBF is the last Extension A codepoint.
        assert!(contains_cjk_or_thai("\u{4DBF}"));
    }

    #[test]
    fn cjk_extension_b_routes_to_cjk() {
        // Supplementary plane (4-byte UTF-8).
        assert!(contains_cjk_or_thai("\u{20000}"));
        assert!(contains_cjk_or_thai("\u{2A6DF}"));
    }

    #[test]
    fn mixed_latin_with_one_cjk_codepoint_routes_to_cjk() {
        assert!(contains_cjk_or_thai("Project 計画 review"));
    }

    #[test]
    fn boundary_codepoints_just_outside_cjk_ranges_do_not_route() {
        // U+30FF is the last Katakana, U+3100 is outside both kana
        // and Han.
        assert!(contains_cjk_or_thai("\u{30FF}"));
        assert!(!contains_cjk_or_thai("\u{3100}"));

        // U+3040 is the first Hiragana, U+303F is outside.
        assert!(contains_cjk_or_thai("\u{3040}"));
        assert!(!contains_cjk_or_thai("\u{303F}"));

        // U+0E00 is the first Thai, U+0DFF is outside.
        assert!(contains_cjk_or_thai("\u{0E00}"));
        assert!(!contains_cjk_or_thai("\u{0DFF}"));

        // U+4E00 is the first CJK Unified Ideograph, U+4DBF is the
        // last Extension A (both should route); U+4DC0..U+4DFF is
        // the Yijing Hexagram range — not a tokeniser problem in
        // practice but currently *not* routed.
        assert!(contains_cjk_or_thai("\u{4E00}"));
        assert!(contains_cjk_or_thai("\u{4DBF}"));
        assert!(!contains_cjk_or_thai("\u{4DC0}"));
    }
}
