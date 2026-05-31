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
/// **Japanese kana**
/// * Hiragana (`U+3040..=U+309F`)
/// * Katakana (`U+30A0..=U+30FF`)
/// * Katakana Phonetic Extensions (`U+31F0..=U+31FF`) — small kana
///   used in Ainu transliteration and Japanese linguistics
/// * Halfwidth Katakana (`U+FF65..=U+FF9F`) — the half-width forms
///   commonly emitted by legacy Japanese IMEs, JIS X 0201–derived
///   sources, mobile-carrier SMS / SS7 gateways, and Japanese
///   telephony systems. Whole Japanese sentences are still written
///   in half-width katakana in production data (carrier billing
///   notifications, ATM receipts, older POS systems), so omitting
///   this block would silently strand those rows from CJK search.
///
/// **CJK Han ideographs**
/// * CJK Unified Ideographs (`U+4E00..=U+9FFF`) — modern Han
/// * CJK Unified Ideographs Extension A (`U+3400..=U+4DBF`) —
///   historical / rare Han, present in Japanese name registries and
///   classical Chinese corpora
/// * CJK Unified Ideographs Extension B (`U+20000..=U+2A6DF`) —
///   supplementary-plane Han used in scholarly / historical text
/// * CJK Unified Ideographs Extensions C..F **and I**
///   (`U+2A700..=U+2EE5F`, contiguous) — Extensions C..F
///   (`U+2A700..=U+2EBEF`) plus Extension I
///   (`U+2EBF0..=U+2EE5F`, added in Unicode 15.1, September 2023).
///   The ranges abut so we encode them as a single `matches!` arm.
/// * CJK Unified Ideographs Extensions G..H **and J**
///   (`U+30000..=U+33479`, contiguous) — Extensions G..H
///   (`U+30000..=U+323AF`) plus Extension J
///   (`U+323B0..=U+33479`, added in Unicode 16.0, September 2024).
///   The ranges abut so we encode them as a single `matches!` arm.
///   All of C..J are extremely rare scholarly characters; we route
///   them so the predicate is forward defensive against any future
///   content surfaced through OCR or academic corpora — the cost is
///   one extra range check per arm. The standing policy is **"every
///   currently-defined CJK Unified Ideographs Extension is routed"**
///   so a future contributor extending the predicate for Unicode
///   17+ Ext K / L / ... just needs to widen the upper bound of
///   whichever contiguous arm the new block belongs to, rather than
///   re-litigating the forward-defensive design every time.
/// * CJK Radicals Supplement (`U+2E80..=U+2EFF`) — Kangxi radical
///   components used in dictionaries and IME candidate lists
/// * CJK Compatibility Ideographs (`U+F900..=U+FAFF`) — duplicates
///   of Han characters preserved for round-trip compatibility with
///   legacy charsets (`KS X 1001`, `JIS X 0213`, `Big5`). Korean
///   text in particular still surfaces these glyphs in proper names
///   and pre-Unicode databases
///
/// **Thai**
/// * Thai (`U+0E00..=U+0E7F`) — Thai also lacks whitespace word
///   boundaries; `unicode61` produces zero tokens for it
///
/// Together these blocks cover the realistic ja / zh / ko-Hanja /
/// th content the substrate ingests via KChat / Tessera / connector
/// pipelines, including the awkward edge cases (half-width
/// katakana, compatibility ideographs) that are easy to miss with
/// a "BMP-only / no compatibility" check and that Devin Review
/// finding `ANALYSIS_…_0005` explicitly called out.
///
/// Korean Hangul (`U+AC00..=U+D7AF`), Vietnamese (Latin with
/// diacritics), Hindi (Devanagari, whitespace-separated words) and
/// Arabic (whitespace-separated words) are intentionally **not**
/// included: `unicode61` segments those scripts correctly because
/// they either use whitespace word boundaries or fall inside its
/// letter category. Adding them to the CJK routing would write
/// redundant rows into `evidence_fts_cjk` with no recall benefit.
///
/// Other scripts that *also* lack whitespace word boundaries
/// (Tibetan `U+0F00..=U+0FFF`, Khmer `U+1780..=U+17FF`, Myanmar
/// `U+1000..=U+109F`, Lao `U+0E80..=U+0EFF`) are intentionally
/// excluded from Phase 1.2 to keep the predicate's scope tightly
/// bound to what Phase 1.4's language detector actually produces
/// (`ja`, `zh`, `th` and the auxiliary CJK languages); a future
/// phase can extend the predicate alongside lexicon support for
/// those scripts.
#[inline]
pub fn is_cjk_or_thai_codepoint(c: char) -> bool {
    matches!(
        c,
        '\u{3040}'..='\u{309F}'      // Hiragana
        | '\u{30A0}'..='\u{30FF}'    // Katakana
        | '\u{31F0}'..='\u{31FF}'    // Katakana Phonetic Extensions
        | '\u{FF65}'..='\u{FF9F}'    // Halfwidth Katakana
        | '\u{2E80}'..='\u{2EFF}'    // CJK Radicals Supplement
        | '\u{3400}'..='\u{4DBF}'    // CJK Unified Ideographs Extension A
        | '\u{4E00}'..='\u{9FFF}'    // CJK Unified Ideographs
        | '\u{F900}'..='\u{FAFF}'    // CJK Compatibility Ideographs
        | '\u{20000}'..='\u{2A6DF}'  // CJK Unified Ideographs Extension B
        | '\u{2A700}'..='\u{2EE5F}'  // CJK Unified Ideographs Extensions C..F + I
        | '\u{30000}'..='\u{33479}'  // CJK Unified Ideographs Extensions G..H + J
        | '\u{0E00}'..='\u{0E7F}'    // Thai
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
    fn halfwidth_katakana_routes_to_cjk() {
        // "ホンヤク" written in halfwidth (U+FF80, U+FF9D, U+FF94,
        // U+FF78) — produced by legacy Japanese IMEs and JIS X
        // 0201-derived sources (telephony / SMS gateways, carrier
        // billing PDFs, older POS printers).
        assert!(contains_cjk_or_thai("\u{FF80}\u{FF9D}\u{FF94}\u{FF78}"));
        // Range boundaries: U+FF65 (halfwidth katakana middle dot)
        // is the first codepoint, U+FF9F is the last.
        assert!(contains_cjk_or_thai("\u{FF65}"));
        assert!(contains_cjk_or_thai("\u{FF9F}"));
        // Just-outside boundaries:
        //  - U+FF64 is the halfwidth ideographic comma (fullwidth
        //    forms block) — covered by neither the halfwidth
        //    katakana range nor any other CJK range we route, so
        //    it must NOT route on its own.
        //  - U+FFA0 is the start of Hangul halfwidth jamo, which
        //    is whitespace-segmented in unicode61 and intentionally
        //    excluded.
        assert!(!contains_cjk_or_thai("\u{FF64}"));
        assert!(!contains_cjk_or_thai("\u{FFA0}"));
    }

    #[test]
    fn cjk_compatibility_ideographs_route_to_cjk() {
        // Korean text routinely uses Compatibility Ideographs for
        // proper names that were encoded in pre-Unicode legacy
        // charsets — U+F900 ("豈") through U+FAFF cover this block.
        assert!(contains_cjk_or_thai("\u{F900}"));
        assert!(contains_cjk_or_thai("\u{FAFF}"));
        // Just-outside boundary: U+F8FF is the last codepoint of
        // the Private Use Area, which we deliberately do not route.
        assert!(!contains_cjk_or_thai("\u{F8FF}"));
        // And U+FB00 is the start of Alphabetic Presentation Forms
        // (Latin ligatures like ﬀ ﬁ ﬂ) — unicode61 handles those
        // correctly, so they must not route either.
        assert!(!contains_cjk_or_thai("\u{FB00}"));
    }

    #[test]
    fn katakana_phonetic_extensions_route_to_cjk() {
        // U+31F0..=U+31FF — small kana used in Ainu transliteration
        // and Japanese linguistics references.
        assert!(contains_cjk_or_thai("\u{31F0}"));
        assert!(contains_cjk_or_thai("\u{31FF}"));
    }

    #[test]
    fn cjk_radicals_supplement_routes_to_cjk() {
        // U+2E80..=U+2EFF — Kangxi radical components used in
        // dictionaries and IME candidate lists.
        assert!(contains_cjk_or_thai("\u{2E80}"));
        assert!(contains_cjk_or_thai("\u{2EFF}"));
    }

    #[test]
    fn cjk_extensions_c_through_j_route_to_cjk() {
        // Pick one codepoint from each merged range to confirm the
        // ranges are covered. The exact codepoints are scholarly /
        // historical Han characters that real corpora rarely
        // include, but if they ever do appear, the predicate must
        // route the row correctly rather than silently strand it.
        //
        // Sweep-6 INFO-0003: Extension I (U+2EBF0..=U+2EE5F, Unicode
        // 15.1, Sep 2023) and Extension J (U+323B0..=U+33479,
        // Unicode 16.0, Sep 2024) are now included to honour the
        // doc-comment's stated forward-defensive policy of routing
        // every currently-defined CJK Unified Ideographs Extension.
        assert!(contains_cjk_or_thai("\u{2A700}")); // first of Ext C
        assert!(contains_cjk_or_thai("\u{2B740}")); // first of Ext D
        assert!(contains_cjk_or_thai("\u{2B820}")); // first of Ext E
        assert!(contains_cjk_or_thai("\u{2CEB0}")); // first of Ext F
        assert!(contains_cjk_or_thai("\u{2EBEF}")); // last of contiguous C..F sub-span
        assert!(contains_cjk_or_thai("\u{2EBF0}")); // first of Ext I (Unicode 15.1)
        assert!(contains_cjk_or_thai("\u{2EE5F}")); // last of merged C..F+I range
        assert!(contains_cjk_or_thai("\u{30000}")); // first of Ext G
        assert!(contains_cjk_or_thai("\u{31350}")); // first of Ext H
        assert!(contains_cjk_or_thai("\u{323AF}")); // last of contiguous G..H sub-span
        assert!(contains_cjk_or_thai("\u{323B0}")); // first of Ext J (Unicode 16.0)
        assert!(contains_cjk_or_thai("\u{33479}")); // last of merged G..H+J range
        // Just past the merged C..F+I upper bound (U+2EE60..=U+2FFFF
        // is unallocated / non-CJK) must NOT route on its own.
        assert!(!contains_cjk_or_thai("\u{2EE60}"));
        assert!(!contains_cjk_or_thai("\u{2FFFF}"));
        // Just past the merged G..H+J upper bound (U+3347A..=U+3FFFF
        // is unallocated as of Unicode 16.0) must NOT route either.
        assert!(!contains_cjk_or_thai("\u{3347A}"));
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
