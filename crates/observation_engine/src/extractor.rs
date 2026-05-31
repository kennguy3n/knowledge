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
//! ## Phase 1.4 — multilingual sentence + question handling
//!
//! [`split_sentences_with_terminator`] recognises CJK
//! (`。！？`), Arabic (`؟ ۔`), Devanagari (`।`), Armenian (`։`),
//! Ethiopic (`።`) sentence terminators alongside ASCII
//! (`. ! ? \n`). [`looks_like_question`] consults the per-language
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

use evidence_store::ScopeId;
use unicode_normalization::UnicodeNormalization;

use crate::interrogatives::{interrogatives_for, InterrogativeMatch};
use crate::language::{detect_language, LanguageTag};
use crate::types::{Observation, ObservationType};

/// Extract structured observations from raw evidence text.
///
/// # Language stamping contract (Phase 1.4)
///
/// Implementations of `extract` are responsible for stamping each
/// returned [`Observation`]'s `language_tag` field. Phase 1.4 of
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
/// fail-closed on language-dependent operations (Phase 1.1
/// `LexiconRegistry` lookup, Phase 1.2 FTS5 tokenizer selection).
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
///     }                                                          // → calls self.extract() → loop
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
#[derive(Debug, Clone)]
pub struct LexiconExtractor {
    decision_keywords: Vec<String>,
    task_keywords: Vec<String>,
    task_imperative_verbs: Vec<String>,
    /// English stop-words to strip from capitalised-token entity
    /// extraction (so we don't promote "The" / "This" / "Today" to
    /// entities).
    stop_words: Vec<String>,
}

impl Default for LexiconExtractor {
    fn default() -> Self {
        Self::english_default()
    }
}

impl LexiconExtractor {
    /// Build with explicit lexicons.
    pub fn new(
        decision_keywords: Vec<&str>,
        task_keywords: Vec<&str>,
        task_imperative_verbs: Vec<&str>,
        stop_words: Vec<&str>,
    ) -> Self {
        Self {
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
        }
    }

    /// Default English lexicon. Production deployments should
    /// override per-tenant.
    pub fn english_default() -> Self {
        Self::new(
            vec![
                "decided",
                "decision",
                "agreed",
                "approved",
                "ratified",
                "signed off",
                "sign-off",
                "go-live approved",
                "rejected",
            ],
            // Multi-word entries belong here (lower_contains_any does
            // substring matching) — `starts_with_imperative` only
            // compares the first alphabetic-only token of a sentence
            // against the imperative-verb list, so multi-word verbs
            // would otherwise be unreachable.
            vec![
                "todo",
                "action",
                "task",
                "please",
                "fyi action",
                "follow up",
                "follow-up",
            ],
            vec![
                "draft",
                "send",
                "schedule",
                "review",
                "publish",
                "fix",
                "deploy",
                "ship",
                "investigate",
                "prepare",
                "update",
                "merge",
            ],
            vec![
                "The",
                "This",
                "That",
                "These",
                "Those",
                "It",
                "Today",
                "Tomorrow",
                "Yesterday",
                "Friday",
                "Monday",
                "Tuesday",
                "Wednesday",
                "Thursday",
                "Saturday",
                "Sunday",
                "May",
                "June",
                "July",
            ],
        )
    }
}

/// One sentence and the punctuation `char` that ended it.
///
/// The terminator is `None` for the trailing fragment of
/// unterminated input. Stored as a `char` (not a `u8`) because
/// Phase 1.4 supports multi-byte UTF-8 terminators — CJK `。`,
/// Arabic `؟`, Devanagari `।`, etc. — that don't fit in a single
/// byte.
#[derive(Debug, Clone, Copy)]
struct SentenceSlice<'a> {
    text: &'a str,
    terminator: Option<char>,
}

/// All sentence-terminator code points the splitter recognises.
///
/// Coverage rationale (per Phase 1.4 of the multilingual
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

