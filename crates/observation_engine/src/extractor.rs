//! Lexicon-first observation extraction.
//!
//! The lexicon baseline. No model required; produces
//! [`crate::types::Observation`]s by scanning the input text for:
//!
//! * **Entities** — capitalised tokens (people / projects), `@`-mentions,
//!   `#`-tags.
//! * **Tasks** — sentences containing `TODO`, `ACTION`, `TASK`,
//!   `please`, or imperative-leading verbs.
//! * **Decisions** — sentences containing `decided`, `agreed`,
//!   `approved`, `ratified`, `signed off`.
//! * **Facts** — declarative-looking sentences that are not tasks,
//!   decisions, or pure entity mentions.
//!
//! Confidence values are intentionally fixed per type; the next
//! pipeline stage (XLM-R + SLM-assisted extraction)
//! refines them.
//!
//! ## multilingual sentence + question handling
//!
//! [`split_sentences_with_terminator`] recognises CJK
//! (`。！？`), Arabic (`؟ ۔`), Devanagari (`। ॥`),
//! Armenian (`։`), Ethiopic (`።`), Tibetan (`། ༎`),
//! Khmer (`។`), and Myanmar (`။`) sentence terminators
//! alongside ASCII (`. ! ? \n`). [`looks_like_question`] consults the per-language
//! interrogative tables in [`crate::interrogatives`] and falls
//! back to first-token English matching when the sentence's
//! detected language is unknown.
//!
//! [`LexiconExtractor::extract`] runs [`crate::language::detect_language`]
//! once on the whole input as the *dominant* language (stamped
//! onto entity-class observations whose source spans the whole
//! text), and once per sentence as the *sentence* language
//! (stamped onto sentence-class observations — Task / Decision /
//! Question / Fact). Per-sentence detection that comes back
//! `None` (whatlang refused to classify a short sentence) falls
//! back to the dominant language, so a long English message with
//! one terse `"Yes."` reply doesn't lose the language stamp on
//! the short sentence.

use std::borrow::Cow;

use evidence_store::ScopeId;

use crate::interrogatives::interrogatives_for;
use crate::language::{detect_language, LanguageTag};
use crate::lexicon::{
    default_registry, normalize_for_lookup, table_matches, KeywordClass, LanguageLexicon,
    LexiconRegistry, MatchStrategy,
};
use crate::types::{Observation, ObservationType};

/// Extract structured observations from raw evidence text.
///
/// # Language stamping contract
///
/// Implementations of `extract` are responsible for stamping each
/// returned [`Observation`]'s `language_tag` field. of
/// the multilingual roadmap moved language detection from the
/// pipeline level (one tag per whole message) to the extractor
/// level (per-sentence for sentence-class observations, dominant
/// for entity-class observations) so that mixed-language messages
/// preserve the per-fragment language of each observation.
///
/// Implementors should follow the convention used by
/// [`LexiconExtractor`]:
///
/// * **Sentence-class observations** ([`ObservationType::Decision`],
///   [`ObservationType::Task`], [`ObservationType::Question`],
///   [`ObservationType::Fact`]) — call
///   [`crate::language::detect_language`] on the individual
///   sentence the observation was extracted from. Falling back to
///   the whole-message dominant tag when the sentence is too
///   short to classify is acceptable and documented.
/// * **Entity-class observations** ([`ObservationType::Entity`]) —
///   span the whole message rather than a single sentence; use
///   the whole-input dominant tag computed once over `text`.
///
/// Both are nullable: when `detect_language` returns `None` (text
/// not classifiable / not reliable), the observation's
/// `language_tag` must remain `None` rather than substituting a
/// default — downstream consumers treat `None` as "unknown" and
/// fail-closed on language-dependent operations (
/// `LexiconRegistry` lookup, FTS5 tokenizer selection).
///
/// # Mutual-delegation hazard ⚠
///
/// `extract_with_dominant_language` has a default implementation
/// that delegates to [`Self::extract`] (dropping the hint). This
/// is convenient for legacy implementors who only know about the
/// single-arg form, but it creates an **infinite recursion trap**
/// for implementors that try to mirror [`LexiconExtractor`]'s
/// previous pattern of having `extract` delegate to
/// `extract_with_dominant_language(None)`:
///
/// ```text
/// // ⚠ INFINITE RECURSION — do NOT do this:
/// impl ObservationExtractor for MyExtractor {
///     fn extract(&self, text: &str, scope: ScopeId) -> Vec<Observation> {
///         self.extract_with_dominant_language(text, scope, None) // calls default
///     } // → calls self.extract() → loop
///     // ... no override of extract_with_dominant_language
/// }
/// ```
///
/// **Correct pattern** (what `LexiconExtractor` does internally):
/// route both trait methods to a single private helper on the
/// concrete type. That keeps the trait surface flexible (callers
/// can pick either entry point) without coupling the two trait
/// methods to each other:
///
/// ```text
/// impl MyExtractor {
///     fn do_extract(&self, text: &str, scope: ScopeId, hint: Option<&LanguageTag>) -> Vec<Observation> {
///         // ... actual work, hint is honoured or ignored as appropriate
///     }
/// }
/// impl ObservationExtractor for MyExtractor {
///     fn extract(&self, text: &str, scope: ScopeId) -> Vec<Observation> {
///         self.do_extract(text, scope, None)
///     }
///     fn extract_with_dominant_language(&self, text: &str, scope: ScopeId, hint: Option<&LanguageTag>) -> Vec<Observation> {
///         self.do_extract(text, scope, hint)
///     }
/// }
/// ```
pub trait ObservationExtractor {
    /// Run the extractor over `text`, returning all observations
    /// found in the supplied `scope`. Implementations must stamp
    /// each observation's `language_tag` per the trait-level
    /// contract above.
    fn extract(&self, text: &str, scope: ScopeId) -> Vec<Observation>;

    /// Same as [`Self::extract`], but accepts a pre-computed
    /// **dominant** language hint to avoid re-running
    /// [`crate::language::detect_language`] on the whole input
    /// when the caller already detected it (e.g. for row-level
    /// metadata). Per-sentence detection still runs inside the
    /// extractor — only the whole-input call is skipped.
    ///
    /// The default implementation ignores the hint and calls
    /// [`Self::extract`] so existing implementations don't need
    /// to be updated. Implementations that care about the
    /// dominant tag (e.g. for entity-class stamping) should
    /// override this method to honour the hint.
    ///
    /// **Do not** delegate back to [`Self::extract`] from a custom
    /// [`Self::extract`] override — see the mutual-delegation
    /// hazard noted in the trait-level documentation above.
    fn extract_with_dominant_language(
        &self,
        text: &str,
        scope: ScopeId,
        _dominant_language: Option<&LanguageTag>,
    ) -> Vec<Observation> {
        self.extract(text, scope)
    }
}

/// Lexicon extractor (`docs/DESIGN.md` §3.2 first pass).
///
/// keyword tables come from a
/// [`LexiconRegistry`] indexed by BCP-47 primary subtag, so
/// each sentence is classified against keywords from the
/// sentence's detected language rather than against a single
/// hard-coded English keyword set. The legacy single-language
/// inline constructor [`Self::new`] still exists for callers
/// that want fully custom keyword lists.
#[derive(Debug, Clone)]
pub struct LexiconExtractor {
    /// Per-sentence keyword source. See [`LexiconSource`] for
    /// the two supported variants.
    source: LexiconSource,
}

/// Where [`LexiconExtractor`] gets its per-sentence keyword
/// tables from. Internal — exposed via the public
/// [`LexiconExtractor::with_registry`] (registry-backed) and
/// [`LexiconExtractor::new`] (inline single-language) ctors.
#[derive(Debug, Clone)]
enum LexiconSource {
    /// Registry-backed: per-sentence lookup of
    /// [`LanguageLexicon`] by detected language tag, falling
    /// back to the English lexicon when the tag is `None` or
    /// unconfigured.
    Registry(&'static LexiconRegistry),
    /// Legacy single-language inline keywords. Applied to
    /// every sentence regardless of detected language —
    /// equivalent to a single-language registry containing
    /// only an English lexicon with the supplied entries.
    /// Preserved for back-compat with earlier callers
    /// of [`LexiconExtractor::new`].
    Inline {
        decision_keywords: Vec<String>,
        task_keywords: Vec<String>,
        task_imperative_verbs: Vec<String>,
        stop_words: Vec<String>,
    },
}

impl Default for LexiconExtractor {
    fn default() -> Self {
        Self::english_default()
    }
}

impl LexiconExtractor {
    /// Build with explicit single-language lexicons. The
    /// supplied keyword lists are applied to every sentence
    /// regardless of detected language — equivalent to a
    /// single-language registry.
    ///
    /// New callers should prefer [`Self::with_registry`] for
    /// multilingual matching. This constructor is retained
    /// for back-compat with earlier call sites that
    /// pass tenant-specific keyword overrides.
    pub fn new(
        decision_keywords: Vec<&str>,
        task_keywords: Vec<&str>,
        task_imperative_verbs: Vec<&str>,
        stop_words: Vec<&str>,
    ) -> Self {
        Self {
            source: LexiconSource::Inline {
                decision_keywords: decision_keywords
                    .into_iter()
                    .map(str::to_lowercase)
                    .collect(),
                task_keywords: task_keywords.into_iter().map(str::to_lowercase).collect(),
                task_imperative_verbs: task_imperative_verbs
                    .into_iter()
                    .map(str::to_lowercase)
                    .collect(),
                stop_words: stop_words.into_iter().map(str::to_lowercase).collect(),
            },
        }
    }

    /// Build with a registry-backed source. Per-sentence
    /// language detection picks the right [`LanguageLexicon`]
    /// from the registry; sentences whose detected language
    /// has no configured lexicon fall back to the registry's
    /// English lexicon.
    pub fn with_registry(registry: &'static LexiconRegistry) -> Self {
        Self {
            source: LexiconSource::Registry(registry),
        }
    }

    /// Default extractor: registry-backed with the built-in
    /// [`default_registry`]. Replaces the earlier
    /// English-only inline lexicon — the built-in registry's
    /// English lexicon carries the same keyword entries as
    /// the earlier inline default, plus 15 additional
    /// languages with their own keyword tables.
    pub fn english_default() -> Self {
        Self::with_registry(default_registry())
    }
}

/// One sentence and the punctuation `char` that ended it.
///
/// The terminator is `None` for the trailing fragment of
/// unterminated input. Stored as a `char` (not a `u8`) because
/// supports multi-byte UTF-8 terminators — CJK `。`,
/// Arabic `؟`, Devanagari `।`, Tibetan `།`, Khmer `។`,
/// Myanmar `။`, etc. — that don't fit in a single byte.
#[derive(Debug, Clone, Copy)]
struct SentenceSlice<'a> {
    text: &'a str,
    terminator: Option<char>,
}

/// All sentence-terminator code points the splitter recognises.
///
/// Coverage rationale (multilingual
/// roadmap):
///
/// * `. ! ? \n` — Latin script (English, Spanish, French,
///   German, Portuguese, Italian, Vietnamese, Indonesian, Malay,
///   …) and any line-terminated text.
/// * `。` (U+3002 IDEOGRAPHIC FULL STOP) — CJK (Japanese,
///   Chinese, sometimes Korean).
/// * `！` (U+FF01 FULLWIDTH EXCLAMATION MARK),
///   `？` (U+FF1F FULLWIDTH QUESTION MARK) — CJK; the
///   fullwidth forms are the canonical CJK question /
///   exclamation terminators.
/// * `؟` (U+061F ARABIC QUESTION MARK),
///   `۔` (U+06D4 ARABIC FULL STOP) — Arabic, Persian, Urdu.
///   (Arabic does not use the ASCII `!` as a sentence
///   terminator, but tolerates ASCII `.`; both ASCII and Arabic
///   terminators are accepted.)
/// * `।` (U+0964 DEVANAGARI DANDA),
///   `॥` (U+0965 DEVANAGARI DOUBLE DANDA) — Hindi, Marathi,
///   Nepali, Sanskrit.
/// * `։` (U+0589 ARMENIAN FULL STOP) — Armenian. The Armenian
///   question mark `՞` is a *combining* mark placed on the
///   stressed vowel of the interrogative word, so it is *not*
///   used as a sentence terminator and is intentionally absent
///   here.
/// * `።` (U+1362 ETHIOPIC FULL STOP) — Amharic, Tigrinya.
/// * `།` (U+0F0D TIBETAN MARK SHAD),
///   `༎` (U+0F0E TIBETAN MARK NYIS SHAD) — Tibetan single
///   shad ends a clause / sentence; the nyis shad (double
///   shad) ends a paragraph or verse, structurally parallel to
///   the Devanagari single / double danda pair above.
/// * `។` (U+17D4 KHMER SIGN KHAN) — Khmer full stop. The
///   Khmer bariyoosan `៕` (U+17D5) is a paragraph-end marker
///   rather than a sentence-end marker and is intentionally
///   absent here — matching the precedent of excluding the
///   Armenian combining question mark `՞`.
/// * `။` (U+104B MYANMAR SIGN SECTION) — Myanmar / Burmese
///   full stop ("visarga"). The Myanmar little section `၊`
///   (U+104A) is a clause-level marker (Burmese comma) rather
///   than a sentence-end marker and is intentionally absent.
///
/// #BUG_pr-review-job-..._0001 closure:
/// without the Tibetan / Khmer / Myanmar arms a multi-sentence
/// body in these scripts (e.g. `statement1။statement2`) was
/// treated as a single sentence, so (a) the interrogative
/// classifier applied substring matching to the entire body
/// rather than per-sentence, and (b) a body with two
/// declaratives produced one Fact observation instead of two.
/// These three scripts ship full lexicon +
/// interrogative coverage; their terminators belong in the
/// splitter alongside them.
fn is_sentence_terminator(c: char) -> bool {
    matches!(
        c,
        // ASCII / Latin
        '.' | '!' | '?' | '\n'
        // CJK
        | '\u{3002}' // 。
        | '\u{FF01}' // ！
        | '\u{FF1F}' // ？
        // Arabic / Persian / Urdu
        | '\u{061F}' // ؟
        | '\u{06D4}' // ۔
        // Devanagari
        | '\u{0964}' // ।
        | '\u{0965}' // ॥
        // Armenian
        | '\u{0589}' // ։
        // Ethiopic
        | '\u{1362}' // ።
        // Tibetan
        | '\u{0F0D}' // །  shad (sentence / clause end)
        | '\u{0F0E}' // ༎  nyis shad (paragraph / verse end)
        // Khmer
        | '\u{17D4}' // ។  khan (full stop)
        // Myanmar
        | '\u{104B}' // ။  sign section (full stop / visarga)
    )
}

/// All recognised question-terminator code points. A subset of
/// the sentence terminators — only those that unambiguously
/// signal a question.
fn is_question_terminator(c: char) -> bool {
    matches!(
        c,
        '?'
        | '\u{FF1F}' // ？  CJK fullwidth question
        | '\u{061F}' // ؟   Arabic question
    )
}

fn split_sentences_with_terminator(text: &str) -> Vec<SentenceSlice<'_>> {
    let mut out = Vec::new();
    let mut start = 0_usize;
    for (i, c) in text.char_indices() {
        if is_sentence_terminator(c) {
            let s = text[start..i].trim();
            if !s.is_empty() {
                out.push(SentenceSlice {
                    text: s,
                    terminator: Some(c),
                });
            }
            start = i + c.len_utf8();
        }
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        out.push(SentenceSlice {
            text: tail,
            terminator: None,
        });
    }
    out
}

