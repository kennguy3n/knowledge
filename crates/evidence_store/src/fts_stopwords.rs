//! Phase 1.9 — per-script stopword inventory for the FTS5 trigram /
//! bigram recall lanes.
//!
//! # Architectural rationale
//!
//! The substrate's three FTS5 lanes (`evidence_fts` /
//! `evidence_fts_cjk` / `evidence_fts_bigram`) split labour by
//! tokeniser:
//!
//! * `evidence_fts` (unicode61) — whitespace-segmented Latin,
//!   Cyrillic, Greek, Arabic, Hebrew, Devanagari, Hangul. BM25's
//!   inverse-document-frequency naturally discounts common tokens,
//!   so an English query like `"the cat"` is dominated by the rare
//!   `cat` trigram even though `the` matches almost every document.
//!   **No stopword pre-filter is needed on the unicode61 lane** —
//!   idf already does the work.
//! * `evidence_fts_cjk` (trigram) and `evidence_fts_bigram`
//!   (precomputed-bigram) — sliding 3-codepoint / 2-codepoint
//!   windows over CJK / Thai / Tibetan / Khmer / Myanmar / Lao text.
//!   These tokenisers do **not** have an idf-style discount built in:
//!   every window is a unique token, so a window straddling a common
//!   particle (Japanese `の`, Chinese `的`, Thai `และ`) is
//!   indistinguishable from a window straddling a rare content
//!   word. A query like `日本のオリンピック` ("the Japanese
//!   Olympics") produces the trigram `日本の` — shared with every
//!   `日本の*` document — which dilutes precision against the
//!   discriminating `オリン` / `リンピ` content windows.
//!
//! # Symmetric stripping
//!
//! The fix is to strip stopword codepoints from **both** the indexed
//! body and the query expression before tokenisation. Symmetry is
//! mandatory for recall preservation:
//!
//! * **Index-only stripping** would mean the body
//!   `日本のオリンピック` is indexed as `日本オリンピック`
//!   (trigrams `日本オ`, `本オリ`, ...), but a query
//!   `日本のオリンピック` still tokenises to `日本の`, `本のオ`, ...
//!   — none of which match the stripped index. Recall regression on
//!   every query containing a stopword.
//! * **Query-only stripping** is the mirror failure: query
//!   `日本のオリンピック` stripped to `日本オリンピック`, but the
//!   index still contains the original `日本の` trigram. The
//!   stripped query asks for `日本オ` which is not in the index.
//!   Same recall regression in the opposite direction.
//! * **Symmetric stripping**: body and query are both stripped, so
//!   both produce the same `日本オ`, `本オリ`, ... trigrams. MATCH
//!   succeeds. The shared stopword trigrams (`日本の`, `本の*`,
//!   `*の*`) never enter the index in the first place — they cannot
//!   contribute to false-positive precision dilution.
//!
//! Symmetric stripping requires a schema bump: every existing
//! `evidence_fts_cjk` / `evidence_fts_bigram` row predates the
//! stripping rule and contains stopword-spanning windows. The v15 ->
//! v16 migration re-tokenises the affected rows from the
//! `evidence_fts` source-of-truth column (which retains the full
//! plaintext for the unicode61 lane). See
//! [`crate::store::migrate_v16_strip_stopwords_from_recall_lanes`].
//!
//! # Per-script inventory
//!
//! Stopwords are organised by **script**, not by BCP-47 language
//! tag, for three reasons:
//!
//! 1. The trigram / bigram lanes are routed by codepoint membership
//!    (`crate::script::contains_cjk_or_thai`) — not by language
//!    detection. Some routed bodies have `language_tag = NULL`
//!    (whatlang refuses to classify Tibetan or Lao) or a mis-detected
//!    tag (mixed-script content); the script-based stopword filter
//!    sidesteps that uncertainty.
//! 2. Most CJK stopwords are 1-codepoint each (`の`, `の`, `は`,
//!    `を`, `が`, `に`, `で`, `と`, `も`, ...) and are
//!    language-exclusive at the codepoint level (`の` is only used
//!    in Japanese; treating it as a Japanese-only stopword vs. a
//!    pan-CJK stopword makes no operational difference because it
//!    doesn't naturally occur in Chinese or Korean text — stripping
//!    it from a zh document is a no-op).
//! 3. Avoids a dependency on `observation_engine` (which owns the
//!    per-language `stop_words` field on `LanguageLexicon`) from
//!    `evidence_store`. The two stopword consumers serve different
//!    purposes — the lexicon stopwords gate the capitalised-token
//!    entity heuristic via per-candidate equality, the FTS5
//!    stopwords gate trigram / bigram windowing via codepoint
//!    substring excision — and decoupling them avoids cross-crate
//!    coupling for two genuinely different consumers.
//!
//! Inventory size is deliberately conservative: only the most
//! frequent function words / particles per script that have no
//! plausible content-word interpretation. Aggressive stopword
//! lists risk stripping content (e.g. Japanese `年` "year" or
//! Chinese `年` could be a stopword in date contexts but is content
//! in `2024年` "year 2024"). Curated per-script entries below are
//! all unambiguously functional.