/// Phase 1.4 question detector — consults the per-language
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
///    sentence's primary BCP-47 subtag. CJK + Thai use
///    [`InterrogativeMatch::Substring`] (the interrogative may
///    appear anywhere in the sentence); space-separated
///    languages use [`InterrogativeMatch::FirstToken`].
/// 3. **Fallback** — when the language tag is `None` or no
///    table is configured for the language, fall back to the
///    English first-token check so substantive English
///    questions in unknown-language threads still get caught.
fn looks_like_question(
    sentence: &str,
    terminator: Option<char>,
    language: Option<&LanguageTag>,
) -> bool {
    if terminator.is_some_and(is_question_terminator) {
        return true;
    }

    // NFC-normalise before lowercasing so that NFD-decomposed input
    // (e.g. macOS file-system paths that decompose `é` into
    // `e + U+0301`) matches the NFC-composed table entries. The
    // `split` predicate below treats non-alphabetic codepoints —
    // including category-Mn combining marks like `U+0301` — as
    // token boundaries, so without NFC the Latin / Cyrillic
    // accented interrogatives (`qué`, `cómo`, `pourquoi`, `où`,
    // ...) and the Arabic tashkeel-marked forms would split into
    // pieces that never match the table. NFC is the standard input
    // form for chat protocols, so this is mostly defence-in-depth.
    // See Devin Review finding #ANALYSIS-0003b.
    let lower: String = sentence.trim().nfc().collect::<String>().to_lowercase();
    if lower.is_empty() {
        return false;
    }

    // Look up per-language interrogatives; fall back to English
    // when the tag is unknown or unconfigured.
    let (table, strategy) = language
        .map(LanguageTag::primary)
        .and_then(interrogatives_for)
        .or_else(|| interrogatives_for("en"))
        .expect("english fallback must always be configured in interrogatives table");

    match strategy {
        InterrogativeMatch::FirstToken => {
            let first = lower
                .split(|c: char| !c.is_alphabetic())
                .find(|s| !s.is_empty())
                .unwrap_or("");
            table.contains(&first)
        }
        InterrogativeMatch::Substring => table.iter().any(|i| lower.contains(*i)),
    }
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

fn lower_contains_any(haystack_lower: &str, needles: &[String]) -> bool {
    needles.iter().any(|n| haystack_lower.contains(n.as_str()))
}

fn starts_with_imperative(haystack_lower: &str, verbs: &[String]) -> bool {
    let first = haystack_lower
        .split(|c: char| !c.is_alphabetic())
        .find(|s| !s.is_empty())
        .unwrap_or("");
    verbs.iter().any(|v| v == first)
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

fn extract_capitalised_words(text: &str, stop_words: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split(|c: char| !c.is_alphabetic() && c != '\'') {
        if raw.is_empty() {
            continue;
        }
        let mut chars = raw.chars();
        let first = chars.next().unwrap();
        if first.is_uppercase()
            && raw.chars().count() >= 2
            && !stop_words.iter().any(|s| s.eq_ignore_ascii_case(raw))
        {
            out.push(raw.to_string());
        }
    }
    out
}

/// A sentence is considered "shaped" enough to be a Fact
/// candidate when it has either a whitespace separator (Latin /
/// Cyrillic / Arabic / Devanagari / etc.) **or** is a run of at
/// least 4 codepoints in a no-inter-word-whitespace script (CJK
/// or Thai). Without this fallback, the "contains space" gate
/// would silently drop every CJK / Thai sentence as
/// not-fact-shaped, since those scripts run words together with
/// no separator. Phase 1.4 added the CJK arm; the Thai arm was
/// added in the Devin Review fixup pass so Thai declaratives
/// like `กรุงเทพมหานครเป็นเมืองหลวงของประเทศไทย` can become Fact
/// observations.
fn is_sentence_shaped_for_fact(sentence: &str) -> bool {
    if sentence.contains(' ') {
        return true;
    }
    let unsegmented_chars = sentence
        .chars()
        .filter(|c| is_cjk_codepoint(*c) || is_thai_codepoint(*c))
        .count();
    unsegmented_chars >= 4
}

/// True for code points in the CJK script blocks: CJK Unified
/// Ideographs (`U+4E00..U+9FFF`), Hiragana (`U+3040..U+309F`),
/// Katakana (`U+30A0..U+30FF`), Hangul Syllables
/// (`U+AC00..U+D7AF`). Used by the sentence-shape heuristic and
/// by the per-sentence-language fallback path.
fn is_cjk_codepoint(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}'   // CJK Unified Ideographs
        | '\u{3040}'..='\u{309F}' // Hiragana
        | '\u{30A0}'..='\u{30FF}' // Katakana
        | '\u{AC00}'..='\u{D7AF}' // Hangul Syllables
        | '\u{3400}'..='\u{4DBF}' // CJK Unified Ideographs Extension A
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

impl LexiconExtractor {
    /// Shared implementation behind both [`ObservationExtractor::extract`]
    /// and [`ObservationExtractor::extract_with_dominant_language`].
    ///
    /// Routing both trait methods through this private helper
    /// instead of having one trait method call the other avoids
    /// the mutual-delegation infinite-recursion trap documented on
    /// the [`ObservationExtractor`] trait. See Devin Review
    /// finding #ANALYSIS-0002b.
    fn do_extract(
        &self,
        text: &str,
        scope: ScopeId,
        dominant_language: Option<&LanguageTag>,
    ) -> Vec<Observation> {
        let mut out = Vec::new();
        let mut seen_entities: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Phase 1.4: dominant language for entity-class observations
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
        // on every call. See Devin Review finding #ANALYSIS-0001d.
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
        for word in extract_capitalised_words(text, &self.stop_words) {
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
        // / facts. Phase 1.4: each sentence is independently
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
            let lower = sentence.to_lowercase();
            if lower_contains_any(&lower, &self.decision_keywords) {
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
            if lower_contains_any(&lower, &self.task_keywords)
                || starts_with_imperative(&lower, &self.task_imperative_verbs)
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
            if looks_like_question(sentence, slice.terminator, sentence_language.as_ref()) {
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
            // Anything sentence-shaped — Latin/Cyrillic/Arabic
            // sentences with whitespace OR CJK sentences with at
            // least 4 ideographs — and not picked up as a task /
            // decision / question is a Fact candidate.
            //
            // Two-gate design (Devin Review #ANALYSIS-0003):
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
        // pass. See Devin Review finding #ANALYSIS-0001d.
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
        // hints — see ANALYSIS-0001d and the comment inside
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
    // Phase 1.4 — multilingual sentence terminator + question detection
    // tests. These exercise the new char-based splitter, the per-language
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
    fn split_sentences_mixed_script_message() {
        // The motivating Phase 1.4 example: a bilingual chat
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
        // Devin Review #ANALYSIS-0003b: NFD-decomposed input (e.g.
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
        // Phase 1.4 makes.
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
        // Regression: pre-Phase-1.4 the fact gate required a
        // space character. A CJK declarative sentence has no
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
        // Regression for Devin Review #BUG-0002: Phase 1.4
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
    fn is_sentence_terminator_covers_phase_1_4_set() {
        // Defensive: pin the exact terminator set so accidental
        // additions/removals fail tests instead of silently
        // changing behaviour.
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
    fn extract_with_dominant_language_hint_is_honoured_for_entity_class() {
        // Devin Review #ANALYSIS-0001: pipeline + extractor used
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
        // Devin Review #ANALYSIS-0001d: the legacy `extract()`
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
        // Devin Review #ANALYSIS-0001d: callers that have already
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
}