/// question detector — consults the per-language
/// interrogative tables in [`crate::interrogatives`] keyed by the
/// sentence's detected language tag.
///
/// Three signals are checked, in priority order:
///
/// 1. **Question-mark terminator** — ASCII `?`, CJK fullwidth
///    `？`, or Arabic `؟`. An unambiguous question marker; if
///    present, return `true` regardless of language.
/// 2. **Language-specific interrogative** — looked up via
///    [`crate::interrogatives::interrogatives_for`] on the
///    sentence's primary BCP-47 subtag, then matched through the
///    unified [`crate::lexicon::table_matches`] entry point.
///    CJK + Thai + Indic-Brahmic (Hindi / Tibetan / Khmer /
///    Myanmar / Lao) use
///    [`crate::lexicon::MatchStrategy::Substring`] (the
///    interrogative may appear anywhere in the sentence and the
///    scripts are not whitespace-segmented at word boundaries);
///    Vietnamese uses
///    [`crate::lexicon::MatchStrategy::FirstBigram`] (
///    collocation closure — `tại sao` / `khi nào` / `vì sao`
///    are bigram entries while the bare unambiguous
///    interrogatives still match via the first-token arm);
///    Arabic uses
///    [`crate::lexicon::MatchStrategy::FirstTokenWithArabicClitics`]
///    (peels productive Arabic proclitic prefixes
///    `و` / `ف` / `ب` / `ل` and the 2-char definite article
///    `ال` / `أل` from the first token before re-checking
///    equality, so `وكيف` / `فمتى` / `بأي` / `لمن` etc. surface
///    the bare interrogative); the remaining space-separated
///    languages use [`crate::lexicon::MatchStrategy::FirstToken`].
/// 3. **Fallback** — when the language tag is `None` or no
///    table is configured for the language, fall back to the
///    English first-token check so substantive English
///    questions in unknown-language threads still get caught.
///
/// Normalisation closure: the question path delegates to the
/// registry's [`normalize_for_lookup`] primitive, which
/// strips Arabic tashkeel + tatweel (when the language tag is
/// Arabic-script), strips bidi/ZWJ format controls, then
/// NFC-composes + lowercases. Routing the question path through
/// the same primitive means Arabic interrogatives decorated
/// with tashkeel (‏كَيْفَ ‏) now match the canonical table
/// entries (‏كيف ‏) consistently with how decision / task /
/// imperative matching already worked.
///
/// The hot-path caller in [`LexiconExtractor::do_extract`]
/// already normalises every sentence once (for decision / task
/// matching) and calls [`looks_like_question_normalised`]
/// directly with the pre-computed normalised string + primary
/// tag, so the earlier double-normalisation concern on the
/// question path no longer holds. The raw-sentence convenience wrapper below is gated
/// on `cfg(test)` because it has no production caller after
/// the closure: the in-tree unit tests use it to keep their
/// arrange phase a one-liner, but every production-shaped
/// matcher path goes through [`looks_like_question_normalised`]
/// with the pre-normalised string the rest of `do_extract`
/// already shares.
#[cfg(test)]
fn looks_like_question(
    sentence: &str,
    terminator: Option<char>,
    language: Option<&LanguageTag>,
) -> bool {
    let primary_tag = language.map(LanguageTag::primary);
    // Short-circuit on terminator before normalising, mirroring
    // the pre-normalised hot path; normalisation costs an
    // allocation we don't need if `?` / `？` / `؟` already
    // resolved the question.
    if terminator.is_some_and(is_question_terminator) {
        return true;
    }
    let normalised = normalize_for_lookup(sentence, primary_tag);
    looks_like_question_normalised(&normalised, terminator, primary_tag)
}

/// Pre-normalised-input variant of [`looks_like_question`].
///
/// Accepts the sentence after it has already been passed
/// through [`normalize_for_lookup`], plus the primary BCP-47
/// subtag used to perform that normalisation. The hot-path
/// caller in [`LexiconExtractor::do_extract`] computes both
/// values once per sentence (for decision / task matching) and
/// reuses them here. The pre-normalised signature exists
/// the
/// per-sentence question path was
/// re-running the NFC + lowercase + tashkeel/bidi-strip pass
/// over the same sentence that decision/task matching had
/// already normalised.
fn looks_like_question_normalised(
    normalised: &str,
    terminator: Option<char>,
    primary_tag: Option<&str>,
) -> bool {
    if terminator.is_some_and(is_question_terminator) {
        return true;
    }
    if normalised.is_empty() {
        return false;
    }
    // Look up per-language interrogatives; fall back to English
    // when the tag is unknown or unconfigured. Promote the
    // per-language InterrogativeMatch into the unified
    // MatchStrategy so the shared table_matches entry
    // point handles the FirstToken / FirstBigram / Substring
    // semantics in one place.
    let (table, strategy) = primary_tag
        .and_then(interrogatives_for)
        .or_else(|| interrogatives_for("en"))
        .expect("english fallback must always be configured in interrogatives table");
    let strategy = MatchStrategy::from_interrogative_match(strategy);
    table_matches(table, normalised, strategy)
}

fn extract_urls(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut idx = 0;
    while idx < text.len() {
        let rest = &text[idx..];
        let candidate_offset = ["http://", "https://"]
            .iter()
            .filter_map(|prefix| rest.find(prefix).map(|p| (p, prefix.len())))
            .min_by_key(|(p, _)| *p);
        let Some((rel, _prefix_len)) = candidate_offset else {
            break;
        };
        let abs_start = idx + rel;
        let after = &text[abs_start..];
        let url_end = after
            .find(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == ')' || c == ']')
            .map_or(after.len(), |e| e);
        let mut url = &after[..url_end];
        // Trim trailing punctuation that is unlikely to be part of
        // the URL (`.` / `!` / `?` / `\"` / `'`).
        while let Some(last) = url.chars().last() {
            if matches!(last, '.' | '!' | '?' | '"' | '\'') {
                url = &url[..url.len() - last.len_utf8()];
            } else {
                break;
            }
        }
        if url.len() > 8 {
            out.push(url.to_string());
        }
        idx = abs_start + url_end;
    }
    out
}

fn extract_emails(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in
        text.split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == '<' || c == '>')
    {
        let token =
            token.trim_matches(|c: char| matches!(c, '.' | '!' | '?' | '"' | '\'' | '(' | ')'));
        let Some(at) = token.find('@') else { continue };
        if at == 0 || at == token.len() - 1 {
            continue;
        }
        let local = &token[..at];
        let domain = &token[at + 1..];
        if !local
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
        {
            continue;
        }
        if !domain.contains('.') {
            continue;
        }
        if !domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            continue;
        }
        out.push(token.to_string());
    }
    out
}

const MONTH_TOKENS: &[&str] = &[
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
    "jan",
    "feb",
    "mar",
    "apr",
    "jun",
    "jul",
    "aug",
    "sep",
    "sept",
    "oct",
    "nov",
    "dec",
];

const DAY_TOKENS: &[&str] = &[
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
    "today",
    "tomorrow",
    "yesterday",
];

const RELATIVE_DATE_PHRASES: &[&str] = &[
    "next week",
    "last week",
    "next month",
    "last month",
    "next quarter",
];

/// Case-insensitive (ASCII-fold) substring search returning the
/// `(start_byte, end_byte)` span of the first match in `haystack`,
/// or `None`. Operates over `char_indices` so the returned byte
/// offsets are always at valid UTF-8 boundaries even when
/// lowercasing would otherwise change byte length (e.g. Turkish
/// `İ` U+0130 → `i\u{307}`).
fn ci_find(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return Some((0, 0));
    }
    let needle_chars: Vec<char> = needle.chars().map(|c| c.to_ascii_lowercase()).collect();
    let h_chars: Vec<(usize, char)> = haystack.char_indices().collect();
    if needle_chars.len() > h_chars.len() {
        return None;
    }
    for start in 0..=(h_chars.len() - needle_chars.len()) {
        let hits = needle_chars
            .iter()
            .enumerate()
            .all(|(i, &n)| h_chars[start + i].1.to_ascii_lowercase() == n);
        if hits {
            let start_byte = h_chars[start].0;
            let end_byte = if start + needle_chars.len() < h_chars.len() {
                h_chars[start + needle_chars.len()].0
            } else {
                haystack.len()
            };
            return Some((start_byte, end_byte));
        }
    }
    None
}

fn extract_date_refs(text: &str) -> Vec<String> {
    let mut out = Vec::new();

    // Multi-word relative phrases. We scan `text` directly with
    // [`ci_find`] so the matched byte span aligns with valid UTF-8
    // boundaries even when the input contains characters whose
    // lowercase form has a different byte length than the original.
    for phrase in RELATIVE_DATE_PHRASES {
        if let Some((start, end)) = ci_find(text, phrase) {
            out.push(text[start..end].to_string());
        }
    }

    // Day / month tokens — walk the *original* text by char so the
    // emitted span preserves casing without ever indexing through a
    // length-changed lowercase view.
    let mut start: Option<usize> = None;
    let mut last_end = 0usize;
    for (i, c) in text.char_indices() {
        if c.is_alphanumeric() {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            let token = &text[s..i];
            let lower = token.to_ascii_lowercase();
            if DAY_TOKENS.contains(&lower.as_str()) || MONTH_TOKENS.contains(&lower.as_str()) {
                out.push(token.to_string());
            }
        }
        last_end = i + c.len_utf8();
    }
    if let Some(s) = start.take() {
        let token = &text[s..last_end];
        let lower = token.to_ascii_lowercase();
        if DAY_TOKENS.contains(&lower.as_str()) || MONTH_TOKENS.contains(&lower.as_str()) {
            out.push(token.to_string());
        }
    }

    // `Q3 2026` / `Q1 2027` style. The pattern is pure ASCII —
    // `q` / `Q`, an ASCII digit, optional ASCII whitespace, four
    // ASCII digits — so byte indices are always char boundaries.
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 2 <= bytes.len() {
        if bytes[i].eq_ignore_ascii_case(&b'q') && bytes[i + 1].is_ascii_digit() {
            let q_digit = bytes[i + 1];
            if matches!(q_digit, b'1' | b'2' | b'3' | b'4') {
                let mut j = i + 2;
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                    j += 1;
                }
                let mut k = j;
                while k < bytes.len() && bytes[k].is_ascii_digit() {
                    k += 1;
                }
                if k - j == 4 {
                    out.push(text[i..k].to_string());
                    i = k;
                    continue;
                }
            }
        }
        i += 1;
    }

    out.sort();
    out.dedup();
    out
}

fn extract_numeric_refs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // `$5M`, `$50,000`, `$2.5B`.
        if bytes[i] == b'$' {
            let mut j = i + 1;
            while j < bytes.len()
                && (bytes[j].is_ascii_digit() || bytes[j] == b',' || bytes[j] == b'.')
            {
                j += 1;
            }
            if j < bytes.len() && matches!(bytes[j], b'k' | b'K' | b'm' | b'M' | b'b' | b'B') {
                j += 1;
            }
            if j > i + 1 {
                out.push(text[i..j].to_string());
            }
            i = j;
            continue;
        }
        // Numeric + unit (`3 sprints`, `2 weeks`, `48h`).
        if bytes[i].is_ascii_digit() {
            let mut j = i;
            while j < bytes.len()
                && (bytes[j].is_ascii_digit() || bytes[j] == b',' || bytes[j] == b'.')
            {
                j += 1;
            }
            // Optional whitespace then a unit word.
            let after_num = j;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            let mut k = j;
            while k < bytes.len() && bytes[k].is_ascii_alphabetic() {
                k += 1;
            }
            if k > j && (k - j) >= 1 {
                let unit = text[j..k].to_lowercase();
                let units = [
                    "sprint", "sprints", "week", "weeks", "day", "days", "month", "months",
                    "quarter", "quarters", "hour", "hours", "minute", "minutes", "h", "min",
                    "users", "people", "tickets", "issues", "pr", "prs", "%", "percent",
                ];
                if units.iter().any(|u| u == &unit.as_str()) {
                    out.push(text[i..k].to_string());
                    i = k;
                    continue;
                }
            }
            // Lone number like `42`. Skip — too noisy.
            i = after_num;
            continue;
        }
        i += 1;
    }
    out
}

fn extract_at_mentions(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j > i + 1 {
                out.push(format!("@{}", &text[i + 1..j]));
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// Extract capitalised tokens from `text`, skipping any that
/// match the caller-supplied `is_stop_word` predicate. The
/// predicate receives each candidate token in its original
/// (mixed) case and is responsible for any case folding it
/// needs to do internally.
///
/// collocation closure: this used to take
/// `stop_words: &[String]` and compare via
/// `str::eq_ignore_ascii_case`, which only folds the ASCII
/// A–Z / a–z range. That silently failed for stop-words
/// outside ASCII — e.g. Russian Cyrillic (Это vs. это), or
/// Vietnamese with prefixed `Đ` / `đ` (Đó vs. đó). Routing the
/// stop-word check through a caller-owned predicate lets
/// [`LexiconExtractor::is_stop_word`] use the Unicode-aware
/// [`str::to_lowercase`] fold against the lexicon's
/// already-lowercase entries, which is the same normalisation
/// the rest of the lexicon matcher uses. The signature change
/// also removes the per-call
/// `Vec<String>` allocation the previous shape required for the
/// registry-backed path.
fn extract_capitalised_words(text: &str, is_stop_word: impl Fn(&str) -> bool) -> Vec<String> {
    // collocation closure: fold
    // typographic / modifier apostrophe variants — U+2019 RIGHT
    // SINGLE QUOTATION MARK (the standard French / English IME
    // / typographically-correct apostrophe used by Word /
    // macOS smart-quotes / iOS), U+2018 LEFT SINGLE QUOTATION
    // MARK (smart-quote rendering of an opening apostrophe),
    // and U+02BC MODIFIER LETTER APOSTROPHE (used in some
    // Romanisations and African-language orthographies) — to
    // ASCII U+0027 before tokenising. The split predicate
    // below treats ASCII `'` as part of a token but treats
    // U+2019 / U+2018 / U+02BC as non-alphabetic separators
    // (U+02BC is technically `Lm` / alphabetic per Unicode but
    // we still fold it for table consistency). Without
    // folding, French `Aujourd\u{2019}hui` would tokenise as
    // `["Aujourd", "hui"]` and `Aujourd` would not match the
    // `aujourd'hui` stop-word entry written with ASCII `'`,
    // emitting it as a false-positive entity. Folding once
    // here also normalises the returned entity form so
    // downstream consumers see the canonical ASCII shape. Same
    // benefit applies to English contractions (`Don\u{2019}t`),
    // Italian elisions (`L\u{2019}altro`), Catalan / Occitan,
    // and any other language whose IME produces typographic
    // apostrophes by default. The cross-language stop-word
    // invariant test
    // [`no_stop_word_entry_contains_typographic_apostrophe`]
    // pins the contract that stop-word entries use ASCII `'`
    // only, so the fold-then-compare path always converges.
    let folded = fold_typographic_apostrophes(text);
    let mut out = Vec::new();
    for raw in folded.split(|c: char| !c.is_alphabetic() && c != '\'') {
        if raw.is_empty() {
            continue;
        }
        let mut chars = raw.chars();
        let first = chars.next().unwrap();
        if first.is_uppercase() && raw.chars().count() >= 2 && !is_stop_word(raw) {
            out.push(raw.to_string());
        }
    }
    out
}

/// Fold typographic / modifier apostrophe variants (U+2019,
/// U+2018, U+02BC) to ASCII U+0027 so the lexicon tokeniser and
/// stop-word lookup see a single canonical apostrophe shape
/// regardless of input source. Allocates a new `String` only
/// when at least one variant is present; otherwise returns the
/// input by borrow.
fn fold_typographic_apostrophes(text: &str) -> Cow<'_, str> {
    if text
        .chars()
        .any(|c| matches!(c, '\u{2019}' | '\u{2018}' | '\u{02BC}'))
    {
        Cow::Owned(
            text.chars()
                .map(|c| match c {
                    '\u{2019}' | '\u{2018}' | '\u{02BC}' => '\'',
                    other => other,
                })
                .collect(),
        )
    } else {
        Cow::Borrowed(text)
    }
}

/// A sentence is considered "shaped" enough to be a Fact
/// candidate when it has either a whitespace separator (Latin /
/// Cyrillic / Arabic / Devanagari / etc.) **or** is a run of at
/// least 4 codepoints in a no-inter-word-whitespace script. The
/// no-whitespace scripts currently recognised are CJK, Thai,
/// Lao, Khmer, Myanmar (Burmese), and Tibetan — the six major
/// living Asian scripts that do not separate words with
/// whitespace. Without this fallback, the "contains space" gate
/// would silently drop every sentence in those scripts as
/// not-fact-shaped.
///
/// The codepoint set is intentionally **decoupled** from
/// whatlang's detected-language set: whatlang 0.18 ships
/// classifiers for CJK / Thai / Khmer / Myanmar but NOT for
/// Lao or Tibetan (`Lang::Bod` is absent). The fact-shape gate
/// still runs when language detection produces `None`, and the
/// substrate ships `lo` and `bo` lexicons reachable via
/// explicit-tag callers (FFI / connector pipelines that stamp
/// the language tag directly). The Lao / Tibetan codepoint
/// arms exist so a body in either script is admitted as a Fact
/// candidate on shape alone, with `language_tag = None`
/// surfaced by the fail-closed contract when whatlang cannot
/// classify it.
///
/// History: the predicate started as CJK-only and grew to
/// cover Thai declaratives
/// like `กรุงเทพมหานครเป็นเมืองหลวงของประเทศไทย`), then Lao /
/// Khmer / Myanmar, then Tibetan and Myanmar Extended-A / -B.
/// A later revision closed the asymmetry on Khmer Symbols
/// (U+19E0..=U+19FF) inside `is_khmer_codepoint`. See the
/// matching earlier-review findings for the script-coverage
/// extension history.
fn is_sentence_shaped_for_fact(sentence: &str) -> bool {
    if sentence.contains(' ') {
        return true;
    }
    let unsegmented_chars = sentence
        .chars()
        .filter(|c| {
            is_cjk_codepoint(*c)
                || is_thai_codepoint(*c)
                || is_lao_codepoint(*c)
                || is_khmer_codepoint(*c)
                || is_myanmar_codepoint(*c)
                || is_tibetan_codepoint(*c)
        })
        .count();
    unsegmented_chars >= 4
}

/// True for code points in the CJK script blocks. Used by the
/// sentence-shape heuristic and by the per-sentence-language
/// fallback path.
///
/// **Lockstep contract.** This predicate is kept in lockstep
/// with [`evidence_store::script::is_cjk_or_thai_codepoint`]
/// (the FTS5 routing predicate) — same defense-in-depth
/// principle as `is_khmer_codepoint` / `is_myanmar_codepoint` /
/// `is_tibetan_codepoint` below. Any codepoint that FTS5 routes
/// to the `evidence_fts_cjk` / `evidence_fts_bigram` lanes must
/// also be admitted by the fact-shape gate; otherwise a body
/// composed entirely of (say) CJK Compatibility Ideographs or
/// Halfwidth Katakana would be indexed-and-searchable but
/// silently rejected from becoming a Fact observation. The
/// predicate has since been extended to cover the full CJK
/// block set, closing the same kind of asymmetry that the
/// Myanmar Extended-A / -B and Khmer Symbols additions closed
/// for the Brahmic scripts.
///
/// **Coverage.**
///
/// * Hiragana (`U+3040..=U+309F`)
/// * Katakana (`U+30A0..=U+30FF`)
/// * Katakana Phonetic Extensions (`U+31F0..=U+31FF`) — small
///   kana used in Ainu transliteration and Japanese linguistics
/// * Halfwidth Katakana (`U+FF65..=U+FF9F`) — JIS X 0201-derived
///   half-width forms commonly emitted by legacy Japanese IMEs,
///   mobile-carrier SMS / SS7 gateways, and Japanese telephony
///   systems
/// * CJK Radicals Supplement (`U+2E80..=U+2EFF`) — Kangxi radical
///   components used in dictionaries and IME candidate lists
/// * CJK Unified Ideographs Extension A (`U+3400..=U+4DBF`)
/// * CJK Unified Ideographs (`U+4E00..=U+9FFF`)
/// * Hangul Syllables (`U+AC00..=U+D7AF`)
/// * CJK Compatibility Ideographs (`U+F900..=U+FAFF`) — duplicates
///   of Han characters preserved for round-trip compatibility
///   with legacy charsets (`KS X 1001`, `JIS X 0213`, `Big5`)
/// * CJK Unified Ideographs Extension B (`U+20000..=U+2A6DF`)
/// * CJK Unified Ideographs Extensions C..F + I
///   (`U+2A700..=U+2EE5F`, contiguous; Extension I added in
///   Unicode 15.1)
/// * CJK Unified Ideographs Extensions G..H + J
///   (`U+30000..=U+33479`, contiguous; Extension J added in
///   Unicode 16.0)
///
/// The standing policy is the same as the FTS5 routing
/// predicate: **"every currently-defined CJK Unified Ideographs
/// Extension is admitted"**. A future contributor extending the
/// predicate for Unicode 17+ Extension K / L / ... only needs to
/// widen the upper bound of whichever contiguous arm the new
/// block belongs to.
fn is_cjk_codepoint(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{309F}'      // Hiragana
        | '\u{30A0}'..='\u{30FF}'    // Katakana
        | '\u{31F0}'..='\u{31FF}'    // Katakana Phonetic Extensions
        | '\u{FF65}'..='\u{FF9F}'    // Halfwidth Katakana
        | '\u{2E80}'..='\u{2EFF}'    // CJK Radicals Supplement
        | '\u{3400}'..='\u{4DBF}'    // CJK Unified Ideographs Extension A
        | '\u{4E00}'..='\u{9FFF}'    // CJK Unified Ideographs
        | '\u{AC00}'..='\u{D7AF}'    // Hangul Syllables
        | '\u{F900}'..='\u{FAFF}'    // CJK Compatibility Ideographs
        | '\u{20000}'..='\u{2A6DF}'  // CJK Unified Ideographs Extension B
        | '\u{2A700}'..='\u{2EE5F}'  // CJK Unified Ideographs Extensions C..F + I
        | '\u{30000}'..='\u{33479}'  // CJK Unified Ideographs Extensions G..H + J
    )
}