use std::borrow::Cow;

/// Japanese particles and copulas with no plausible content-word
/// interpretation. All single-codepoint hiragana except `です` /
/// `ます` (sentence-end copulas, 2 codepoints each).
///
/// Sourced from JLPT N5 grammatical particle inventory; cross-
/// checked against MeCab IPAdic part-of-speech tags `助詞`
/// (particle) and `助動詞` (auxiliary verb). Excludes punctuation
/// (those are unicode61-tokenised away in the source body before
/// reaching this stripping layer).
pub const STOPWORDS_JA: &[&str] = &[
    "の", "は", "が", "を", "に", "へ", "と", "で", "や", "も", "か", "ね", "よ", "な", "だ",
    "です", "ます",
];

/// Chinese (Mandarin) function words. Single-codepoint Han
/// particles, copulas, and connectives only — content-bearing
/// codepoints (personal pronouns `我` / `你` / `他` / `她` / `它`,
/// directional `上` / `下`, demonstratives `这` / `那`, modal
/// `有`, interrogatives `什么` / `怎么`) are **excluded** from the
/// recall-lane strip because they participate in content queries
/// (e.g. a user typing `什么是 AI` ("what is AI") expects the
/// `什么` codepoints to anchor the trigram match against any body
/// answering that question). The unicode61 baseline lane catches
/// those terms via BM25 idf without the false-positive risk that
/// substring stripping would create.
///
/// Sourced from CC-CEDICT high-frequency function-word labels;
/// cross-checked against the HSK 1 grammatical word list. The
/// classical-Chinese particle `之` is included for documents that
/// quote classical sources (common in scholarly Chinese KChat
/// content).
pub const STOPWORDS_ZH: &[&str] = &[
    "的", "了", "在", "是", "和", "也", "就", "都", "而", "及", "与", "或", "之",
];

// Korean Hangul is intentionally **not** included in the recall-
// lane stopword inventory. Hangul codepoints (`U+AC00..=U+D7AF`)
// are excluded from `crate::script::is_cjk_or_thai_codepoint`
// because the unicode61 tokeniser segments Korean cleanly at the
// eojeol (whitespace) boundary, so Korean rows never route into
// the trigram / bigram lanes in the first place. Adding Korean
// particles here would also be hazardous: most 1-codepoint
// Hangul particles (`은`, `이`, `가`, `를`, `도`, ...) are also
// the first syllable of common content words (`은행` "bank",
// `이름` "name", `가족` "family", `를` is exclusively a particle,
// `도시` "city"), and substring-based stripping has no way to
// distinguish the two. The unicode61 lane's BM25 inverse-document-
// frequency naturally discounts the high-frequency Korean
// particles without the false-positive risk.