/// True for code points in the Thai script block
/// (`U+0E00..U+0E7F`). Thai is the other major living script in
/// the multilingual roadmap that does not use inter-word
/// whitespace, so the fact-shape heuristic accepts a Thai
/// codepoint run on the same terms as a CJK run.
fn is_thai_codepoint(c: char) -> bool {
    matches!(c, '\u{0E00}'..='\u{0E7F}')
}

/// True for code points in the Lao script block
/// (`U+0E80..U+0EFF`). Lao is a sister script to Thai (Brahmic
/// family, descended from the Khom script) and shares the same
/// no-inter-word-whitespace convention. whatlang 0.18 does not
/// currently detect Lao (the next-largest open-source detector
/// `lingua-rs` does — this arm anticipates the LexiconRegistry
/// with richer detection), but the
/// fact-shape gate runs even when language detection produces
/// `None`, so this codepoint check ensures a Lao declarative is
/// still admitted as a Fact candidate on shape alone, with the
/// `language_tag` stayed at `None` via the fail-closed contract.
fn is_lao_codepoint(c: char) -> bool {
    matches!(c, '\u{0E80}'..='\u{0EFF}')
}

/// True for code points in either Khmer block:
///
/// * Khmer main block (`U+1780..=U+17FF`) — consonants, vowels,
///   the invisible coeng (`U+17D2`), and the standard prose
///   inventory.
/// * Khmer Symbols (`U+19E0..=U+19FF`) — astronomical / lunar
///   date symbols used in liturgical / horoscopic corpora.
///
/// Both blocks are routed to the FTS5 dual / bigram lanes by
/// [`crate::script::is_cjk_or_thai_codepoint`]. This
/// predicate is kept in lockstep with the FTS5 routing predicate
/// — same defense-in-depth principle as Myanmar Extended-A / -B
/// above and Tibetan below: a body composed entirely of symbols
/// from the supplementary block must be admitted by the
/// fact-shape gate on the same terms as it is indexed by FTS5.
/// Without the supplement arm such a body would be indexed-and-
/// searchable but never become a Fact observation.
///
/// Khmer is Brahmic-derived with no inter-word whitespace
/// (though it does use whitespace between phrases / clauses, so
/// many Khmer sentences fall through the `sentence.contains(' ')`
/// fast path; this arm is the safety net for clause-internal
/// Khmer runs). whatlang detects Khmer as the `khm` enum
/// variant, mapped to BCP-47 `km`.
fn is_khmer_codepoint(c: char) -> bool {
    matches!(c,
        '\u{1780}'..='\u{17FF}'   // Khmer main
        | '\u{19E0}'..='\u{19FF}' // Khmer Symbols (astronomical / lunar date)
    )
}

/// True for code points in any of the three Myanmar script
/// blocks:
///
/// * Myanmar main block (`U+1000..=U+109F`) — Burmese plus the
///   shared consonant inventory.
/// * Myanmar Extended-A (`U+AA60..=U+AA7F`) — Pao + Pwo Karen
///   letters extending the main block (Unicode 5.2, 2009).
/// * Myanmar Extended-B (`U+A9E0..=U+A9FF`) — Shan additions
///   (Unicode 7.0, 2014).
///
/// All three blocks are routed to the FTS5 dual / bigram lanes
/// by [`crate::script::is_cjk_or_thai_codepoint`].
/// This predicate is kept in lockstep with the FTS5 routing
/// predicate so that a body in a Myanmar minority script (e.g.
/// pure Shan text using Extended-B codepoints) is admitted by
/// the extractor's fact-shape gate on the same terms as it is
/// indexed by the FTS5 layer. Without the Extended-A/-B arms
/// the asymmetry was: such bodies would be indexed-and-
/// searchable but never become Fact observations.
///
/// whatlang detects Burmese as the `mya` enum variant, mapped
/// to BCP-47 `my`.
fn is_myanmar_codepoint(c: char) -> bool {
    matches!(c,
        '\u{1000}'..='\u{109F}'   // Myanmar main
        | '\u{AA60}'..='\u{AA7F}' // Myanmar Extended-A (Pao + Pwo Karen)
        | '\u{A9E0}'..='\u{A9FF}' // Myanmar Extended-B (Shan)
    )
}

/// True for code points in the Tibetan script block
/// (`U+0F00..=U+0FFF`). Tibetan is a Brahmic-derived script
/// that uses the tsheg (`U+0F0B`) as a *syllable* separator,
/// not a word boundary, so the `sentence.contains(' ')` fast
/// path in [`is_sentence_shaped_for_fact`] does not fire for
/// pure-Tibetan bodies and this codepoint gate is what admits
/// them as Fact candidates.
///
/// whatlang 0.18 does NOT ship a Tibetan classifier
/// ([`Lang::Bod`](https://docs.rs/whatlang/0.18.0/whatlang/enum.Lang.html)
/// is absent), so the per-sentence detector will normally
/// leave the `language_tag` as `None` for Tibetan bodies — but
/// the fact-shape gate runs even when language detection
/// produces `None`, and ships a `bo` lexicon
/// reachable via explicit-tag callers (FFI / connector
/// pipelines), so the Tibetan body is fully indexable, fully
/// FTS-routable, and (with this arm) fully Fact-eligible.
/// Without this arm the asymmetry was: a Tibetan sentence
/// without ASCII spaces would route to `evidence_fts_cjk` /
/// `evidence_fts_bigram` via
/// [`crate::script::is_cjk_or_thai_codepoint`] but silently
/// fail the fact-shape gate and never become a Fact
/// observation. Same defense-in-depth principle as the Lao /
/// Khmer / Myanmar arms.
fn is_tibetan_codepoint(c: char) -> bool {
    matches!(c, '\u{0F00}'..='\u{0FFF}')
}