/// Thai function words. Thai is unsegmented (no whitespace
/// between words), so these strings act as codepoint-substring
/// matches inside larger phrases (e.g. `ของเรา` "of ours" →
/// stripping `ของ` leaves `เรา`).
///
/// Sourced from the Royal Institute of Thailand's official
/// stop-word list (excerpt of most-frequent function words);
/// cross-checked against PyThaiNLP's `thai_stopwords()` corpus.
/// Restricted to entries that are unambiguously functional —
/// content-bearing items found in many published Thai stop-word
/// lists are **deliberately excluded** from the recall-lane
/// strip:
///
/// * Time deictics `วันนี้` ("today"), `พรุ่งนี้` ("tomorrow"),
///   `เมื่อวาน` ("yesterday") — users explicitly query these as
///   content (e.g. a query of `วันนี้` against a body containing
///   `วันนี้` should hit).
/// * Modal `ได้` ("able/can") and negation `ไม่` ("not") — these
///   are productive content-bearing morphemes; stripping them
///   would silently turn "X cannot Y" into the same indexed form
///   as "X can Y".
/// * Existential `มี` ("to have") — a content verb in most
///   contexts.
/// * Demonstratives `นี้` ("this") and `นั้น` ("that") — used
///   as content in noun-phrases like `บ้านนี้` ("this house").
/// * `วัน` ("day") — content word in date contexts
///   (`วันเกิด` "birthday").
///
/// The unicode61 baseline lane catches these via BM25 idf without
/// the false-positive risk that substring stripping would create.
pub const STOPWORDS_TH: &[&str] = &["และ", "หรือ", "ที่", "ใน", "บน", "ของ", "เป็น", "จะ", "แล้ว"];

/// Tibetan particles. Tibetan uses the tsheg (`U+0F0B`) as an
/// intra-word codepoint separator (not a word boundary), so
/// unicode61 segments aggressively at the tsheg and the
/// stopword particles slip through as standalone tokens. The
/// trigram / bigram lanes window across the tsheg so stripping
/// the particles (and any trailing tsheg) removes the noise.
///
/// Sourced from the Tibetan Buddhist Resource Center (TBRC)'s
/// classical Tibetan grammar inventory; modern Tibetan usage on
/// kchat-style platforms is functionally identical for the
/// connective / aspectual entries below. Demonstratives `དེ`
/// ("that") and `འདི` ("this") are **excluded** for the same
/// reason demonstratives are excluded from the Chinese and Thai
/// inventories — they participate in content queries (e.g.
/// `འདི་ནི` "this is" naming a referent the user wants to find).
pub const STOPWORDS_BO: &[&str] = &["ནི", "དང", "ལ", "ཡིན", "རེད", "ཡོད", "མེད"];

/// Khmer function words. Khmer is unsegmented and uses the
/// invisible coeng (`U+17D2`) virama to stack subscript
/// consonants; the trigram / bigram lanes window across the
/// coeng-joined glyphs, so stripping the particles below removes
/// the cross-boundary noise.
///
/// Sourced from the Khmer Wikipedia stop-word list (cross-
/// referenced against the SIL Khmer Reference Grammar's
/// "particle" section). Restricted to function-word entries with
/// no content-word interpretation. The following are
/// **deliberately excluded** as content:
///
/// * `មាន` ("to have") — productive content verb.
/// * `ការ` ("the (action of)") — productive content nominaliser;
///   appears as the first morpheme of every gerund-style
///   construction (e.g. `ការងារ` "work").
/// * Demonstratives `នេះ` ("this") and `នោះ` ("that") — used as
///   content in noun-phrases.
pub const STOPWORDS_KM: &[&str] = &["និង", "នៅ", "ជា"];

/// Myanmar (Burmese) function words. Myanmar is unsegmented and
/// uses subscript consonant stacking via the asat (`U+103A`) and
/// virama (`U+1039`); the trigram / bigram lanes window across
/// these, so stripping function-word particles removes the
/// cross-boundary noise.
///
/// Sourced from the Myanmar Language Commission's official
/// particle inventory (the entries below are the canonical
/// case markers `သည်` / `က` / `ကို` / `မှ` / `နှင့်` / `တွင်` /
/// `မှာ`). Demonstratives `ဤ` ("this") and `ထို` ("that") are
/// **excluded** consistent with the demonstrative-exclusion rule
/// applied to every other script in this module.
pub const STOPWORDS_MY: &[&str] = &["သည်", "က", "နှင့်", "မှ", "ကို", "တွင်", "မှာ"];

/// Lao function words. Lao shares Tai linguistic structure with
/// Thai and is also unsegmented; stopword particles are the Lao
/// cognates of the Thai entries above (e.g. `ໃນ` = Thai `ใน`
/// "in", `ຂອງ` = Thai `ของ` "of").
///
/// Sourced from the Lao Wikipedia stop-word list; cross-checked
/// against the Pan-Lao corpus annotations published by the Lao
/// Ministry of Information & Culture. Demonstratives `ນີ້`
/// ("this") and `ນັ້ນ` ("that") and modal `ມີ` ("to have") are
/// **excluded** for parity with the Thai exclusion rationale.
pub const STOPWORDS_LO: &[&str] = &["ໃນ", "ຂອງ", "ແລະ", "ຫຼື", "ເປັນ"];

/// All recall-lane stopwords across every script, in a single
/// slice. Order in this constant is purely a documentation /
/// readability concern — the [`strip_recall_lane_stopwords`]
/// matcher computes the longest match at each codepoint position
/// dynamically and is **order-independent**. This is the
/// architecturally robust choice: adding a new entry never
/// requires re-sorting, and the matcher correctness is invariant
/// under any permutation of this slice.
///
/// The entries below are grouped by source-script for human
/// readability; the grouping has no effect on the matching
/// algorithm or its output.
pub const ALL_RECALL_LANE_STOPWORDS: &[&str] = &[
    // Japanese (hiragana particles + copulas). All pure-grammar.
    "です",
    "ます",
    "の",
    "は",
    "が",
    "を",
    "に",
    "へ",
    "と",
    "で",
    "や",
    "も",
    "か",
    "ね",
    "よ",
    "な",
    "だ",
    // Chinese (Han function words). Pronouns, demonstratives,
    // and interrogatives intentionally omitted — see
    // STOPWORDS_ZH doc-comment.
    "的",
    "了",
    "在",
    "是",
    "和",
    "也",
    "就",
    "都",
    "而",
    "及",
    "与",
    "或",
    "之",
    // (Korean Hangul deliberately excluded — see comment above
    // STOPWORDS_TH for the rationale.)
    // Thai (function words). Time deictics, modals, and
    // demonstratives intentionally omitted — see STOPWORDS_TH
    // doc-comment.
    "และ",
    "หรือ",
    "ที่",
    "ใน",
    "บน",
    "ของ",
    "เป็น",
    "จะ",
    "แล้ว",
    // Tibetan (classical particles). Demonstratives omitted.
    "ནི",
    "དང",
    "ལ",
    "ཡིན",
    "རེད",
    "ཡོད",
    "མེད",
    // Khmer (function words). Content nominaliser `ការ` and
    // demonstratives omitted.
    "និង",
    "នៅ",
    "ជា",
    // Myanmar (case markers). Demonstratives omitted.
    "သည်",
    "နှင့်",
    "တွင်",
    "မှာ",
    "ကို",
    "က",
    "မှ",
    // Lao (function-word cognates of Thai). Demonstratives and
    // existential `ມີ` omitted.
    "ໃນ",
    "ຂອງ",
    "ແລະ",
    "ຫຼື",
    "ເປັນ",
];