impl LexiconExtractor {
    /// Look up the per-sentence [`LanguageLexicon`] for a
    /// detected primary BCP-47 subtag. Returns `None` when the
    /// extractor is in legacy inline-keywords mode (which
    /// doesn't carry per-language lexicons).
    fn lexicon_for(&self, primary_tag: Option<&str>) -> Option<&'static LanguageLexicon> {
        match &self.source {
            LexiconSource::Registry(reg) => Some(reg.lexicon_for_or_english(primary_tag)),
            LexiconSource::Inline { .. } => None,
        }
    }

    /// True when `normalised_sentence` matches the keyword
    /// table for the requested class.
    ///
    /// In registry-backed mode the lookup is per-sentence-language
    /// (with English fallback for unconfigured tags). In legacy
    /// inline mode the inline keyword list is applied
    /// regardless of language (matching earlier behaviour
    /// exactly, including the substring-style match for the
    /// inline decision / task lists).
    fn sentence_matches_class(
        &self,
        normalised_sentence: &str,
        primary_tag: Option<&str>,
        class: KeywordClass,
    ) -> bool {
        match &self.source {
            LexiconSource::Registry(_) => {
                let Some(lex) = self.lexicon_for(primary_tag) else {
                    return false;
                };
                let Some((table, strategy)) = lex.entries(class) else {
                    return false;
                };
                table_matches(table, normalised_sentence, strategy)
            }
            LexiconSource::Inline {
                decision_keywords,
                task_keywords,
                task_imperative_verbs,
                stop_words: _,
            } => match class {
                KeywordClass::Decision => {
                    // Legacy behaviour was substring match across
                    // the whole lowercased sentence; reproduce that
                    // here rather than switching inline-mode callers
                    // to FirstToken silently.
                    decision_keywords
                        .iter()
                        .any(|n| normalised_sentence.contains(n.as_str()))
                }
                KeywordClass::Task => task_keywords
                    .iter()
                    .any(|n| normalised_sentence.contains(n.as_str())),
                KeywordClass::TaskImperative => {
                    let first = normalised_sentence
                        .split(|c: char| !c.is_alphabetic())
                        .find(|s| !s.is_empty())
                        .unwrap_or("");
                    !first.is_empty() && task_imperative_verbs.iter().any(|v| v == first)
                }
                KeywordClass::Stopword | KeywordClass::Interrogative => false,
            },
        }
    }

    /// True when `raw` (in its original mixed case) matches a
    /// stop-word entry for `dominant_language`'s lexicon under
    /// Unicode-aware lowercase folding. Used by the
    /// capitalised-token entity extractor to skip candidates
    /// that are actually function words.
    ///
    /// In registry-backed mode the lookup uses the dominant
    /// language's lexicon, falling back to the English lexicon's
    /// stop-words for unconfigured languages. In legacy inline
    /// mode it uses the inline stop-word list (already
    /// lowercased by [`LexiconExtractor::new`]).
    ///
    /// collocation closure: the earlier path
    /// compared via `str::eq_ignore_ascii_case` which silently
    /// failed for non-ASCII stop-words. We now lowercase the
    /// raw candidate once via [`str::to_lowercase`] (Unicode-
    /// aware) and compare against the already-lowercase entries,
    /// matching what the rest of the lexicon matcher does.
    ///
    /// collocation closure: the predicate shape
    /// avoids the per-call `Vec<String>` allocation the previous
    /// `stop_words_for_entity_extraction` returned for the
    /// registry path. The lowercase allocation per candidate is
    /// the minimum required for Unicode-correct case folding.
    ///
    /// The capitalised-token extractor is itself only
    /// meaningful for case-bearing scripts, so this predicate is
    /// mostly relevant for Latin / Cyrillic / Greek / Armenian —
    /// CJK / Arabic / Thai naturally emit no capitalised-token
    /// candidates and therefore reach this method zero times for
    /// those scripts.
    fn is_stop_word(&self, raw: &str, dominant_language: Option<&LanguageTag>) -> bool {
        let lowered = raw.to_lowercase();
        match &self.source {
            LexiconSource::Registry(reg) => {
                let primary = dominant_language.map(LanguageTag::primary);
                let lex = reg.lexicon_for_or_english(primary);
                lex.stop_words.iter().any(|s| *s == lowered)
            }
            LexiconSource::Inline { stop_words, .. } => stop_words.iter().any(|s| s == &lowered),
        }
    }

    /// Shared implementation behind both [`ObservationExtractor::extract`]
    /// and [`ObservationExtractor::extract_with_dominant_language`].
    ///
    /// Routing both trait methods through this private helper
    /// instead of having one trait method call the other avoids
    /// the mutual-delegation infinite-recursion trap documented on
    /// the [`ObservationExtractor`] trait.
    fn do_extract(
        &self,
        text: &str,
        scope: ScopeId,
        dominant_language: Option<&LanguageTag>,
    ) -> Vec<Observation> {
        let mut out = Vec::new();
        let mut seen_entities: std::collections::HashSet<String> = std::collections::HashSet::new();

        // dominant language for entity-class observations
        // (mentions / URLs / dates / numerics span the whole input,
        // so the dominant language is the only language that makes
        // semantic sense to stamp on them).
        //
        // `dominant_language` is treated as authoritative: callers
        // are responsible for running [`detect_language`] on the
        // whole input (or for explicitly supplying `None` when
        // detection failed) before calling `do_extract`. We do not
        // re-run detection here — a `None` hint means "detection
        // already ran and produced no language", not "detection has
        // not been attempted". This avoids a redundant trigram pass
        // on every call.
        let dominant_language = dominant_language.cloned();

        // Entity extraction over the entire input.
        for mention in extract_at_mentions(text) {
            if seen_entities.insert(mention.clone()) {
                out.push(
                    Observation::new_candidate(ObservationType::Entity, mention, scope, 0.85)
                        .with_language_tag(dominant_language.clone()),
                );
            }
        }
        // stop-word check uses
        // [`Self::is_stop_word`], which routes to the per-message
        // dominant language's lexicon when registry-backed, falls
        // back to the English lexicon's stop-words for
        // unconfigured languages, and falls back to the inline
        // list when the extractor was constructed via the legacy
        // [`LexiconExtractor::new`] path. Comparison is Unicode-
        // lowercase aware (closing the collocation gap) so
        // Cyrillic and Vietnamese stop-words match their
        // capitalised forms. The capitalised-token entity
        // heuristic is itself only meaningful for case-bearing
        // scripts (Latin / Cyrillic / Greek / Armenian / …), but
        // we run it unconditionally — sentences in CJK / Arabic
        // / Thai contain no capitalised tokens, so the heuristic
        // naturally emits no results for those scripts and the
        // predicate is consulted zero times for those scripts.
        let dominant_for_stop_words = dominant_language.as_ref();
        for word in
            extract_capitalised_words(text, |raw| self.is_stop_word(raw, dominant_for_stop_words))
        {
            if seen_entities.insert(word.clone()) {
                out.push(
                    Observation::new_candidate(ObservationType::Entity, word, scope, 0.55)
                        .with_language_tag(dominant_language.clone()),
                );
            }
        }
        for url in extract_urls(text) {
            if seen_entities.insert(url.clone()) {
                out.push(
                    Observation::new_candidate(ObservationType::Entity, url, scope, 0.9)
                        .with_language_tag(dominant_language.clone()),
                );
            }
        }
        for email in extract_emails(text) {
            if seen_entities.insert(email.clone()) {
                out.push(
                    Observation::new_candidate(ObservationType::Entity, email, scope, 0.9)
                        .with_language_tag(dominant_language.clone()),
                );
            }
        }
        for date_ref in extract_date_refs(text) {
            if seen_entities.insert(date_ref.clone()) {
                out.push(
                    Observation::new_candidate(ObservationType::Entity, date_ref, scope, 0.6)
                        .with_language_tag(dominant_language.clone()),
                );
            }
        }
        for numeric in extract_numeric_refs(text) {
            if seen_entities.insert(numeric.clone()) {
                out.push(
                    Observation::new_candidate(ObservationType::Entity, numeric, scope, 0.7)
                        .with_language_tag(dominant_language.clone()),
                );
            }
        }

        // Sentence-level extraction for tasks / decisions / questions
        // / facts: each sentence is independently
        // language-detected so a bilingual chat message
        // (`"Hello. 안녕하세요. Let's ship Friday."`) gets per-sentence
        // tags. When whatlang refuses to classify a short sentence,
        // fall back to the dominant language so a single `"Yes."`
        // reply in a long English message still inherits `en`.
        for slice in split_sentences_with_terminator(text) {
            let sentence = slice.text;
            let sentence_language = detect_language(sentence)
                .map(|d| d.tag)
                .or_else(|| dominant_language.clone());
            // normalise the sentence ONCE through
            // [`normalize_for_lookup`] (NFC + lowercase +
            // script-aware combining-mark strip) and reuse the
            // result for every keyword class. This is the
            // single normalisation primitive every matcher path
            // shares — see the [`crate::lexicon`] module doc.
            let primary_tag = sentence_language.as_ref().map(|t| t.primary().to_string());
            let normalised = normalize_for_lookup(sentence, primary_tag.as_deref());

            // Class precedence: Question first, then
            // Decision, then Task. Speech-act signals (question
            // terminator `?` / `？` / `؟` + per-language
            // interrogative words) are unambiguous — a sentence
            // that ends in `？` or contains a language-specific
            // interrogative is a question, even when it also
            // contains a polite-request opener like Japanese
            // `お願い`, Spanish `por favor` or Vietnamese
            // `vui lòng` that would otherwise route to the Task
            // class. The earlier inline English keyword set
            // never overlapped this way (English `please` is in
            // the task list but rarely co-occurs with a `?`-
            // terminator), but the per-language lexicons
            // include polite-request openers that ARE common in
            // interrogative sentences, so question-first
            // precedence is required to avoid mis-routing.
            // Decision still wins over Task because announcement
            // sentences like "We decided to please everyone"
            // (decision-class) should not become tasks because
            // they happen to contain the substring `please`.
            // collocation closure: pass the
            // already-normalised sentence + primary tag into the
            // pre-normalised variant so the question path
            // doesn't re-run NFC + lowercase + tashkeel/bidi-
            // strip on the same string that decision/task
            // matching just normalised. The wrapper
            // [`looks_like_question`] is still available for
            // test callers and external users that want the
            // raw-sentence convenience.
            if looks_like_question_normalised(&normalised, slice.terminator, primary_tag.as_deref())
            {
                out.push(
                    Observation::new_candidate(
                        ObservationType::Question,
                        sentence.to_string(),
                        scope,
                        0.7,
                    )
                    .with_language_tag(sentence_language),
                );
                continue;
            }
            if self.sentence_matches_class(
                &normalised,
                primary_tag.as_deref(),
                KeywordClass::Decision,
            ) {
                out.push(
                    Observation::new_candidate(
                        ObservationType::Decision,
                        sentence.to_string(),
                        scope,
                        0.75,
                    )
                    .with_language_tag(sentence_language),
                );
                continue;
            }
            if self.sentence_matches_class(&normalised, primary_tag.as_deref(), KeywordClass::Task)
                || self.sentence_matches_class(
                    &normalised,
                    primary_tag.as_deref(),
                    KeywordClass::TaskImperative,
                )
            {
                out.push(
                    Observation::new_candidate(
                        ObservationType::Task,
                        sentence.to_string(),
                        scope,
                        0.7,
                    )
                    .with_language_tag(sentence_language),
                );
                continue;
            }
            // Anything sentence-shaped — Latin/Cyrillic/Arabic
            // sentences with whitespace OR CJK sentences with at
            // least 4 ideographs — and not picked up as a task /
            // decision / question is a Fact candidate.
            //
            // Two-gate design :
            //
            // * `sentence.len() >= 6` is a **byte-length** lower bound
            //   targeting Latin scripts. It rejects very short ASCII
            //   fragments (`"Hi"`, `"Yes"`, `"OK"`) that are too short
            //   to carry factual content. For 3-byte-per-char scripts
            //   (CJK / Thai) this gate is redundant — any 2-character
            //   run already passes — but it is harmless because the
            //   second gate below tightens the contract for those
            //   scripts.
            // * `is_sentence_shaped_for_fact` is the **codepoint** gate:
            //   for whitespace-bearing scripts (Latin / Cyrillic /
            //   Arabic / Devanagari) it just checks for a space, and
            //   for no-inter-word-whitespace scripts (CJK / Thai) it
            //   requires at least 4 codepoints. The 4-codepoint floor
            //   is what actually rejects short CJK / Thai fragments.
            //
            // Both gates must pass; together they reject both
            // "ASCII fragment too short to be a fact" AND "CJK / Thai
            // fragment too short to be a fact" without needing
            // per-script length thresholds.
            if sentence.len() >= 6 && is_sentence_shaped_for_fact(sentence) {
                out.push(
                    Observation::new_candidate(
                        ObservationType::Fact,
                        sentence.to_string(),
                        scope,
                        0.5,
                    )
                    .with_language_tag(sentence_language),
                );
            }
        }

        out
    }
}

impl ObservationExtractor for LexiconExtractor {
    fn extract(&self, text: &str, scope: ScopeId) -> Vec<Observation> {
        // `extract` is the no-hint entry point: callers (typically
        // tests / direct lexicon-only users) haven't pre-computed
        // a dominant tag. Run [`detect_language`] **once** here and
        // pass the result through to `do_extract`, which treats
        // its hint argument as authoritative and never re-detects.
        // This consolidates the whole-input detection to a single
        // call site so future implementors of
        // [`ObservationExtractor`] don't accidentally duplicate the
        // pass.
        let dominant_language = detect_language(text).map(|d| d.tag);
        self.do_extract(text, scope, dominant_language.as_ref())
    }