/// Strip recall-lane stopwords from `text`, replacing each match
/// with a single ASCII space. Returns `Cow::Borrowed(text)` when
/// no stopword appears, avoiding an allocation for stopword-free
/// inputs (Latin-only bodies, CJK content-word-dense bodies).
///
/// # Algorithm
///
/// Single-pass codepoint scan over `text`. At each codepoint
/// boundary, the scanner enumerates every entry in
/// [`ALL_RECALL_LANE_STOPWORDS`] that matches starting at the
/// current byte position and selects the **longest** match
/// (computed dynamically; the constant's source order is
/// irrelevant). Matches are replaced with a single ASCII space;
/// non-matching codepoints are copied verbatim. The dynamic
/// longest-match rule means a contributor adding a new stopword
/// never has to think about source ordering — the matcher is
/// correct under any permutation of `ALL_RECALL_LANE_STOPWORDS`.
///
/// # Space replacement vs. removal
///
/// Stopwords are replaced with `' '` rather than removed
/// entirely. The space replacement preserves the **non-adjacency
/// of the surrounding content codepoints** — without it, the
/// stripping would join unrelated content tokens into a single
/// trigram / bigram window. For body
/// `日本のオリンピック` (Japanese: "Japan's Olympics"):
///
/// * Stripping `の` → removal:  body becomes `日本オリンピック`
///   (8 codepoints). Trigram windows: `日本オ`, `本オリ`, `オリン`,
///   ... The first trigram `日本オ` would falsely match
///   `日本オーストラリア` ("Japan-Australia") which shares the
///   adjacent `日本オ` substring even though the original text
///   semantically said `日本 の オリンピック` ("Japan + 's +
///   Olympics") — a different concept.
/// * Stripping `の` → space replacement: body becomes
///   `日本 オリンピック` (with explicit whitespace). The unicode61
///   trigram tokeniser treats whitespace as a token separator, so
///   the windows are `日本` (rejected — < 3 codepoints, no trigram
///   produced) and `オリン`, `リンピ`, `ンピッ`, `ピック`. The
///   spurious `日本オ` cross-particle window is eliminated.
///
/// The unicode61 source-of-truth lane (`evidence_fts.content`)
/// never sees the stripping — the full plaintext stays addressable
/// via the universal lexical index for any Latin / Cyrillic /
/// Greek / Arabic / Hebrew / Devanagari / Hangul terms embedded
/// in the CJK body.
///
/// # Performance
///
/// Worst case is O(`text.len()` * `ALL_RECALL_LANE_STOPWORDS.len()`)
/// — each codepoint may probe every stopword. In practice the
/// scanner short-circuits aggressively: most codepoints don't
/// match any stopword and the first-character `starts_with` probe
/// returns `false` immediately. For a 10 KB body with no stopwords
/// the function is a 10 KB pointer copy and the `Cow::Borrowed`
/// fast path; with stopwords the cost grows linearly in match
/// count, not body size.
pub fn strip_recall_lane_stopwords(text: &str) -> Cow<'_, str> {
    // Fast path: scan once to detect whether any stopword appears.
    // If none, return `Cow::Borrowed` without allocating. The
    // detection scan is O(n * m) worst-case but short-circuits on
    // the first `starts_with` match; in the common case (pure-Han
    // content-word-dense bodies) no stopword is found and we exit
    // early with zero allocation.
    let mut probe = text;
    let mut any_match = false;
    while !probe.is_empty() {
        if ALL_RECALL_LANE_STOPWORDS
            .iter()
            .any(|sw| probe.starts_with(sw))
        {
            any_match = true;
            break;
        }
        // Advance by one codepoint (variable byte width in UTF-8).
        let ch_len = probe.chars().next().map_or(1, char::len_utf8);
        probe = &probe[ch_len..];
    }
    if !any_match {
        return Cow::Borrowed(text);
    }

    // Allocation path: build the stripped output. We size for the
    // input length on the assumption that stopwords compose a
    // small fraction of the body (typical CJK content is content-
    // word dense, particles compose roughly 10-20% of codepoints).
    let mut out = String::with_capacity(text.len());
    let mut remaining = text;
    while !remaining.is_empty() {
        // Dynamic longest-match: enumerate every entry that
        // matches at the current position and pick the longest
        // by byte length. This is O(m) per position where m is
        // the stopword count, but the `starts_with` probe
        // short-circuits on the first codepoint mismatch so the
        // constant factor is small. The dynamic-max rule is
        // correct under any ordering of
        // `ALL_RECALL_LANE_STOPWORDS` — a contributor adding a
        // new entry never has to think about sort order.
        let matched = ALL_RECALL_LANE_STOPWORDS
            .iter()
            .filter(|sw| remaining.starts_with(*sw))
            .max_by_key(|sw| sw.len());
        if let Some(sw) = matched {
            out.push(' ');
            remaining = &remaining[sw.len()..];
        } else {
            // No stopword at this position — copy one codepoint
            // and advance.
            let ch = remaining
                .chars()
                .next()
                .expect("non-empty remaining slice has at least one codepoint");
            out.push(ch);
            remaining = &remaining[ch.len_utf8()..];
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_script_lists_are_non_empty() {
        // Every script's stopword list must contain at least one
        // entry — an empty list means the lane has no precision
        // benefit and the script's inclusion in the recall lanes
        // is purely additive recall (which is fine, but the
        // empty list should be explicit at the type level so a
        // contributor adding a new script remembers to populate
        // it).
        for (name, list) in [
            ("STOPWORDS_JA", STOPWORDS_JA),
            ("STOPWORDS_ZH", STOPWORDS_ZH),
            ("STOPWORDS_TH", STOPWORDS_TH),
            ("STOPWORDS_BO", STOPWORDS_BO),
            ("STOPWORDS_KM", STOPWORDS_KM),
            ("STOPWORDS_MY", STOPWORDS_MY),
            ("STOPWORDS_LO", STOPWORDS_LO),
        ] {
            assert!(!list.is_empty(), "{name} must contain at least one entry");
        }
    }

    #[test]
    fn all_stopwords_contains_every_per_script_entry() {
        // The unified `ALL_RECALL_LANE_STOPWORDS` slice must
        // contain every entry from every per-script slice. A
        // contributor adding a new entry to a per-script list
        // without adding it to `ALL_RECALL_LANE_STOPWORDS` would
        // silently ship a partially-stripped index — the lane
        // SQL only consults `ALL_RECALL_LANE_STOPWORDS`.
        for (name, list) in [
            ("STOPWORDS_JA", STOPWORDS_JA),
            ("STOPWORDS_ZH", STOPWORDS_ZH),
            ("STOPWORDS_TH", STOPWORDS_TH),
            ("STOPWORDS_BO", STOPWORDS_BO),
            ("STOPWORDS_KM", STOPWORDS_KM),
            ("STOPWORDS_MY", STOPWORDS_MY),
            ("STOPWORDS_LO", STOPWORDS_LO),
        ] {
            for entry in list {
                assert!(
                    ALL_RECALL_LANE_STOPWORDS.contains(entry),
                    "{name} entry {entry:?} missing from ALL_RECALL_LANE_STOPWORDS"
                );
            }
        }
    }

    #[test]
    fn strip_picks_longest_match_regardless_of_source_order() {
        // Pin the dynamic-longest-match invariant on an input
        // where the source ordering of
        // `ALL_RECALL_LANE_STOPWORDS` could otherwise produce a
        // shorter wrong match — Japanese `です` (2 chars, polite
        // copula) starts with `で` (1 char, te-form particle).
        // A naive first-match algorithm could strip `で` first
        // and leave the dangling `す` codepoint to contaminate
        // downstream trigram windows. The dynamic
        // `max_by_key(sw.len)` rule sidesteps that hazard
        // independent of source order.
        let stripped = strip_recall_lane_stopwords("今日はいい天気です");
        // `は` (topic marker) strips, then later `です` (full
        // copula) strips as a single 2-char unit — not `で`
        // alone followed by `す`.
        assert_eq!(stripped, "今日 いい天気 ");
    }

    #[test]
    fn all_stopwords_are_unique() {
        // Duplicate entries in `ALL_RECALL_LANE_STOPWORDS` would
        // not break correctness but waste cycles on every probe.
        // Pin uniqueness so the inventory stays minimal.
        let mut seen = std::collections::BTreeSet::new();
        for sw in ALL_RECALL_LANE_STOPWORDS {
            assert!(
                seen.insert(*sw),
                "ALL_RECALL_LANE_STOPWORDS contains duplicate entry {sw:?}"
            );
        }
    }

    #[test]
    fn strip_preserves_empty_input() {
        let stripped = strip_recall_lane_stopwords("");
        assert_eq!(stripped, "");
        // Empty input takes the Cow::Borrowed fast path.
        assert!(matches!(stripped, Cow::Borrowed("")));
    }

    #[test]
    fn strip_returns_borrowed_for_stopword_free_text() {
        // Pure-Latin input has no recall-lane stopwords — should
        // return `Cow::Borrowed` without allocating.
        let text = "Hello, world!";
        let stripped = strip_recall_lane_stopwords(text);
        assert_eq!(stripped, text);
        assert!(matches!(stripped, Cow::Borrowed(_)));
    }

    #[test]
    fn strip_returns_borrowed_for_cjk_content_words_only() {
        // CJK content-word-dense text (no particles) — should
        // also take the Cow::Borrowed fast path.
        let text = "東京オリンピック開会式"; // "Tokyo Olympics opening ceremony"
        let stripped = strip_recall_lane_stopwords(text);
        assert_eq!(stripped, text);
        assert!(matches!(stripped, Cow::Borrowed(_)));
    }

    #[test]
    fn strip_japanese_particle_no() {
        // Body: "Japan's Olympics" — strip the genitive particle.
        let stripped = strip_recall_lane_stopwords("日本のオリンピック");
        assert_eq!(stripped, "日本 オリンピック");
    }

    #[test]
    fn strip_japanese_topic_marker_wa() {
        let stripped = strip_recall_lane_stopwords("今日は良い天気");
        // `は` (topic marker) stripped; remaining content joined
        // by space; no other stopwords (`良`, `い`, `天`, `気`
        // are content codepoints; `今`, `日` are content
        // codepoints).
        assert_eq!(stripped, "今日 良い天気");
    }

    #[test]
    fn strip_japanese_polite_copula_desu_longest_first() {
        // Body ends with `です` (2-codepoint polite copula). The
        // greedy longest-first matcher must strip `です` as a
        // single unit, not the 1-codepoint `で` followed by the
        // residual `す` codepoint (which is not a stopword).
        let stripped = strip_recall_lane_stopwords("良い天気です");
        assert_eq!(stripped, "良い天気 ");
    }

    #[test]
    fn strip_chinese_de_particle() {
        // Body: "Japan's weather" — strip the genitive `的`.
        let stripped = strip_recall_lane_stopwords("日本的天气");
        assert_eq!(stripped, "日本 天气");
    }

    #[test]
    fn strip_thai_function_words() {
        // Body: "weather of us" — strip the preposition `ของ`
        // ("of"). Content-bearing time deictic `วันนี้` is
        // deliberately NOT in the stopword inventory (see
        // STOPWORDS_TH doc-comment), so a body containing it
        // would survive the strip unchanged; this test instead
        // pins one of the inventory's remaining pure-function
        // words.
        let stripped = strip_recall_lane_stopwords("อากาศของเรา");
        assert_eq!(stripped, "อากาศ เรา");
    }

    #[test]
    fn strip_does_not_touch_thai_time_deictic_wannii() {
        // Pin the deliberate-exclusion contract for `วันนี้`
        // ("today"). A previous draft of this module had it in
        // STOPWORDS_TH, which caused a test of the integration
        // path (`fts5_thai_query_returns_hit`, body
        // `อากาศวันนี้ดี` queried by `วันนี้`) to fail with 0
        // hits because both sides stripped the content word out
        // of the bigram/trigram windows. The current inventory
        // omits `วันนี้`, so a body containing it round-trips
        // verbatim through the strip and the integration query
        // hits as expected.
        let body = "อากาศวันนี้ดี";
        let stripped = strip_recall_lane_stopwords(body);
        assert_eq!(stripped, body);
        assert!(matches!(stripped, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn strip_mixed_script_only_affects_recall_lane_codepoints() {
        // Body mixes Latin and Japanese. The Latin portion has
        // no recall-lane stopwords; the Japanese particle `の`
        // is the only stripped codepoint.
        let stripped = strip_recall_lane_stopwords("Hello 日本の世界");
        assert_eq!(stripped, "Hello 日本 世界");
    }

    #[test]
    fn strip_does_not_split_classical_chinese_proper_nouns() {
        // The classical particle `之` is in the stopword list and
        // strips even inside a proper noun like `孔子之言` ("the
        // words of Confucius"). This is acceptable: the trigram
        // lane re-windows the residual `孔子` + `言` after
        // stripping, which keeps both content tokens addressable.
        let stripped = strip_recall_lane_stopwords("孔子之言");
        assert_eq!(stripped, "孔子 言");
    }

    #[test]
    fn strip_pure_stopword_input_yields_only_whitespace() {
        // Input consisting entirely of stopwords reduces to one
        // space per stopword. The trigram / bigram tokeniser
        // treats whitespace as a separator so the residual
        // produces zero windows — i.e. the lane contributes
        // nothing for stopword-only inputs. Correct: a query of
        // pure particles is uninformative.
        let stripped = strip_recall_lane_stopwords("のはがを");
        assert_eq!(stripped, "    ");
    }

    #[test]
    fn strip_is_idempotent_on_already_stripped_input() {
        // Stripping twice produces the same output as stripping
        // once. This is the symmetry-preservation property the
        // index-time and query-time paths rely on (the schema
        // migration applies the strip exactly once per row).
        let once = strip_recall_lane_stopwords("日本のオリンピック");
        let twice = strip_recall_lane_stopwords(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn strip_does_not_touch_korean_hangul_content() {
        // Korean Hangul codepoints (`U+AC00..=U+D7AF`) are
        // intentionally excluded from the recall-lane stopword
        // inventory because the unicode61 lane segments Korean
        // cleanly at the eojeol (whitespace) boundary and BM25
        // idf already discounts the high-frequency particles
        // (`은`, `는`, `이`, `가`, `의`). Substring-based
        // stripping would false-positive on common content words
        // like `도시` ("city" — starts with the particle
        // codepoint `도`) and `은행` ("bank" — starts with `은`),
        // so Korean is left out entirely. This test pins that
        // contract on a pure-Hangul body with embedded particles
        // (`은`, `의`) — none of which strip — so a future
        // contributor who adds Korean entries triggers a failure.
        let body = "서울은 일본의 도시"; // "Seoul is a city of Japan"
        let stripped = strip_recall_lane_stopwords(body);
        assert_eq!(stripped, body);
        // The fast path must short-circuit on stopword-free
        // Korean input — verifying `Cow::Borrowed` pins the zero-
        // allocation guarantee that protects the unicode61 lane
        // from gratuitous re-tokenisation work.
        assert!(matches!(stripped, std::borrow::Cow::Borrowed(_)));
    }
}