    fn extract_with_dominant_language(
        &self,
        text: &str,
        scope: ScopeId,
        dominant_language: Option<&LanguageTag>,
    ) -> Vec<Observation> {
        // The hint is authoritative: callers that supply
        // `Some(tag)` get that tag stamped on entity-class
        // observations, and callers that supply `None` get
        // `None`-stamped entities (i.e. "language unknown"). We do
        // not fall back to running [`detect_language`] on `None`
        // hints — see the comment inside
        // [`Self::do_extract`].
        self.do_extract(text, scope, dominant_language)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_at_mentions_and_capitalised_entities() {
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        let obs = ext.extract("@Sara please loop in Acme on the Migration", scope);
        assert!(obs
            .iter()
            .any(|o| o.observation_type == ObservationType::Entity && o.content == "@Sara"));
        assert!(obs
            .iter()
            .any(|o| o.observation_type == ObservationType::Entity && o.content == "Acme"));
        assert!(obs
            .iter()
            .any(|o| o.observation_type == ObservationType::Entity && o.content == "Migration"));
    }

    #[test]
    fn detects_tasks_via_keywords_and_imperatives() {
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        let obs = ext.extract(
            "TODO: draft the RFC. Please send it by Friday. Schedule a review.",
            scope,
        );
        let tasks: Vec<_> = obs
            .iter()
            .filter(|o| o.observation_type == ObservationType::Task)
            .collect();
        assert!(tasks.len() >= 2);
    }

    #[test]
    fn detects_decisions() {
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        let obs = ext.extract("We decided to ship the launch on Friday.", scope);
        assert!(obs
            .iter()
            .any(|o| o.observation_type == ObservationType::Decision));
    }

    #[test]
    fn declarative_sentences_become_facts() {
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        let obs = ext.extract("The migration ships next Friday.", scope);
        assert!(obs
            .iter()
            .any(|o| o.observation_type == ObservationType::Fact));
    }

    // ===================================================================
    // Multilingual sentence terminator + question detection tests.
    // These exercise the char-based splitter, the per-language
    // interrogative tables, and per-sentence language stamping.
    // ===================================================================

    #[test]
    fn split_sentences_recognises_cjk_terminators() {
        // 3 Japanese sentences ending in 。 ！ ？ with no spaces
        // between them. The byte-based splitter would have read
        // these as one big sentence; the char-based splitter must
        // produce 3 slices.
        let text = "今日は晴れです。とても暑い！明日はどうですか？";
        let slices = split_sentences_with_terminator(text);
        assert_eq!(
            slices.len(),
            3,
            "expected 3 CJK sentences, got {}: {slices:?}",
            slices.len()
        );
        assert_eq!(slices[0].terminator, Some('。'));
        assert_eq!(slices[1].terminator, Some('！'));
        assert_eq!(slices[2].terminator, Some('？'));
    }

    #[test]
    fn split_sentences_recognises_arabic_terminators() {
        // Arabic uses ؟ (U+061F) for questions and ۔ (U+06D4) /
        // ASCII `.` for statements. RTL display order in the source
        // doesn't change the logical sentence boundaries.
        let text = "مرحبا. كيف حالك؟ أنا بخير۔";
        let slices = split_sentences_with_terminator(text);
        assert_eq!(slices.len(), 3, "expected 3 Arabic sentences: {slices:?}");
        assert_eq!(slices[0].terminator, Some('.'));
        assert_eq!(slices[1].terminator, Some('؟'));
        assert_eq!(slices[2].terminator, Some('۔'));
    }

    #[test]
    fn split_sentences_recognises_devanagari_danda() {
        // Hindi sentences end in । (danda) or ॥ (double danda).
        let text = "मैं ठीक हूँ। आप कैसे हैं॥";
        let slices = split_sentences_with_terminator(text);
        assert_eq!(slices.len(), 2);
        assert_eq!(slices[0].terminator, Some('।'));
        assert_eq!(slices[1].terminator, Some('॥'));
    }

    #[test]
    fn split_sentences_recognises_armenian_full_stop() {
        // Armenian uses ։ (U+0589) as the sentence-ending full stop.
        let text = "Բարև։ Ինչպես ես։";
        let slices = split_sentences_with_terminator(text);
        assert_eq!(slices.len(), 2);
        assert_eq!(slices[0].terminator, Some('։'));
        assert_eq!(slices[1].terminator, Some('։'));
    }

    #[test]
    fn split_sentences_recognises_ethiopic_full_stop() {
        // Amharic / Tigrinya use ። (U+1362).
        let text = "ሰላም። እንዴት ነህ።";
        let slices = split_sentences_with_terminator(text);
        assert_eq!(slices.len(), 2);
        assert_eq!(slices[0].terminator, Some('።'));
        assert_eq!(slices[1].terminator, Some('።'));
    }

    #[test]
    fn split_sentences_recognises_tibetan_shad() {
        // Tibetan ends sentences with shad
        // (་།, U+0F0D) and paragraphs / verses with nyis shad
        // (༎, U+0F0E) — structurally parallel to Devanagari
        // single / double danda above. Without these arms in
        // `is_sentence_terminator`, a multi-sentence Tibetan
        // body would be treated as a single sentence and the
        // interrogative + fact-shape classifiers would only
        // apply to the whole thing. Statement 1 = "Lhasa is
        // the capital of Tibet"; statement 2 = "Tibetan is the
        // language of Tibet".
        let text = "ལྷ་ས་ནི་བོད་ཀྱི་རྒྱལ་ས་ཡིན།བོད་སྐད་ནི་བོད་ཀྱི་སྐད་ཡིན༎";
        let slices = split_sentences_with_terminator(text);
        assert_eq!(slices.len(), 2, "expected 2 sentences: {slices:?}");
        assert_eq!(slices[0].terminator, Some('\u{0F0D}'));
        assert_eq!(slices[1].terminator, Some('\u{0F0E}'));
    }

    #[test]
    fn split_sentences_recognises_khmer_khan() {
        // Khmer ends sentences with khan
        // (។, U+17D4). Statement 1 = "Phnom Penh is the
        // capital of Cambodia"; statement 2 = "Khmer is the
        // language of Cambodia".
        let text = "ភ្នំពេញគឺជារដ្ឋធានីនៃប្រទេសកម្ពុជា។ខ្មែរគឺជាភាសានៃប្រទេសកម្ពុជា។";
        let slices = split_sentences_with_terminator(text);
        assert_eq!(slices.len(), 2, "expected 2 sentences: {slices:?}");
        assert_eq!(slices[0].terminator, Some('\u{17D4}'));
        assert_eq!(slices[1].terminator, Some('\u{17D4}'));
    }

    #[test]
    fn split_sentences_recognises_myanmar_visarga() {
        // Myanmar / Burmese ends sentences
        // with sign section / "visarga" (။, U+104B).
        // Statement 1 = "Yangon is the largest city of
        // Myanmar"; statement 2 = "Naypyidaw is the capital
        // of Myanmar".
        let text = "ရန်ကုန်သည်မြန်မာနိုင်ငံ၏အကြီးဆုံးမြို့ဖြစ်သည်။နေပြည်တော်သည်မြန်မာနိုင်ငံ၏မြို့တော်ဖြစ်သည်။";
        let slices = split_sentences_with_terminator(text);
        assert_eq!(slices.len(), 2, "expected 2 sentences: {slices:?}");
        assert_eq!(slices[0].terminator, Some('\u{104B}'));
        assert_eq!(slices[1].terminator, Some('\u{104B}'));
    }

    #[test]
    fn split_sentences_terminators_do_not_swallow_non_terminator_punctuation() {
        // Defense in depth: codepoints adjacent to or visually
        // similar to the terminators must NOT trigger
        // a split. Locks the precision contract so a future
        // sweep doesn't accidentally add the Khmer bariyoosan
        // (\u{17D5}, paragraph-end), Myanmar little section
        // (\u{104A}, clause-comma), or Tibetan rin chen
        // spungs shad (\u{0F11}, ornamental) as sentence
        // terminators.
        for non_terminator in [
            '\u{17D5}', // ៕  Khmer bariyoosan (paragraph end)
            '\u{104A}', // ၊  Myanmar little section (clause comma)
            '\u{0F11}', // ༑  Tibetan rin chen spungs shad
            '\u{0F0C}', // ་ Tibetan delimiter mark tsheg bstar
        ] {
            assert!(
                !is_sentence_terminator(non_terminator),
                "{:?} (U+{:04X}) must not be a sentence terminator",
                non_terminator,
                non_terminator as u32,
            );
        }
    }

    #[test]
    fn split_sentences_mixed_script_message() {
        // The motivating example: a bilingual chat
        // message ("Hello. 안녕하세요. Let's ship Friday.") splits
        // into 3 sentences across Latin + Hangul scripts.
        let text = "Hello. 안녕하세요. Let's ship Friday.";
        let slices = split_sentences_with_terminator(text);
        assert_eq!(slices.len(), 3, "expected 3 sentences: {slices:?}");
        assert_eq!(slices[0].text, "Hello");
        assert_eq!(slices[1].text, "안녕하세요");
        assert!(slices[2].text.contains("Friday"));
    }

    #[test]
    fn cjk_question_terminator_detected_as_question() {
        // The fullwidth ？ alone is enough to mark the sentence
        // as a question even with no interrogative word lookup.
        assert!(looks_like_question("今日は晴れですか", Some('？'), None));
    }

    #[test]
    fn arabic_question_terminator_detected_as_question() {
        assert!(looks_like_question("كيف حالك", Some('؟'), None));
    }

    #[test]
    fn japanese_interrogative_substring_match() {
        // The sentence ends without `？` but contains the
        // interrogative `何`. Substring matching for ja must
        // catch this.
        let ja = LanguageTag::new("ja").unwrap();
        assert!(looks_like_question(
            "今日は何曜日ですか",
            Some('。'),
            Some(&ja)
        ));
    }

    #[test]
    fn japanese_sentence_final_ka_particle_detected() {
        // The Japanese question construction `〜ですか。` ends
        // with `か` before the full stop, with no `？` terminator.
        // Our table includes `ですか` as an interrogative so
        // substring match catches it.
        let ja = LanguageTag::new("ja").unwrap();
        assert!(looks_like_question(
            "明日は晴れですか",
            Some('。'),
            Some(&ja)
        ));
    }

    #[test]
    fn korean_interrogative_substring_match() {
        let ko = LanguageTag::new("ko").unwrap();
        // No `?`; the interrogative `무엇` (what) is in the middle.
        assert!(looks_like_question(
            "오늘은 무엇을 먹을까요",
            Some('.'),
            Some(&ko)
        ));
    }

    #[test]
    fn mandarin_yesno_particle_ma_detected() {
        let zh = LanguageTag::new("zh").unwrap();
        // Mandarin yes/no question with `吗` particle, no `？`.
        assert!(looks_like_question("你好吗", Some('。'), Some(&zh)));
    }

    #[test]
    fn spanish_first_token_interrogative_with_inverted_question_open() {
        let es = LanguageTag::new("es").unwrap();
        // `¿Cómo estás` — the leading `¿` is *not* a sentence
        // terminator (it's an opening punctuation). The
        // interrogative `cómo` is the first alphabetic token.
        assert!(looks_like_question("¿Cómo estás", Some('.'), Some(&es)));
    }

    #[test]
    fn french_first_token_interrogative_no_question_mark() {
        let fr = LanguageTag::new("fr").unwrap();
        assert!(looks_like_question(
            "Pourquoi tu fais ça",
            Some('.'),
            Some(&fr)
        ));
    }

    #[test]
    fn german_first_token_interrogative_no_question_mark() {
        let de = LanguageTag::new("de").unwrap();
        assert!(looks_like_question(
            "Wer hat das gemacht",
            Some('.'),
            Some(&de)
        ));
    }

    #[test]
    fn english_first_token_does_not_substring_match_unrelated_words() {
        // Regression: ensure FirstToken strategy doesn't false-
        // positive on words that *contain* an interrogative as a
        // substring ("whole" contains "who", "whether" contains
        // "when's" stem, etc.).
        let en = LanguageTag::new("en").unwrap();
        assert!(!looks_like_question(
            "The whole team approved this",
            Some('.'),
            Some(&en)
        ));
        assert!(!looks_like_question(
            "Whether we ship Friday is up to legal",
            Some('.'),
            Some(&en)
        ));
    }

    #[test]
    fn looks_like_question_handles_nfd_decomposed_input() {
        // Regression coverage: NFD-decomposed input (e.g.
        // accented Spanish text coming from a macOS file system or
        // some IME pipelines) decomposes `é` into `e + U+0301`
        // (COMBINING ACUTE ACCENT). The FirstToken tokeniser
        // splits on non-alphabetic codepoints, and `U+0301`
        // (category Mn) is non-alphabetic. Without NFC
        // normalisation the tokeniser would split `qué` into
        // `que` + the empty tail after the combining mark, so
        // `que` would never match the NFC-composed table entry
        // `qué`. Confirm both forms now match.
        let es = LanguageTag::new("es").unwrap();
        let nfc = "qué pasa";
        // Manually construct the NFD form (e + COMBINING ACUTE ACCENT).
        let nfd = "que\u{0301} pasa";
        assert_ne!(nfc, nfd, "NFC and NFD forms must differ at the byte level");
        assert!(
            looks_like_question(nfc, Some('.'), Some(&es)),
            "NFC form should match"
        );
        assert!(
            looks_like_question(nfd, Some('.'), Some(&es)),
            "NFD form should match after NFC normalisation"
        );

        // Same check for French `où` (NFD: `o + U+0300` GRAVE).
        let fr = LanguageTag::new("fr").unwrap();
        assert!(
            looks_like_question("où est le bureau", Some('.'), Some(&fr)),
            "french NFC form should match"
        );
        assert!(
            looks_like_question("ou\u{0300} est le bureau", Some('.'), Some(&fr)),
            "french NFD form should match after NFC normalisation"
        );
    }

    #[test]
    fn extractor_stamps_per_sentence_language_in_bilingual_message() {
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        // A bilingual chat message: long English intro + long
        // Japanese question + long English task. Each sentence
        // is long enough on its own that whatlang reliably
        // classifies it — that's the per-sentence guarantee
        // makes.
        let text = "Please review the migration plan for the deadline this Friday. \
                    今日の会議では何時に開始する予定でしょうか、教えてください。 \
                    Please send the agenda document to the entire team today.";
        let obs = ext.extract(text, scope);

        let question = obs
            .iter()
            .find(|o| o.observation_type == ObservationType::Question)
            .expect("japanese question should be detected (interrogative 何 substring match)");
        assert_eq!(
            question
                .language_tag
                .as_ref()
                .map(|t| t.primary().to_string()),
            Some("ja".to_string()),
            "japanese question observation should be tagged `ja`, got {:?}",
            question.language_tag
        );

        // At least one English task observation should be tagged
        // `en`. (Multiple English sentences in the input may both
        // become tasks via the "please" keyword; we just need one
        // to confirm per-sentence detection ran on the English
        // sentences independently of the Japanese sentence.)
        let en_task = obs
            .iter()
            .find(|o| {
                o.observation_type == ObservationType::Task
                    && o.language_tag.as_ref().is_some_and(|t| t.primary() == "en")
            })
            .unwrap_or_else(|| {
                panic!(
                    "expected at least one english-tagged task observation; got tags: {:?}",
                    obs.iter()
                        .filter(|o| o.observation_type == ObservationType::Task)
                        .map(|o| o.language_tag.clone())
                        .collect::<Vec<_>>()
                )
            });
        let _ = en_task;
    }

    #[test]
    fn cjk_fact_shaped_without_whitespace() {
        // Regression: an earlier version of the fact gate required
        // a space character. A CJK declarative sentence has no
        // spaces, so it would have been silently dropped. Verify
        // CJK declaratives now produce Fact candidates.
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        // "Tokyo Tower is 333 meters tall."
        let obs = ext.extract("東京タワーの高さは三百三十三メートルです。", scope);
        assert!(
            obs.iter()
                .any(|o| o.observation_type == ObservationType::Fact),
            "expected at least one Fact from CJK declarative; got {:?}",
            obs.iter().map(|o| o.observation_type).collect::<Vec<_>>()
        );
    }

    #[test]
    fn cjk_sentence_too_short_does_not_become_fact() {
        // The CJK fact gate requires ≥ 4 ideographs to avoid
        // tagging "は" or "です" alone as facts.
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        // Short single-clause utterances — should NOT produce
        // facts.
        let obs = ext.extract("はい。", scope);
        assert!(
            !obs.iter()
                .any(|o| o.observation_type == ObservationType::Fact),
            "expected no Fact from very short CJK utterance"
        );
    }

    #[test]
    fn short_sentence_falls_back_to_dominant_language() {
        // A long English message with a short `"Yes."` sentence.
        // whatlang refuses to classify `"Yes."` alone; the
        // sentence-level tag falls back to the dominant `en`.
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        let text = "The migration is on track and the team approved the rollout plan. Yes.";
        let obs = ext.extract(text, scope);
        // Every observation should be tagged `en` (either via
        // per-sentence detection on the long sentence, or via
        // the fallback for `"Yes."`).
        for o in &obs {
            if let Some(tag) = o.language_tag.as_ref() {
                assert_eq!(
                    tag.primary(),
                    "en",
                    "observation {o:?} should fall back to en"
                );
            }
        }
    }

    #[test]
    fn extractor_entity_class_uses_dominant_language() {
        // @mentions span the whole message, not a single
        // sentence, so they must inherit the *whole-input*
        // dominant language regardless of what individual
        // sentences are tagged with. We compute the expected
        // dominant via the same `detect_language(text)` call the
        // extractor uses, then assert the mention's tag matches.
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        let text = "Hello team please review the migration plan and ship Friday. \
                    @sara can you draft the rollout schedule by tomorrow?";
        let obs = ext.extract(text, scope);
        let mention = obs
            .iter()
            .find(|o| o.observation_type == ObservationType::Entity && o.content == "@sara")
            .expect("@sara mention should be extracted");
        let dominant = detect_language(text).map(|d| d.tag);
        assert_eq!(
            mention.language_tag, dominant,
            "@mention must carry the dominant-language tag computed from the whole input \
             (got {:?}, expected {:?})",
            mention.language_tag, dominant
        );
        // And separately: when the dominant is reliable, the
        // mention should carry that exact tag.
        assert!(
            mention.language_tag.is_some(),
            "english input should produce a reliable dominant tag"
        );
        assert_eq!(mention.language_tag.as_ref().unwrap().primary(), "en");
    }

    #[test]
    fn extractor_entity_dominant_can_be_none_when_input_is_unclassifiable() {
        // Inverse of the above: a short bilingual message that
        // whatlang refuses to classify as a whole. The mention
        // still gets `None`, but per-sentence-detected sentences
        // may still get a confident tag (e.g. a long pure-CJK
        // sentence in the same input). This documents the
        // asymmetry: entity-class tag follows the *dominant*
        // (whole-input) detection; sentence-class tag follows
        // the per-sentence detection. They can diverge.
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        let text = "@bob hi. 今日は会議の時間を変更してもらってもいいですか。";
        let obs = ext.extract(text, scope);
        let mention = obs
            .iter()
            .find(|o| o.observation_type == ObservationType::Entity && o.content == "@bob")
            .expect("@bob mention should be extracted");
        let dominant = detect_language(text).map(|d| d.tag);
        assert_eq!(
            mention.language_tag, dominant,
            "@mention must carry the dominant tag exactly, whatever it is (got {:?}, \
             expected {:?})",
            mention.language_tag, dominant
        );
    }

    #[test]
    fn is_cjk_codepoint_classifies_correctly() {
        // Spot-check the CJK detector.
        assert!(is_cjk_codepoint('東')); // CJK Unified Ideograph
        assert!(is_cjk_codepoint('あ')); // Hiragana
        assert!(is_cjk_codepoint('カ')); // Katakana
        assert!(is_cjk_codepoint('한')); // Hangul Syllables
        assert!(!is_cjk_codepoint('a')); // ASCII
        assert!(!is_cjk_codepoint('ä')); // Latin Extended
        assert!(!is_cjk_codepoint('م')); // Arabic
        assert!(!is_cjk_codepoint('क')); // Devanagari
        assert!(!is_cjk_codepoint('ก')); // Thai (handled separately)

        // Extended CJK coverage mirrors the FTS5 routing
        // predicate `script::is_cjk_or_thai_codepoint`, closing
        // the same kind of lockstep asymmetry that the Myanmar
        // Extended-A / -B and Khmer Symbols additions closed
        // for the Brahmic scripts. Without these arms,
        // a body composed entirely of (say) Halfwidth Katakana
        // or CJK Compatibility Ideographs would be indexed in
        // the dual FTS5 lanes but silently rejected by the
        // fact-shape gate.

        // Halfwidth Katakana (legacy Japanese IME / SMS).
        assert!(is_cjk_codepoint('\u{FF65}')); // first cp
        assert!(is_cjk_codepoint('\u{FF9F}')); // last cp
        assert!(is_cjk_codepoint('カ' /* カ */)); // full-width sanity

        // Katakana Phonetic Extensions (Ainu transliteration).
        assert!(is_cjk_codepoint('\u{31F0}')); // first cp
        assert!(is_cjk_codepoint('\u{31FF}')); // last cp

        // CJK Radicals Supplement (Kangxi radical components).
        assert!(is_cjk_codepoint('\u{2E80}')); // first cp
        assert!(is_cjk_codepoint('\u{2EFF}')); // last cp

        // CJK Compatibility Ideographs (legacy round-trip Han).
        assert!(is_cjk_codepoint('\u{F900}')); // first cp
        assert!(is_cjk_codepoint('\u{FAFF}')); // last cp

        // CJK Unified Ideographs Extension B (supplementary-
        // plane Han, scholarly / historical text).
        assert!(is_cjk_codepoint('\u{20000}')); // first cp
        assert!(is_cjk_codepoint('\u{2A6DF}')); // last cp

        // CJK Unified Ideographs Extensions C..F + I
        // (contiguous range, Ext I added in Unicode 15.1).
        assert!(is_cjk_codepoint('\u{2A700}')); // first cp (Ext C)
        assert!(is_cjk_codepoint('\u{2EBEF}')); // last cp of Ext F
        assert!(is_cjk_codepoint('\u{2EBF0}')); // first cp of Ext I
        assert!(is_cjk_codepoint('\u{2EE5F}')); // last cp (Ext I)

        // CJK Unified Ideographs Extensions G..H + J
        // (contiguous range, Ext J added in Unicode 16.0).
        assert!(is_cjk_codepoint('\u{30000}')); // first cp (Ext G)
        assert!(is_cjk_codepoint('\u{323AF}')); // last cp of Ext H
        assert!(is_cjk_codepoint('\u{323B0}')); // first cp of Ext J
        assert!(is_cjk_codepoint('\u{33479}')); // last cp (Ext J)

        // Boundary checks: codepoints immediately outside each
        // newly-added block must remain outside the predicate.
        // Pins the precision contract so a future contributor
        // doesn't accidentally over-widen the upper bound and
        // start admitting unrelated scripts as CJK.
        assert!(!is_cjk_codepoint('\u{FF64}')); // one below Halfwidth Katakana
        assert!(!is_cjk_codepoint('\u{FFA0}')); // one above Halfwidth Katakana
        assert!(!is_cjk_codepoint('\u{31EF}')); // one below Phonetic Ext
        assert!(!is_cjk_codepoint('\u{3200}')); // one above Phonetic Ext
        assert!(!is_cjk_codepoint('\u{2E7F}')); // one below Radicals Supplement
        assert!(!is_cjk_codepoint('\u{F8FF}')); // one below Compat Ideographs (PUA)
        assert!(!is_cjk_codepoint('\u{FB00}')); // one above Compat Ideographs (Latin presentation)
        assert!(!is_cjk_codepoint('\u{1FFFF}')); // one below Ext B (SMP misc)
        assert!(!is_cjk_codepoint('\u{2A6E0}')); // one above Ext B (CJK Compat Ideographs Supplement starts at 2F800)
        assert!(!is_cjk_codepoint('\u{2A6FF}')); // gap between Ext B and Ext C
        assert!(!is_cjk_codepoint('\u{2EE60}')); // one above Ext I
        assert!(!is_cjk_codepoint('\u{2FFFF}')); // gap between Ext F-I and Ext G
        assert!(!is_cjk_codepoint('\u{3347A}')); // one above Ext J
    }

    #[test]
    fn is_thai_codepoint_classifies_correctly() {
        // Spot-check the Thai detector.
        assert!(is_thai_codepoint('ก')); // Thai consonant ko kai
        assert!(is_thai_codepoint('ท')); // Thai consonant tho thahan
        assert!(is_thai_codepoint('ย')); // Thai consonant yo yak
        assert!(is_thai_codepoint('ไ')); // Thai vowel sara ai mai malai
        assert!(is_thai_codepoint('๛')); // Thai khomut (end-of-text marker)
        assert!(!is_thai_codepoint('a')); // ASCII
        assert!(!is_thai_codepoint('東')); // CJK (handled separately)
        assert!(!is_thai_codepoint('म')); // Devanagari
    }

    #[test]
    fn thai_fact_shaped_without_whitespace() {
        // Regression for :
        // shipped CJK fact-shape support but missed Thai, the
        // other major no-inter-word-whitespace script. A Thai
        // declarative sentence (no spaces, ≥ 4 Thai codepoints)
        // must now produce a Fact candidate.
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        // "Bangkok is the capital of Thailand."
        let obs = ext.extract("กรุงเทพมหานครเป็นเมืองหลวงของประเทศไทย", scope);
        assert!(
            obs.iter()
                .any(|o| o.observation_type == ObservationType::Fact),
            "expected at least one Fact from Thai declarative; got {:?}",
            obs.iter().map(|o| o.observation_type).collect::<Vec<_>>()
        );
    }

    #[test]
    fn thai_sentence_too_short_does_not_become_fact() {
        // Symmetric with the CJK gate: < 4 Thai codepoints +
        // no spaces should not become a Fact, to avoid spurious
        // very-short Thai utterances getting promoted.
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        // "ใช่" — "yes" in Thai, 3 Thai codepoints, no spaces.
        let obs = ext.extract("ใช่", scope);
        assert!(
            !obs.iter()
                .any(|o| o.observation_type == ObservationType::Fact),
            "expected no Fact from very short Thai utterance"
        );
    }

    #[test]
    fn is_lao_khmer_myanmar_codepoint_classifies_correctly() {
        // Regression coverage: widen
        // no-whitespace-script fact-shape coverage from CJK +
        // Thai to also include the three other major Brahmic-
        // family scripts present in whatlang's detection set
        // (Khmer, Myanmar) plus its sibling script Lao
        // (forward-defensive — whatlang 0.18 does not detect
        // Lao but its codepoints are still recognised by the
        // fact-shape gate so a Lao declarative is admitted on
        // shape alone with `language_tag = None`).

        // Lao: spot-check consonants + vowel + tone mark.
        assert!(is_lao_codepoint('ກ')); // Lao letter ko
        assert!(is_lao_codepoint('ນ')); // Lao letter no
        assert!(is_lao_codepoint('ະ')); // Lao vowel sign nyo
        assert!(!is_lao_codepoint('ก')); // Thai (handled separately)
        assert!(!is_lao_codepoint('a')); // ASCII

        // Khmer main: spot-check consonants + sign coeng.
        assert!(is_khmer_codepoint('ក')); // Khmer letter ka
        assert!(is_khmer_codepoint('ម')); // Khmer letter ma
        assert!(is_khmer_codepoint('ែ')); // Khmer vowel sign ae
        assert!(is_khmer_codepoint('្')); // Khmer sign coeng (subscript)
        assert!(!is_khmer_codepoint('म')); // Devanagari (visually similar)
        assert!(!is_khmer_codepoint('a')); // ASCII

        // Khmer Symbols (astronomical /
        // lunar date symbols). The FTS5 routing predicate at
        // `script::is_cjk_or_thai_codepoint` covers the
        // supplementary block, so the extractor predicate must
        // too — otherwise a body composed of pure Khmer Symbols
        // (a calendar or horoscope page) would be indexed in
        // the dual FTS5 lanes but silently rejected by the
        // fact-shape gate. Same lockstep principle as Myanmar
        // Extended-A / -B above.
        assert!(is_khmer_codepoint('᧠')); // U+19E0 first cp of Khmer Symbols
        assert!(is_khmer_codepoint('᧿')); // U+19FF last cp of Khmer Symbols
                                          // Boundary: codepoints just outside the supplement must
                                          // remain outside the predicate.
        assert!(!is_khmer_codepoint('᧟')); // U+19DF (one below — New Tai Lue)
        assert!(!is_khmer_codepoint('ᨀ')); // U+1A00 (one above — Buginese)

        // Myanmar: spot-check consonants + medial.
        assert!(is_myanmar_codepoint('က')); // Myanmar letter ka
        assert!(is_myanmar_codepoint('မ')); // Myanmar letter ma
        assert!(is_myanmar_codepoint('န')); // Myanmar letter na
        assert!(is_myanmar_codepoint('ြ')); // Myanmar consonant sign medial ra
        assert!(!is_myanmar_codepoint('ก')); // Thai (handled separately)
        assert!(!is_myanmar_codepoint('a')); // ASCII

        // Myanmar Extended-A (Pao + Pwo
        // Karen) and Extended-B (Shan). The FTS5 routing
        // predicate at `script::is_cjk_or_thai_codepoint`
        // covers both blocks, so the extractor predicate
        // must too — otherwise a body in a pure-Shan or
        // pure-Pwo-Karen minority script would be indexed in
        // the dual FTS5 lanes but silently rejected by the
        // fact-shape gate.
        assert!(is_myanmar_codepoint('ꩠ')); // first cp of Myanmar Ext-A
        assert!(is_myanmar_codepoint('ꩿ')); // last cp of Myanmar Ext-A
        assert!(is_myanmar_codepoint('ꧠ')); // first cp of Myanmar Ext-B (Shan)
        assert!(is_myanmar_codepoint('꧿')); // last cp of Myanmar Ext-B (Shan)
                                            // Boundary: codepoints just outside Ext-A / Ext-B
                                            // must remain outside the predicate.
        assert!(!is_myanmar_codepoint('꧟')); // U+A9DF (one below Ext-B)
        assert!(!is_myanmar_codepoint('ꪀ')); // U+AA80 (one above Ext-A)

        // Cross-bleed: each script's helper must reject the other
        // two no-whitespace-script ranges to keep the helpers
        // useful as standalone predicates (callers may want to
        // distinguish later for per-script tokeniser routing).
        assert!(!is_lao_codepoint('ក')); // Khmer
        assert!(!is_lao_codepoint('က')); // Myanmar
        assert!(!is_khmer_codepoint('ກ')); // Lao
        assert!(!is_khmer_codepoint('က')); // Myanmar
        assert!(!is_myanmar_codepoint('ກ')); // Lao
        assert!(!is_myanmar_codepoint('ក')); // Khmer
    }

    #[test]
    fn is_tibetan_codepoint_classifies_correctly() {
        // Tibetan is now in the fact-shape
        // gate's codepoint set. Spot-check the predicate at
        // boundaries, against neighbouring scripts in the
        // gate, and against ASCII.
        assert!(is_tibetan_codepoint('ༀ')); // first cp of Tibetan block
        assert!(is_tibetan_codepoint('ཀ')); // Tibetan letter ka
        assert!(is_tibetan_codepoint('་')); // Tibetan tsheg (syllable separator)
        assert!(is_tibetan_codepoint('ས')); // Tibetan letter sa
        assert!(is_tibetan_codepoint('࿿')); // last cp of Tibetan block

        // Boundary: codepoints just outside the block must
        // remain outside the predicate.
        assert!(!is_tibetan_codepoint('໿')); // last cp of Lao (one below)
        assert!(!is_tibetan_codepoint('က')); // first cp of Myanmar (one above)

        // Cross-bleed: Tibetan must reject the other Brahmic /
        // Indic scripts in the fact-shape gate's set, plus
        // ASCII.
        assert!(!is_tibetan_codepoint('ก')); // Thai
        assert!(!is_tibetan_codepoint('ກ')); // Lao
        assert!(!is_tibetan_codepoint('ហ')); // Khmer
        assert!(!is_tibetan_codepoint('က')); // Myanmar
        assert!(!is_tibetan_codepoint('म')); // Devanagari
        assert!(!is_tibetan_codepoint('a')); // ASCII
    }

    #[test]
    fn lao_khmer_myanmar_tibetan_fact_shaped_without_whitespace() {
        //  +
        // a declarative Khmer / Myanmar / Lao /
        // Tibetan sentence (no-inter-word-whitespace scripts
        // so the `contains(' ')` fast path does not fire)
        // must produce a Fact candidate via the codepoint-
        // count gate. Tibetan was added later
        // for parity with the BO_LEXICON shipped in this PR —
        // explicit-tag callers (FFI / connector pipelines
        // passing `bo`) MUST be able to round-trip a Tibetan
        // declarative through the fact extractor.
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();

        // "Phnom Penh is the capital of Cambodia." — Khmer, no
        // inter-word spaces, well over 4 Khmer codepoints.
        let obs_km = ext.extract("ភ្នំពេញគឺជារដ្ឋធានីនៃប្រទេសកម្ពុជា", scope);
        assert!(
            obs_km
                .iter()
                .any(|o| o.observation_type == ObservationType::Fact),
            "expected at least one Fact from Khmer declarative; got {:?}",
            obs_km
                .iter()
                .map(|o| o.observation_type)
                .collect::<Vec<_>>()
        );

        // "Yangon is the largest city in Myanmar." — Burmese, no
        // inter-word spaces, well over 4 Myanmar codepoints.
        let obs_my = ext.extract("ရန်ကုန်သည်မြန်မာနိုင်ငံ၏အကြီးဆုံးမြို့ဖြစ်သည်", scope);
        assert!(
            obs_my
                .iter()
                .any(|o| o.observation_type == ObservationType::Fact),
            "expected at least one Fact from Burmese declarative; got {:?}",
            obs_my
                .iter()
                .map(|o| o.observation_type)
                .collect::<Vec<_>>()
        );

        // "Vientiane is the capital of Laos." — Lao, no
        // inter-word spaces. whatlang refuses to classify it but
        // the codepoint-count gate must still admit it as a
        // Fact (the row will just carry `language_tag = None`).
        let obs_lo = ext.extract("ວຽງຈັນເປັນນະຄອນຫຼວງຂອງປະເທດລາວ", scope);
        assert!(
            obs_lo
                .iter()
                .any(|o| o.observation_type == ObservationType::Fact),
            "expected at least one Fact from Lao declarative on shape alone; got {:?}",
            obs_lo
                .iter()
                .map(|o| o.observation_type)
                .collect::<Vec<_>>()
        );

        // "Lhasa is the capital of Tibet." — Tibetan, no
        // ASCII spaces (tsheg `\u{0F0B}` is a syllable
        // separator, not a word boundary). whatlang 0.18
        // does not ship a Tibetan classifier so the row
        // carries `language_tag = None`, but the codepoint-
        // count gate MUST still admit it as a Fact — same
        // defense-in-depth as the Lao arm above. Without the
        // Tibetan arm added later this
        // assertion fails and no Tibetan declarative ever
        // becomes a Fact.
        let obs_bo = ext.extract("ལྷ་ས་ནི་བོད་ཀྱི་རྒྱལ་ས་ཡིན", scope);
        assert!(
            obs_bo
                .iter()
                .any(|o| o.observation_type == ObservationType::Fact),
            "expected at least one Fact from Tibetan declarative on shape alone; got {:?}",
            obs_bo
                .iter()
                .map(|o| o.observation_type)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn is_sentence_terminator_covers_initial_multilingual_set() {
        // Defensive: pin the exact terminator set so accidental
        // additions/removals fail tests instead of silently
        // changing behaviour. The scope of this test is
        // intentionally limited to the original multilingual
        // codepoints so that an accidental REMOVAL of any
        // terminator (regression of the original multilingual
        // terminator work) fires here with an unambiguous
        // error. Later additions have their own sibling pinning
        // test below (`is_sentence_terminator_covers_extended_set`).
        let terminators = [
            '.', '!', '?', '\n', '。', '！', '？', '؟', '۔', '।', '॥', '։', '።',
        ];
        for c in terminators {
            assert!(
                is_sentence_terminator(c),
                "{c:?} (U+{:04X}) must be a sentence terminator",
                c as u32
            );
        }
        let non_terminators = ['a', 'あ', ',', ';', '¿', '¡', ':', '\t'];
        for c in non_terminators {
            assert!(
                !is_sentence_terminator(c),
                "{c:?} (U+{:04X}) must NOT be a sentence terminator",
                c as u32
            );
        }
    }

    #[test]
    fn is_sentence_terminator_covers_extended_set() {
        // A sibling to the initial-set pinning test above.
        // Pins the four extended (Tibetan / Khmer / Myanmar /
        // Lao) sentence-final marks added later so an accidental
        // removal fires here independently of the initial set.
        //
        // The split between this and the initial-set test is
        // intentional: each test fails with a set-specific error
        // message, so a regression that drops (say) the Khmer khan
        // can be triaged to the commit line that introduced it
        // without first ruling out a regression in the initial set.
        let terminators = [
            ('\u{0F0D}', "Tibetan shad (sentence / clause end)"),
            ('\u{0F0E}', "Tibetan nyis shad (paragraph / verse end)"),
            ('\u{17D4}', "Khmer khan (full stop)"),
            ('\u{104B}', "Myanmar sign section / visarga (full stop)"),
        ];
        for (c, role) in terminators {
            assert!(
                is_sentence_terminator(c),
                "{c:?} (U+{:04X}, {role}) must be a sentence terminator",
                c as u32
            );
        }
        // Lao is intentionally absent: the Lao script (Unicode
        // block U+0E80..=U+0EFF) has no dedicated sentence-end
        // punctuation; modern Lao typography uses ASCII `.`,
        // `!`, `?` for sentence termination, which are already
        // in the set. The 3-new-terminators-vs-
        // 4-new-scripts asymmetry is documented by absence
        // here and at the doc-comment of `is_sentence_terminator`.
    }

    #[test]
    fn extract_with_dominant_language_hint_is_honoured_for_entity_class() {
        // Regression coverage: the pipeline + extractor used
        // to detect the dominant language twice on the same
        // text. The new `extract_with_dominant_language` hint
        // skips the extractor's whole-input detect_language when
        // the caller supplies a tag. Verify (a) the hinted tag
        // wins on entity-class observations, and (b) supplying a
        // contrived non-canonical tag changes the entity-class
        // language, proving the hint actually flows through.
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        let text = "Hello team please review the migration plan and ship Friday. \
                    @sara can you draft the rollout schedule by tomorrow?";
        // Hint a deliberately wrong tag so we can distinguish
        // "extractor honoured the hint" from "extractor
        // re-detected and got the same answer by coincidence".
        let hint = LanguageTag::new("xq").expect("contrived tag must construct");
        let obs = ext.extract_with_dominant_language(text, scope, Some(&hint));
        let mention = obs
            .iter()
            .find(|o| o.observation_type == ObservationType::Entity && o.content == "@sara")
            .expect("@sara mention should be extracted");
        assert_eq!(
            mention.language_tag.as_ref(),
            Some(&hint),
            "@mention must inherit the supplied dominant-language hint \
             instead of re-detecting (got {:?})",
            mention.language_tag
        );
    }

    #[test]
    fn extract_runs_whole_input_detection_once_at_call_site() {
        // Regression coverage: the legacy `extract()`
        // entry point is the only caller that has *not* already
        // run `detect_language` on the whole input, so it is
        // responsible for the single whole-input detection pass.
        // Verify the dominant language ends up stamped on
        // entity-class observations even though the caller never
        // supplied a hint \u2014 i.e. detection still happens, just
        // exactly once and at the public-API boundary instead of
        // inside the private `do_extract` helper.
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        let text = "@sara please draft the rollout schedule by Friday and ship the migration plan.";
        let obs = ext.extract(text, scope);
        let mention = obs
            .iter()
            .find(|o| o.observation_type == ObservationType::Entity && o.content == "@sara")
            .expect("@sara mention should be extracted");
        let en = LanguageTag::new("en").expect("en tag");
        assert_eq!(
            mention.language_tag.as_ref(),
            Some(&en),
            "extract() must run detect_language once at the public-API boundary \
             and stamp the detected tag on entity-class observations (got {:?})",
            mention.language_tag
        );
    }

    #[test]
    fn extract_with_dominant_language_treats_none_hint_as_authoritative() {
        // Regression coverage: callers that have already
        // attempted detection and got `None` (text not classifiable,
        // not reliable, too short) must be able to communicate that
        // to the extractor without the extractor redundantly
        // re-running `detect_language` on the same text. A `None`
        // hint to `extract_with_dominant_language` is authoritative:
        // entity-class observations get `language_tag = None`, not
        // a tag derived from a second detection pass.
        //
        // To prove the contract, use an input that *would* detect
        // to a known tag (so the test is sensitive to a regression
        // that reintroduces the fallback) and assert the absence
        // of that tag on the resulting entity-class observations.
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        // English text long enough that `detect_language` is
        // reliable on it: if the extractor re-ran detection it
        // would re-derive `en`.
        let text = "@sara please draft the rollout schedule by Friday and ship the migration plan.";
        let pre_detect = detect_language(text).map(|d| d.tag);
        assert!(
            pre_detect.is_some(),
            "test premise: detect_language must succeed on this input so the \
             regression test is sensitive to a future re-introduction of the \
             fallback detection inside do_extract"
        );

        let obs = ext.extract_with_dominant_language(text, scope, None);
        let mention = obs
            .iter()
            .find(|o| o.observation_type == ObservationType::Entity && o.content == "@sara")
            .expect("@sara mention should be extracted");
        assert!(
            mention.language_tag.is_none(),
            "@mention must inherit the authoritative None hint, NOT re-detect \
             the dominant language inside do_extract (got {:?})",
            mention.language_tag
        );
    }

    // ====================================================================
    // per-language registry-backed sentence classification
    // ====================================================================

    /// Helper: find observations of a given type.
    fn find_obs_by_type(obs: &[Observation], t: ObservationType) -> Vec<&Observation> {
        obs.iter().filter(|o| o.observation_type == t).collect()
    }

    #[test]
    fn french_decision_keyword_matches_through_registry() {
        // French sentence with `approuvé` (past
        // participle of `approuver` — "approve") must route to
        // Decision via the FR lexicon. earlier this was
        // never matched because the inline English lexicon
        // didn't carry `approuvé`.
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        let text = "Le plan de lancement a été approuvé par l'équipe lors de la réunion d'hier.";
        let obs = ext.extract(text, scope);
        let decisions = find_obs_by_type(&obs, ObservationType::Decision);
        assert!(
            !decisions.is_empty(),
            "expected a Decision from French `approuvé`; got {:?}",
            obs.iter()
                .map(|o| (o.observation_type, o.content.clone()))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            decisions[0]
                .language_tag
                .as_ref()
                .map(|t| t.primary().to_string()),
            Some("fr".to_string()),
            "decision tag must be `fr` (got {:?})",
            decisions[0].language_tag
        );
    }

    #[test]
    fn spanish_task_imperative_matches_through_registry() {
        // Spanish sentence opening with the
        // 2nd-person-singular imperative `envía` ("send") must
        // route to Task via FirstBigram-strategy lookup against
        // the ES imperative-verb table. Inline English
        // imperatives (draft / send / …) would not have caught
        // `envía`.
        //
        // We supply an explicit `es` dominant hint via
        // [`ObservationExtractor::extract_with_dominant_language`]
        // so the test isolates the *registry-matching* behaviour
        // from whatlang's per-sentence-reliability heuristic
        // (whatlang often returns `None` for individual short
        // sentences — we already cover the per-sentence-detection
        // path in the bilingual pipeline test). When the per-
        // sentence detection on this single sentence returns
        // `None`, the extractor falls back to the dominant hint,
        // which here pins the ES lexicon.
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        let es = LanguageTag::new("es").unwrap();
        let text = "Envía el informe de migración al equipo de operaciones antes del viernes.";
        let obs = ext.extract_with_dominant_language(text, scope, Some(&es));
        let tasks = find_obs_by_type(&obs, ObservationType::Task);
        assert!(
            !tasks.is_empty(),
            "expected a Task from Spanish imperative `envía`; got {:?}",
            obs.iter()
                .map(|o| (o.observation_type, o.content.clone()))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            tasks[0]
                .language_tag
                .as_ref()
                .map(|t| t.primary().to_string()),
            Some("es".to_string())
        );
    }

    #[test]
    fn vietnamese_task_imperative_matches_through_first_bigram() {
        // Earlier reviews flagged that multi-word collocations
        // need FirstBigram-strategy matching. Vietnamese
        // `triển khai` ("deploy", "roll out") is a single
        // semantic verb spelt as two tokens — FirstToken would
        // miss it. FirstBigram tries first-token first
        // (so single-word `chốt`-style verbs still match) then
        // joins the first two tokens with a single ASCII space
        // and retries.
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        let text = "Triển khai bản kế hoạch di trú vào sáng thứ Sáu này nhé.";
        let obs = ext.extract(text, scope);
        let tasks = find_obs_by_type(&obs, ObservationType::Task);
        assert!(
            !tasks.is_empty(),
            "expected a Task from Vietnamese `triển khai`; got {:?}",
            obs.iter()
                .map(|o| (o.observation_type, o.content.clone()))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            tasks[0]
                .language_tag
                .as_ref()
                .map(|t| t.primary().to_string()),
            Some("vi".to_string())
        );
    }

    #[test]
    fn arabic_decision_keyword_matches_after_tashkeel_strip() {
        // Closes: Arabic combining
        // marks (tashkeel) like fatha / kasra
        // would otherwise split the FirstToken matcher's view
        // of the word boundary because the marks are category
        // Mn (non-alphabetic). The
        // [`normalize_for_lookup`] primitive — called once per
        // sentence by the extractor — strips tashkeel and
        // tatweel when the detected primary tag is `ar` before
        // matching runs, so a fully-vocalised input matches the
        // unvocalised lexicon entry.
        //
        // Whatlang frequently returns `None` on short fully-
        // vocalised Arabic sentences (the diacritics break its
        // trigram model), so this test supplies an explicit
        // `ar` dominant hint via
        // [`ObservationExtractor::extract_with_dominant_language`]
        // to isolate the tashkeel-strip + lexicon-matching
        // behaviour from whatlang's per-sentence-reliability
        // heuristic. The bilingual pipeline test
        // already exercises end-to-end whatlang detection.
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        let ar = LanguageTag::new("ar").unwrap();
        // "We decided the plan in the meeting." — `قررنا`
        // ("we decided") is an AR lexicon decision-class
        // lemma. The input here adds a tashkeel mark (fatha)
        // and the tatweel elongation character to the verb to
        // test the tashkeel-strip path (without the strip,
        // Substring matching on the raw input would not find
        // the unvocalised lexicon entry `قررنا`).
        let text = "قَررـنا الخطة في الاجتماع.";
        let obs = ext.extract_with_dominant_language(text, scope, Some(&ar));
        let decisions = find_obs_by_type(&obs, ObservationType::Decision);
        assert!(
            !decisions.is_empty(),
            "expected a Decision from Arabic `قررنا` (tashkeel+tatweel-stripped); got {:?}",
            obs.iter()
                .map(|o| (o.observation_type, o.content.clone()))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            decisions[0]
                .language_tag
                .as_ref()
                .map(|t| t.primary().to_string()),
            Some("ar".to_string())
        );
    }

    #[test]
    fn japanese_task_keyword_matches_via_substring_strategy() {
        // CJK lexicons use Substring strategy
        // because there is no inter-word whitespace to split
        // into tokens. Japanese `お願い` ("please" / polite
        // request opener) must match anywhere in the sentence.
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        // Pure Japanese declarative ending in `。` (no
        // interrogative terminator) carrying `お願い`. Pre-
        // earlier this would have fallen through to Fact
        // because the inline English keyword set never matched
        // `お願い`.
        let text = "明日の朝までに移行プランをレビューしてくださいお願いします。";
        let obs = ext.extract(text, scope);
        let tasks = find_obs_by_type(&obs, ObservationType::Task);
        assert!(
            !tasks.is_empty(),
            "expected a Task from Japanese `お願い`; got {:?}",
            obs.iter()
                .map(|o| (o.observation_type, o.content.clone()))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            tasks[0]
                .language_tag
                .as_ref()
                .map(|t| t.primary().to_string()),
            Some("ja".to_string())
        );
    }

    #[test]
    fn unsupported_language_falls_back_to_english_lexicon() {
        // a primary subtag NOT in
        // [`BUILTIN_LEXICONS`] must transparently fall back to
        // the English lexicon (so English keywords still work
        // in an `xx`-tagged or unconfigured-language sentence,
        // and the fallback is silent — no panic, no error
        // observation).
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        let xx = LanguageTag::new("xx").unwrap();
        // English content; supply `xx` as the dominant hint to
        // simulate a detector that produced an unsupported
        // BCP-47 tag. The whole-sentence detection on this
        // ASCII English sentence will also return `en`, but
        // since per-sentence detection runs first inside the
        // extractor it will override the dominant hint — to
        // really exercise the fallback we need an input that
        // detects as `xx` per-sentence too, which whatlang
        // can't produce. So we settle for asserting the no-
        // panic path under an `xx` dominant hint and that
        // English content STILL extracts (proving the per-
        // sentence detection works orthogonally to the hint).
        let text = "Please review the migration plan and ship by Friday.";
        let obs = ext.extract_with_dominant_language(text, scope, Some(&xx));
        let tasks = find_obs_by_type(&obs, ObservationType::Task);
        assert!(
            !tasks.is_empty(),
            "english `please` must still trigger Task even with `xx` dominant hint; got {:?}",
            obs.iter()
                .map(|o| (o.observation_type, o.content.clone()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn legacy_inline_constructor_preserves_pre_behaviour() {
        // Back-compat: callers of [`LexiconExtractor::new`]
        // (earlier single-language inline keyword
        // overrides) get exactly the earlier
        // substring-match semantics regardless of detected
        // language. This pins the API contract so future
        // refactors don't silently drop the inline path.
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::new(
            vec!["wir haben entschieden"], // German decision phrase
            vec!["bitte"],                 // German task opener
            vec!["beschicke"],             // German imperative
            vec!["The", "Today"],
        );
        let text = "Wir haben entschieden, das Migrationsdokument zu veröffentlichen.";
        let obs = ext.extract(text, scope);
        let decisions = find_obs_by_type(&obs, ObservationType::Decision);
        assert!(
            !decisions.is_empty(),
            "inline German decision phrase must still match through legacy constructor; got {:?}",
            obs.iter()
                .map(|o| (o.observation_type, o.content.clone()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn class_precedence_question_beats_task_keyword() {
        // ordering invariant: question detection
        // (sentence terminator + per-language interrogative
        // table) runs BEFORE task-keyword detection so that a
        // sentence that ends in `？` or contains an
        // interrogative is classified as Question even when it
        // also contains a polite-request opener (`お願い`,
        // `por favor`, `vui lòng`) that would otherwise route
        // to Task.
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        // Japanese sentence with both `お願い` (task keyword)
        // AND interrogative semantics (`何時に`, `か`
        // terminator).
        let text = "今日の会議では何時に開始する予定でしょうかご確認お願いします。";
        let obs = ext.extract(text, scope);
        let questions = find_obs_by_type(&obs, ObservationType::Question);
        assert!(
            !questions.is_empty(),
            "Japanese interrogative containing polite-request opener must be a Question \
             (question-first precedence); got {:?}",
            obs.iter()
                .map(|o| (o.observation_type, o.content.clone()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn capitalised_extractor_skips_non_ascii_stop_words_unicode_lowercase() {
        // collocation closure: the earlier path
        // compared capitalised tokens against stop-words via
        // `str::eq_ignore_ascii_case`, which only folds the
        // ASCII A–Z / a–z range. Non-ASCII stop-words (Cyrillic
        // Russian Это / это; Vietnamese with prefixed Đ / đ)
        // therefore silently passed through as entity
        // candidates. The fix routes the check through
        // `LexiconExtractor::is_stop_word`, which lowercases the
        // candidate with the Unicode-aware `str::to_lowercase`
        // fold before comparing.
        //
        // Russian regression: the Russian lexicon's stop-word
        // table includes `это` (lower-case). When the candidate
        // is the capitalised opener `Это`, the predicate must
        // return true — otherwise `Это` is mis-emitted as an
        // entity observation.
        let ru = LanguageTag::new("ru").unwrap();
        let ext = LexiconExtractor::default();
        assert!(
            ext.is_stop_word("Это", Some(&ru)),
            "Cyrillic capitalised `Это` must match lexicon stop-word `это` under \
             Unicode lowercase folding "
        );
        assert!(
            !ext.is_stop_word("Москва", Some(&ru)),
            "Real Russian entity `Москва` must not match any stop-word"
        );

        // Vietnamese regression: `đó` is in the Vietnamese
        // stop-word table; the capitalised opener `Đó` must
        // fold to `đó` (Vietnamese-specific `Đ` → `đ` is a
        // standard Unicode lowercase mapping, not ASCII).
        let vi = LanguageTag::new("vi").unwrap();
        assert!(
            ext.is_stop_word("Đó", Some(&vi)),
            "Vietnamese capitalised `Đó` must match lexicon stop-word `đó` under \
             Unicode lowercase folding"
        );
        assert!(
            !ext.is_stop_word("Hà", Some(&vi)),
            "Real Vietnamese entity fragment `Hà` must not match any stop-word"
        );

        // English baseline regression: the ASCII case fold must
        // still work (regression guard against the new predicate
        // accidentally dropping ASCII coverage).
        let en = LanguageTag::new("en").unwrap();
        assert!(
            ext.is_stop_word("The", Some(&en)),
            "English capitalised `The` must continue to match stop-word `the`"
        );
    }

    #[test]
    fn capitalised_extractor_drops_cyrillic_function_word_entity_e2e() {
        // End-to-end version of the earlier regression:
        // without the fix, a Russian sentence whose first word
        // is the demonstrative `Это` produced an `Entity`
        // observation for `Это` because the ASCII case fold
        // failed. With the fix it should not.
        let scope = ScopeId::new_v4();
        let ext = LexiconExtractor::default();
        let ru = LanguageTag::new("ru").unwrap();
        let text = "Это просто текст для теста.";
        let obs = ext.extract_with_dominant_language(text, scope, Some(&ru));
        let entities = find_obs_by_type(&obs, ObservationType::Entity);
        assert!(
            !entities.iter().any(|o| o.content == "Это"),
            "Russian function word `Это` must not surface as an Entity observation \
             ; got entities {:?}",
            entities.iter().map(|o| &o.content).collect::<Vec<_>>()
        );
    }

    #[test]
    fn looks_like_question_strips_arabic_tashkeel_via_normalize_for_lookup() {
        // collocation closure: the question detector
        // used to apply its own ad-hoc NFC + lowercase pass that
        // did NOT strip Arabic tashkeel + tatweel, so an Arabic
        // interrogative decorated with vowel marks (`كَيْفَ`) did
        // not match the canonical table entry (`كيف`) under the
        // FirstToken matcher (the tokeniser splits on the
        // tashkeel codepoints, which are category Mn). Routing
        // the question path through `normalize_for_lookup` now
        // strips the tashkeel before tokenisation, matching how
        // the decision / task / imperative paths already
        // worked.
        let ar = LanguageTag::new("ar").unwrap();
        // `كَيْفَ` = `كيف` ("how") with three tashkeel marks
        // (fatha + sukun + fatha). Without tashkeel strip the
        // FirstToken tokeniser would split this into pieces that
        // never match the bare `كيف` entry.
        let with_tashkeel = "كَيْفَ يمكنني المساعدة";
        let without_tashkeel = "كيف يمكنني المساعدة";
        assert!(
            looks_like_question(without_tashkeel, Some('.'), Some(&ar)),
            "Bare Arabic `كيف` interrogative must classify as a question"
        );
        assert!(
            looks_like_question(with_tashkeel, Some('.'), Some(&ar)),
            "Tashkeel-decorated Arabic `كَيْفَ` must classify as a question \
             after normalize_for_lookup strips the tashkeel \
             "
        );
    }

    #[test]
    fn looks_like_question_recovers_vietnamese_bigram_interrogatives() {
        // collocation closure: Vietnamese now uses
        // `InterrogativeMatch::FirstBigram` so the high-frequency
        // bare prepositions / conjunctions `tại` / `khi` / `vì`
        // recover their interrogative readings via the two-token
        // collocations `tại sao` / `khi nào` / `vì sao` without
        // re-introducing the false positives the bare forms
        // caused. Bare `Khi tôi đến...` must still NOT classify.
        let vi = LanguageTag::new("vi").unwrap();
        // Bigram interrogatives — must classify even without a
        // `?` terminator (otherwise the matcher path is never
        // exercised, since the terminator short-circuits first).
        for question in ["tại sao bạn buồn", "khi nào chúng ta đi", "vì sao trời mưa"]
        {
            assert!(
                looks_like_question(question, Some('.'), Some(&vi)),
                "Vietnamese bigram interrogative {question:?} must classify as a question \
                 via FirstBigram "
            );
        }
        // Bare forms must still NOT classify (the false-positive
        // guard that motivated the deferred bigram approach).
        for declarative in [
            "khi tôi đến nhà của bạn",
            "tại Hà Nội mọi thứ rất khác",
            "vì tôi bận nên không thể đến",
        ] {
            assert!(
                !looks_like_question(declarative, Some('.'), Some(&vi)),
                "Vietnamese declarative {declarative:?} starting with bare function word must \
                 NOT classify as a question (regression guard against re-adding bare forms)"
            );
        }
        // Bare unambiguous interrogatives must still classify via
        // the FirstToken arm of FirstBigram.
        assert!(
            looks_like_question("ai là người đó", Some('.'), Some(&vi)),
            "Vietnamese bare interrogative `ai` must still classify via FirstBigram's \
             first-token arm"
        );
    }

    #[test]
    fn hindi_devanagari_virama_imperatives_match_via_substring() {
        // Earlier review: Hindi `task_imperative_verbs`
        // containing the Devanagari
        // virama `U+094D` (Category Mn) — `मर्ज` (merge),
        // `समीक्षा` (review), `प्रकाशित` (publish), `अद्यतन`
        // (update) — are unreachable under the FirstBigram
        // matcher because `alphabetic_tokens` splits at every
        // non-alphabetic character (the virama qualifies). The
        // structural fix promotes `task_imperative_strategy` to a
        // per-language [`LanguageLexicon`] field and sets it to
        // [`MatchStrategy::Substring`] for Hindi specifically,
        // matching how `decision_strategy` and `task_strategy`
        // are already overridden per-language. This test exercises
        // each affected verb through the full pipeline-shaped
        // [`LexiconExtractor`] surface so a future regression in
        // either the field plumbing or the strategy choice will
        // produce zero Task observations for these sentences.
        let extractor = LexiconExtractor::default();
        let hi = LanguageTag::new("hi").unwrap();
        let scope = ScopeId::new_v4();

        // Each sentence ends with a Devanagari purna virama `।`
        // so the multilingual sentence splitter sees a single
        // sentence per input, and Hindi imperatives sit
        // mid-sentence (where FirstToken / FirstBigram would
        // still fail even without the virama issue, so this
        // also tests that Substring catches non-leading
        // positions).
        let cases = [
            ("कृपया इस PR की समीक्षा करें।", "समीक्षा (review)"),
            ("इस ब्रांच को मर्ज करें।", "मर्ज (merge)"),
            ("रिपोर्ट प्रकाशित करें।", "प्रकाशित (publish)"),
            ("दस्तावेज़ अद्यतन करें।", "अद्यतन (update)"),
        ];
        for (sentence, label) in cases {
            let obs = extractor.extract_with_dominant_language(sentence, scope, Some(&hi));
            assert!(
                obs.iter()
                    .any(|o| matches!(o.observation_type, ObservationType::Task)),
                "Hindi imperative containing virama {label:?} must produce a Task \
                 observation under MatchStrategy::Substring \
                 "
            );
        }
    }

    #[test]
    fn french_aujourdhui_with_typographic_apostrophe_is_recognised_as_stop_word() {
        // collocation closure. The French
        // stop-word `aujourd'hui` is stored in `FR_LEXICON` with
        // ASCII apostrophe `U+0027`, but most French IMEs
        // (macOS smart-quotes, iOS, Word) emit `U+2019` RIGHT
        // SINGLE QUOTATION MARK by default. Before the fix the
        // capitalised-token splitter only treated ASCII `'` as
        // in-token, so `Aujourd\u{2019}hui` tokenised as
        // `["Aujourd", "hui"]` and `Aujourd` (no match against
        // `aujourd'hui`) was emitted as an entity. The fix folds
        // the three apostrophe variants (U+2019, U+2018, U+02BC)
        // to ASCII `'` before tokenisation and entity extraction.
        let extractor = LexiconExtractor::default();
        let fr = LanguageTag::new("fr").unwrap();
        let scope = ScopeId::new_v4();

        // Both inputs must end up extracting `Paris` and NOT
        // extracting `Aujourd` / `aujourd'hui` / any apostrophe
        // fragment as an entity.
        let ascii = "Aujourd'hui Paris est ensoleillé.";
        let typographic = "Aujourd\u{2019}hui Paris est ensoleillé.";

        let entities_ascii: Vec<String> = extractor
            .extract_with_dominant_language(ascii, scope, Some(&fr))
            .into_iter()
            .filter(|o| matches!(o.observation_type, ObservationType::Entity))
            .map(|o| o.content)
            .collect();
        let entities_typographic: Vec<String> = extractor
            .extract_with_dominant_language(typographic, scope, Some(&fr))
            .into_iter()
            .filter(|o| matches!(o.observation_type, ObservationType::Entity))
            .map(|o| o.content)
            .collect();

        assert_eq!(
            entities_ascii, entities_typographic,
            "French Aujourd\u{2019}hui (typographic U+2019) must produce the same \
             entity set as Aujourd'hui (ASCII U+0027) after typographic-apostrophe \
             folding in extract_capitalised_words \
             (per a follow-up review)"
        );
        assert!(
            entities_typographic.iter().any(|e| e == "Paris"),
            "Paris must be emitted as an entity from the typographic-apostrophe input"
        );
        assert!(
            !entities_typographic
                .iter()
                .any(|e| e.starts_with("Aujourd")),
            "No Aujourd-prefixed fragment may be emitted as an entity — the stop-word \
             check must converge for both apostrophe shapes (entities: {:?})",
            entities_typographic
        );
    }

    #[test]
    fn no_stop_word_entry_contains_typographic_apostrophe() {
        // Cross-language invariant pinning the contract that
        // every stop-word entry in every registry lexicon uses
        // ASCII U+0027 (and never U+2019 / U+2018 / U+02BC).
        // The capitalised-token splitter folds typographic
        // apostrophes in the INPUT to ASCII before lookup; the
        // lookup table itself must mirror that canonical form
        // or the fold-then-compare path would silently miss.
        // See `extract_capitalised_words` doc and the matching
        // earlier-review threads for the case-folding contract.
        for lexicon in default_registry().iter() {
            for entry in lexicon.stop_words {
                for c in entry.chars() {
                    assert!(
                        !matches!(c, '\u{2019}' | '\u{2018}' | '\u{02BC}'),
                        "Stop-word entry {:?} in lexicon {:?} contains a non-ASCII \
                         apostrophe variant U+{:04X}. Stop-word entries must use ASCII \
                         apostrophe `'` (U+0027) so the fold-then-compare path in \
                         extract_capitalised_words converges. Replace it with U+0027.",
                        entry,
                        lexicon.primary_tag,
                        c as u32
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------
    // Arabic proclitic-aware classification, end-to-end.
    // -----------------------------------------------------------------

    #[test]
    fn arabic_proclitic_prefixed_interrogatives_classify_as_questions() {
        // entry point: the productive Arabic proclitic
        // prefix forms of canonical interrogatives must classify
        // as questions through the LexiconExtractor's
        // `looks_like_question` path, including when the
        // terminator is ASCII `.` (so the `؟` short-circuit does
        // not apply and the test exercises the actual lookup).
        //
        // Each case is a Modern Standard Arabic sentence drawn
        // from the productive proclitic stack documented in
        // Ryding §10.1 ("Proclitics"):
        //
        // * `و` ("and") + `كيف` ("how") = `وكيف`
        // * `ف` ("then") + `متى` ("when") = `فمتى`
        // * `ب` ("with/by") + `أي` ("which") = `بأي`
        // * `ل` ("to/for") + `من` ("who") = `لمن`
        let ar = LanguageTag::new("ar").unwrap();
        let cases = [
            ("وكيف يمكنني المساعدة", "و+كيف"),
            ("فمتى نلتقي", "ف+متى"),
            ("بأي طريقة نفعل ذلك", "ب+أي"),
            ("لمن هذا الكتاب", "ل+من"),
        ];
        for (sentence, label) in cases {
            assert!(
                looks_like_question(sentence, Some('.'), Some(&ar)),
                "proclitic-prefixed Arabic interrogative {label:?} in \
                 sentence {sentence:?} must classify as a question via the \
                 FirstTokenWithArabicClitics matcher even with `.` terminator (the \
                 `؟` short-circuit is bypassed in this test on purpose)"
            );
        }
    }

    #[test]
    fn arabic_proclitic_prefixed_imperatives_emit_task_observations() {
        // entry point: the productive Arabic proclitic
        // prefix forms of canonical imperative verbs must emit
        // Task observations through the LexiconExtractor when
        // routed via the FirstTokenWithArabicClitics matcher.
        // Each sentence chains the imperative behind a proclitic
        // (`و` / `ف`), which is the realistic multi-clause Arabic
        // task directive pattern that earlier FirstBigram
        // missed.
        let extractor = LexiconExtractor::default();
        let ar = LanguageTag::new("ar").unwrap();
        let scope = ScopeId::new_v4();
        let cases = [
            ("واكتب التقرير غدا", "و+اكتب (write the report tomorrow)"),
            ("وأرسل البريد الآن", "و+أرسل (send the email now)"),
            (
                "فجدول الاجتماع الأسبوع القادم",
                "ف+جدول (schedule the meeting next week)",
            ),
            (
                "وراجع الخطة قبل الاجتماع",
                "و+راجع (review the plan before the meeting)",
            ),
        ];
        for (sentence, label) in cases {
            let obs = extractor.extract_with_dominant_language(sentence, scope, Some(&ar));
            assert!(
                obs.iter()
                    .any(|o| matches!(o.observation_type, ObservationType::Task)),
                "proclitic-prefixed Arabic imperative {label:?} in sentence \
                 {sentence:?} must produce a Task observation via the \
                 FirstTokenWithArabicClitics matcher"
            );
        }
    }

    #[test]
    fn arabic_declaratives_do_not_falsely_classify_as_questions() {
        // false-positive guard: a declarative whose
        // first token starts with `أ` (interrogative-hamza
        // orthography) must NOT classify as a question, because
        // `أ` is deliberately omitted from the peel set —
        // peeling would over-classify the large open class of
        // `أ`-initial nouns / pronouns / proper names.
        //
        // Each case is an Arabic declarative with a leading
        // `أ`-word that earlier was correctly NOT detected
        // (no proclitic stripping happened); must
        // preserve that correctness.
        let ar = LanguageTag::new("ar").unwrap();
        let declaratives = [
            "أنا في المكتب اليوم", // "I am in the office today" — pronoun `أنا`.
            "أحمد قادم غدا",       // "Ahmad is coming tomorrow" — proper-name `أحمد`.
            "أمي في المنزل",       // "My mother is at home" — `أمي` (my mother).
            "أبي يعمل في الشركة",  // "My father works at the company" — `أبي` (my father).
        ];
        for sentence in declaratives {
            assert!(
                !looks_like_question(sentence, Some('.'), Some(&ar)),
                "Arabic declarative {sentence:?} starting with `أ`-prefixed \
                 word must NOT classify as a question — `أ` is deliberately omitted \
                 from the proclitic peel set to avoid over-classifying the open class \
                 of `أ`-initial nouns/pronouns/proper-names"
            );
        }
    }

    #[test]
    fn arabic_definite_article_in_declarative_does_not_emit_task() {
        // false-positive guard for the imperative
        // path: a noun starting with the definite article `ال`
        // must NOT trigger a Task observation just because the
        // peel surfaces a substring that happens to share
        // letters with an imperative verb.
        //
        // Worked example: `الكتاب على المنضدة` ("The book is on
        // the table") peels `ال` from `الكتاب` to leave
        // `كتاب`. None of the AR_LEXICON imperative entries are
        // `كتاب` (they are `اكتب` / `أرسل` / `جدول` / `راجع` /
        // `انشر` / `أصلح` / `وزع` / `تحقق` / `حضر` / `حدث` /
        // `ادمج`), so the peel-then-compare path must yield no
        // Task observation. This pins the precision contract:
        // peeling produces a residual to test for exact
        // equality, NOT a substring-match license.
        let extractor = LexiconExtractor::default();
        let ar = LanguageTag::new("ar").unwrap();
        let scope = ScopeId::new_v4();
        let declaratives = [
            "الكتاب على المنضدة",
            "الاجتماع في الساعة الثالثة",
            "البيت كبير وجميل",
        ];
        for sentence in declaratives {
            let obs = extractor.extract_with_dominant_language(sentence, scope, Some(&ar));
            assert!(
                !obs.iter()
                    .any(|o| matches!(o.observation_type, ObservationType::Task)),
                "Arabic declarative {sentence:?} starting with `ال` must NOT \
                 emit a Task observation — peeling `ال` produces a noun residual that is \
                 not in the imperative table, so exact-equality must hold and the false \
                 positive must not surface"
            );
        }
    }

    #[test]
    fn arabic_proclitic_stack_resolves_through_two_peels() {
        // 2-peel realistic stack. `فلكتاب` (`ف` +
        // `ل` + `كتاب`) appears in formal Arabic prose meaning
        // "then for-book" / "so as-for-the-book". The Task
        // path does NOT trigger here (no `كتاب` in the
        // imperative table), and the Question path does not
        // trigger either (no `كتاب` in the interrogative
        // table). What this test pins is the architectural
        // contract that the peeler can in fact iterate 2 peels
        // without false-positive bleed into either class — a
        // sanity check that's tedious but high-signal because
        // a regression would surface as either a spurious Task
        // or a spurious Question for a declarative noun phrase.
        //
        // The companion `lexicon::tests::table_matches_
        // arabic_clitic_strip_iterates_stacked_prefixes` test
        // already pins the positive iteration semantics with
        // a synthetic table containing `كتاب`; this test pins
        // the production-table negative semantics.
        let extractor = LexiconExtractor::default();
        let ar = LanguageTag::new("ar").unwrap();
        let scope = ScopeId::new_v4();
        let sentence = "فلكتاب أهمية كبيرة";
        let obs = extractor.extract_with_dominant_language(sentence, scope, Some(&ar));
        assert!(
            !obs.iter()
                .any(|o| matches!(o.observation_type, ObservationType::Task)),
            "2-peel-deep stack `فلكتاب` must not trigger Task on a noun residual"
        );
        assert!(
            !looks_like_question(sentence, Some('.'), Some(&ar)),
            "2-peel-deep stack `فلكتاب` on a declarative noun phrase must not \
             trigger Question on a noun residual"
        );
    }

    #[test]
    fn arabic_clitic_aware_strategy_preserves_tashkeel_path() {
        // cross-feature interaction: the
        // FirstTokenWithArabicClitics matcher must compose
        // correctly with the tashkeel-strip normalisation path
        // . A
        // tashkeel-decorated proclitic-prefixed interrogative
        // (`وَكَيْفَ` = `و` + tashkeel-decorated `كيف`) must
        // classify as a question because (a) `normalize_for_lookup`
        // strips the tashkeel first, leaving `وكيف`, then (b)
        // the proclitic-aware matcher peels `و` to reveal `كيف`.
        let ar = LanguageTag::new("ar").unwrap();
        let with_tashkeel = "وَكَيْفَ يمكنني المساعدة";
        let without_tashkeel = "وكيف يمكنني المساعدة";
        assert!(
            looks_like_question(without_tashkeel, Some('.'), Some(&ar)),
            "bare proclitic-prefixed `وكيف` must classify as a question"
        );
        assert!(
            looks_like_question(with_tashkeel, Some('.'), Some(&ar)),
            "tashkeel-decorated proclitic-prefixed `وَكَيْفَ` must compose \
             tashkeel-strip + proclitic-peel correctly and classify as a question"
        );
    }

    #[test]
    fn arabic_first_person_future_does_not_emit_task() {
        // Precision guard, end-to-end:
        // 1st-person future-tense declaratives that share a
        // verb root with an `أ`-initial
        // imperative must NOT emit a Task observation. The future
        // marker `س` is deliberately omitted from the proclitic
        // peel set precisely because peeling `سأرسل` ("I will
        // send") to surface `أرسل` (which IS in the imperative
        // table) would produce a phantom Task observation on
        // every Arabic future-tense statement of intent.
        //
        // The companion `lexicon::tests::table_matches_arabic_
        // clitic_strip_drops_unproductive_k_and_s_prefixes` test
        // pins this at the matcher boundary; this test pins it
        // at the end-to-end integration boundary so an accidental
        // re-addition of `س` to the peel set would surface as a
        // failing test in BOTH layers.
        let extractor = LexiconExtractor::default();
        let ar = LanguageTag::new("ar").unwrap();
        let scope = ScopeId::new_v4();
        let declaratives = [
            "سأرسل البريد غدا",            // "I will send the email tomorrow".
            "سأكتب التقرير الأسبوع القادم", // "I will write the report next week".
            "سأصلح الخلل قريبا",           // "I will fix the bug soon".
        ];
        for sentence in declaratives {
            let obs = extractor.extract_with_dominant_language(sentence, scope, Some(&ar));
            assert!(
                !obs.iter()
                    .any(|o| matches!(o.observation_type, ObservationType::Task)),
                "1st-person future declarative {sentence:?} \
                 must NOT emit a Task observation — `س` is deliberately omitted from the \
                 peel set so the future marker cannot conflate with the imperative table"
            );
        }
    }
}
