//! Phase 1.1 — multilingual `LexiconRegistry`.
//!
//! Phases 1.3 and 1.4 of the multilingual roadmap landed
//! per-message + per-sentence language detection and language-aware
//! question detection (via [`crate::interrogatives`]). Phase 1.1 is
//! the structural follow-on: a single typed registry that owns the
//! per-BCP-47-primary-subtag keyword tables for **all** lexicon
//! classes — decisions, tasks, task-imperative verbs, stop-words,
//! and (by delegation) interrogatives — and the shared
//! normalisation primitive that callers use to compare keyword
//! tables against running text in a script-aware way.
//!
//! ## Why a registry, and what it replaces
//!
//! Pre-Phase-1.1 [`crate::extractor::LexiconExtractor`] carried a
//! single set of English decision / task / imperative / stop-word
//! lists on its struct (the only built-in `english_default` set).
//! Per-sentence language detection was wired in Phase 1.4, but the
//! sentence-level matcher still consulted the same English
//! keyword lists no matter what language the sentence was
//! detected as. That worked for the per-sentence
//! [`crate::interrogatives`] lookup (which IS per-language) but
//! left every non-English decision / task sentence silently
//! falling through.
//!
//! Phase 1.1 fixes that by introducing [`LexiconRegistry`] —
//! a lookup-by-BCP-47-primary-subtag map of
//! [`LanguageLexicon`]s, each of which bundles the keyword
//! tables for every observation class. The extractor
//! resolves the right lexicon per sentence (using the sentence's
//! detected language, falling back to English when detection
//! produced `None` or the language has no configured lexicon).
//!
//! ## Deferred items from Phase 1.4 sweeps that this module closes
//!
//! * **NFC + locale-lowercase primitive** (Devin Review
//!   #ANALYSIS-0005, Phase 1.4 sweep 5). The Phase 1.4 question
//!   matcher applied NFC + lowercase ad-hoc inside
//!   [`crate::extractor::looks_like_question`] but the decision /
//!   task paths still used plain `to_lowercase`. That was safe
//!   only as long as those tables stayed ASCII. Phase 1.1 ships
//!   [`normalize_for_lookup`] as the single normalisation
//!   primitive every classifier path now uses, so Romance /
//!   Cyrillic / Arabic decision and task keywords match
//!   independently of the input's Unicode normalisation form.
//! * **Tashkeel-tolerant Arabic tokeniser** (Devin Review
//!   #ANALYSIS-0001, Phase 1.4 sweep 3). Arabic running text
//!   often carries non-spacing combining marks (fatha, kasra,
//!   damma, sukun, shadda, …) and the elongation glyph
//!   *tatweel* (U+0640). The Phase 1.4 FirstToken splitter
//!   broke on tashkeel because tashkeel codepoints are category
//!   `Mn` (non-alphabetic), which split tokens internally. The
//!   normalisation primitive in this module **strips** the
//!   Arabic combining marks and tatweel **before** lowercasing
//!   so that a tashkeel-decorated `كَيْفَ` matches the
//!   table entry `كيف`.
//! * **Bigram-prefix matching** (Devin Review #BUG-0001 /
//!   #FLAG-0002d, Phase 1.4 sweeps 1+4). Several languages
//!   form question / decision / task openers from multi-word
//!   collocations: Vietnamese `tại sao` ("why"),
//!   `khi nào` ("when"), `làm sao` ("how"); French
//!   `est-ce que`; Arabic `هل ال…` (yes/no opener that
//!   binds to the following definite article); Portuguese
//!   `por que`. The FirstToken strategy can't see these
//!   collocations and the Substring strategy is too loose for
//!   space-separated scripts. Phase 1.1 adds
//!   [`MatchStrategy::FirstBigram`] which compares the
//!   space-joined first two alphabetic tokens against the
//!   keyword table.
//!
//! ## Module-level invariants enforced by tests
//!
//! * Every BCP-47 primary subtag listed in
//!   [`SUPPORTED_LEXICON_TAGS`] has an entry in
//!   [`default_registry`] AND an entry in
//!   [`crate::interrogatives::interrogatives_for`].
//! * The English lexicon always exists (used as the fallback
//!   when the per-sentence language detection produces `None` or
//!   when the detected language has no configured lexicon).
//! * No keyword in any class contains an ASCII whitespace
//!   character (which would never match the way the
//!   first-token / substring matchers work). The only legal
//!   way to express a multi-word collocation is via the
//!   [`MatchStrategy::FirstBigram`] strategy, which splits the
//!   entry on the single ASCII space.
//! * No keyword duplicates within a class within a language.

use std::collections::BTreeMap;

use unicode_normalization::UnicodeNormalization;

use crate::interrogatives::{interrogatives_for, matching_strategy_for, InterrogativeMatch};

/// Which observation class a keyword table targets.
///
/// Used by the registry-aware matcher in
/// [`crate::extractor::LexiconExtractor`] to pick the right
/// keyword list per sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeywordClass {
    /// Decision keywords (e.g. `"decided"`, `"approved"`,
    /// `"ratified"`).
    Decision,
    /// Task keywords (e.g. `"todo"`, `"please"`, `"follow up"`).
    Task,
    /// Task imperative verbs (e.g. `"draft"`, `"send"`,
    /// `"deploy"`).
    TaskImperative,
    /// Interrogative words / collocations (e.g. `"who"`,
    /// `"なぜ"`, `"tại sao"`).
    Interrogative,
    /// Stop-words for the capitalised-token entity heuristic
    /// (e.g. `"The"`, `"Today"`).
    Stopword,
}

/// How a keyword in a lexicon table is matched against a
/// normalised sentence.
///
/// This unifies the [`crate::interrogatives::InterrogativeMatch`]
/// strategy (used by Phase 1.4 question detection) with the new
/// [`MatchStrategy::FirstBigram`] strategy added in Phase 1.1 to
/// cover multi-word collocations that the FirstToken /
/// Substring strategies can't express cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchStrategy {
    /// The first alphabetic token of the normalised sentence
    /// must exactly equal an entry in the table. Suitable for
    /// space-separated languages where the question / decision
    /// / task opener is canonically sentence-initial (English,
    /// German, Romance languages, Arabic, Vietnamese,
    /// Indonesian).
    FirstToken,
    /// Either the first alphabetic token OR the space-joined
    /// first two alphabetic tokens must exactly equal an entry
    /// in the table. Allows multi-word collocations
    /// (`"por que"`, `"tại sao"`, `"khi nào"`, `"est ce"`) to
    /// participate in space-separated-language matchers without
    /// degrading the single-token FirstToken strategy. Bigram
    /// entries are written with a single ASCII space.
    FirstBigram,
    /// Any entry in the table that appears as a substring of
    /// the normalised sentence counts as a match. Used for
    /// no-inter-word-whitespace scripts (CJK, Thai) where
    /// boundary-based token comparison doesn't apply.
    Substring,
}

impl MatchStrategy {
    /// Bridge from the Phase 1.4 interrogative-strategy enum to
    /// the Phase 1.1 registry strategy enum. Used by
    /// [`LexiconRegistry::interrogatives_for`] to expose the
    /// per-language interrogative matcher through the unified
    /// [`table_matches`] entry point. Phase 1.1
    /// (#ANALYSIS-0004): now maps
    /// [`InterrogativeMatch::FirstBigram`] (Vietnamese) to
    /// [`MatchStrategy::FirstBigram`] so the Vietnamese
    /// bigram interrogatives (`tại sao`, `khi nào`, `vì sao`)
    /// reach the matcher.
    pub fn from_interrogative_match(strategy: InterrogativeMatch) -> Self {
        match strategy {
            InterrogativeMatch::FirstToken => MatchStrategy::FirstToken,
            InterrogativeMatch::FirstBigram => MatchStrategy::FirstBigram,
            InterrogativeMatch::Substring => MatchStrategy::Substring,
        }
    }
}

/// Per-language keyword bundle.
///
/// Owned by the [`LexiconRegistry`]; one [`LanguageLexicon`]
/// per BCP-47 primary subtag we ship a built-in for. All keyword
/// data is `&'static [&'static str]` so building the registry is
/// allocation-free; the only runtime cost is the BTreeMap lookup.
#[derive(Debug, Clone, Copy)]
pub struct LanguageLexicon {
    /// BCP-47 primary subtag (`"en"`, `"ja"`, …). Used as the
    /// registry key.
    pub primary_tag: &'static str,
    /// English-language display name, for diagnostics and tests
    /// (`"English"`, `"Japanese"`).
    pub display_name: &'static str,
    /// Decision-class keywords. Matched against the normalised
    /// lowercase sentence via [`KeywordClass::Decision`] +
    /// [`Self::decision_strategy`].
    pub decision_keywords: &'static [&'static str],
    /// Strategy for matching [`Self::decision_keywords`].
    pub decision_strategy: MatchStrategy,
    /// Task-class keywords. Matched via [`KeywordClass::Task`]
    /// + [`Self::task_strategy`].
    pub task_keywords: &'static [&'static str],
    /// Strategy for matching [`Self::task_keywords`].
    pub task_strategy: MatchStrategy,
    /// Imperative verbs for task detection. Matched via
    /// [`KeywordClass::TaskImperative`], which uses
    /// [`MatchStrategy::FirstBigram`] unconditionally so that
    /// single-word imperatives (English `please`, German
    /// `bitte`, Vietnamese `vui`) still match via the
    /// first-token arm while multi-syllable Vietnamese
    /// imperatives (`triển khai`, `chuẩn bị`, `cập nhật`)
    /// match via the bigram arm. Bigram entries must be written
    /// with a single ASCII space separating the two alphabetic
    /// tokens; see
    /// [`first_alphabetic_bigram`](crate::lexicon::first_alphabetic_bigram).
    /// See Devin Review finding #BUG-0002 (Phase 1.1) — the
    /// strategy is now documented to match the code.
    pub task_imperative_verbs: &'static [&'static str],
    /// Stop-words for the capitalised-token entity extractor.
    /// Only relevant for languages with case distinction —
    /// CJK / Arabic / Thai lexicons leave this empty.
    pub stop_words: &'static [&'static str],
}

impl LanguageLexicon {
    /// Returns the entries + strategy for a given keyword
    /// class, or `None` for the interrogative class (which is
    /// served by [`LexiconRegistry::interrogatives_for`] —
    /// the interrogative tables live in
    /// [`crate::interrogatives`] for historical reasons and to
    /// avoid duplicating the Phase 1.4 data).
    pub fn entries(&self, class: KeywordClass) -> Option<(&'static [&'static str], MatchStrategy)> {
        match class {
            KeywordClass::Decision => Some((self.decision_keywords, self.decision_strategy)),
            KeywordClass::Task => Some((self.task_keywords, self.task_strategy)),
            // TaskImperative uses FirstBigram unconditionally:
            // FirstBigram is a strict superset of FirstToken
            // (it tries FirstToken first, then the bigram), so
            // single-word imperative verbs in en / es / fr /
            // de / pt / it / ru still match via the FirstToken
            // arm, while Vietnamese-style multi-syllable
            // imperative verbs (`triển khai`, `chuẩn bị`,
            // `cập nhật`) match via the bigram arm. This
            // unifies the imperative-verb path so the lexicon
            // doesn't need a per-language strategy field for
            // imperatives.
            KeywordClass::TaskImperative => {
                Some((self.task_imperative_verbs, MatchStrategy::FirstBigram))
            }
            KeywordClass::Stopword => Some((self.stop_words, MatchStrategy::FirstToken)),
            // Interrogatives are served via `interrogatives_for`
            // (Phase 1.4 module) — see `interrogatives_for` on
            // the registry.
            KeywordClass::Interrogative => None,
        }
    }
}

/// Registry of per-language lexicons, keyed by BCP-47 primary
/// subtag.
///
/// Construct via [`default_registry`] for the canonical
/// built-in set; tests and custom integrations may construct
/// their own via [`LexiconRegistry::from_static`] passing a
/// custom `&'static [LanguageLexicon]`.
#[derive(Debug, Clone)]
pub struct LexiconRegistry {
    by_tag: BTreeMap<&'static str, &'static LanguageLexicon>,
}

impl LexiconRegistry {
    /// Build a registry from a static slice of lexicons. Panics
    /// at construction time if the English lexicon (`"en"`) is
    /// missing, since the English fallback is part of the
    /// matcher contract.
    pub fn from_static(lexicons: &'static [LanguageLexicon]) -> Self {
        let mut by_tag = BTreeMap::new();
        for lex in lexicons {
            assert!(
                !lex.primary_tag.is_empty(),
                "LanguageLexicon primary_tag must not be empty"
            );
            let prev = by_tag.insert(lex.primary_tag, lex);
            assert!(
                prev.is_none(),
                "duplicate LanguageLexicon primary_tag {:?}",
                lex.primary_tag
            );
        }
        assert!(
            by_tag.contains_key("en"),
            "LexiconRegistry::from_static requires an English ('en') lexicon as the fallback"
        );
        Self { by_tag }
    }

    /// Look up the lexicon for a BCP-47 primary subtag. Returns
    /// `None` when no lexicon is configured for the tag.
    pub fn lexicon_for(&self, primary_tag: &str) -> Option<&'static LanguageLexicon> {
        self.by_tag.get(primary_tag).copied()
    }

    /// Look up the lexicon for a BCP-47 primary subtag, falling
    /// back to the English lexicon when the tag is unconfigured
    /// or `None`. The English fallback is guaranteed to exist
    /// by [`Self::from_static`].
    pub fn lexicon_for_or_english(&self, primary_tag: Option<&str>) -> &'static LanguageLexicon {
        primary_tag
            .and_then(|t| self.lexicon_for(t))
            .or_else(|| self.lexicon_for("en"))
            .expect("English fallback lexicon must exist in registry")
    }

    /// Look up the interrogative table + matching strategy for
    /// a BCP-47 primary subtag. Delegates to the Phase 1.4
    /// [`crate::interrogatives::interrogatives_for`] so the
    /// registry and the question-detection path share a single
    /// source of truth for interrogative entries.
    pub fn interrogatives_for(
        &self,
        primary_tag: &str,
    ) -> Option<(&'static [&'static str], MatchStrategy)> {
        interrogatives_for(primary_tag)
            .map(|(list, strat)| (list, MatchStrategy::from_interrogative_match(strat)))
    }

    /// Convenience: matching strategy alone for an
    /// interrogative table. Returns `None` when the tag has no
    /// configured interrogatives.
    pub fn interrogative_strategy_for(&self, primary_tag: &str) -> Option<MatchStrategy> {
        matching_strategy_for(primary_tag).map(MatchStrategy::from_interrogative_match)
    }

    /// Iterate over every configured lexicon. Used by
    /// invariant tests and diagnostics.
    pub fn iter(&self) -> impl Iterator<Item = &'static LanguageLexicon> + '_ {
        self.by_tag.values().copied()
    }

    /// Sorted list of configured primary tags. Order is
    /// `BTreeMap` order (lexicographic).
    pub fn supported_tags(&self) -> Vec<&'static str> {
        self.by_tag.keys().copied().collect()
    }
}

// ---------------------------------------------------------------------
// Normalisation primitives
// ---------------------------------------------------------------------

/// True for Arabic combining marks (tashkeel + Quranic
/// annotations) and the elongation glyph *tatweel* (U+0640).
///
/// Ranges from the Unicode 15 Arabic block:
///
/// * `U+0610..=U+061A` — Arabic small letters above
///   (Quranic annotations).
/// * `U+064B..=U+065F` — Arabic vowel marks (fatha, damma,
///   kasra, sukun, shadda, …).
/// * `U+0670` — Arabic letter superscript alef.
/// * `U+06D6..=U+06ED` — Arabic small marks for Quran.
/// * `U+06DF..=U+06E8`, `U+06EA..=U+06ED` — Arabic small marks.
/// * `U+0640` — Arabic tatweel (elongation glyph; purely
///   decorative).
///
/// These codepoints are stripped during
/// [`normalize_for_lookup`] when the language tag is `"ar"`
/// (or any other Arabic-script primary subtag we add later)
/// so that a tashkeel-decorated word like `كَيْفَ` matches the
/// canonical table entry `كيف`. Without the strip, the
/// FirstToken splitter (which treats non-alphabetic codepoints
/// as token boundaries) would break the word into pieces that
/// never match.
///
/// We strip rather than NFKD-decompose because NFKD would also
/// touch unrelated codepoints (e.g. fullwidth Latin, CJK
/// compatibility ideographs) — strip-by-range is targeted and
/// fully deterministic. See Devin Review finding
/// #ANALYSIS-0001 (Phase 1.4 sweep 3 deferred to Phase 1.1).
pub fn is_arabic_combining_or_tatweel(c: char) -> bool {
    matches!(c,
        '\u{0610}'..='\u{061A}'
        | '\u{064B}'..='\u{065F}'
        | '\u{0670}'
        | '\u{06D6}'..='\u{06ED}'
        | '\u{0640}'
    )
}

/// True for the bidirectional / zero-width formatting
/// codepoints that should be stripped during normalisation
/// for **all** languages: zero-width joiner (U+200D),
/// zero-width non-joiner (U+200C), left-to-right /
/// right-to-left marks + embedding controls (U+200E, U+200F,
/// U+202A..=U+202E, U+2066..=U+2069).
///
/// These are category `Cf` (format) codepoints that are
/// neither alphabetic nor structural; they appear in
/// copy-pasted IM text and would otherwise act as token
/// boundaries under the FirstToken splitter (`Cf` is not
/// alphabetic per [`char::is_alphabetic`]).
pub fn is_bidi_or_zwj_format(c: char) -> bool {
    matches!(c,
        '\u{200C}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2066}'..='\u{2069}'
    )
}

/// Normalise an arbitrary input string for lexicon lookup.
///
/// Steps applied in order:
///
/// 1. Trim leading / trailing ASCII + Unicode whitespace.
/// 2. Strip bidirectional + zero-width formatting codepoints
///    (script-independent — these never carry semantic
///    content).
/// 3. If the primary tag is one of the Arabic-script BCP-47
///    primary subtags (`ar`, `fa`, `ur`, `ckb`, `ps`, `sd`),
///    strip Arabic combining marks (tashkeel) and tatweel.
/// 4. NFC-compose the remainder. NFC matches the canonical
///    form of every lexicon entry in this module and matches
///    the form chat protocols normalise to before sending.
/// 5. Lowercase via [`str::to_lowercase`], which is locale-
///    independent but does the right thing for every language
///    whose lexicon we ship today (it's not the
///    locale-aware fold prescribed by CLDR — see the
///    long-term note below — but the deltas vs. CLDR fold for
///    en/es/fr/de/pt/it/vi/id/ar/ja/ko/zh/th are nil for our
///    keyword tables, all of which are already lowercase /
///    case-insensitive in their script).
///
/// **Long-term note on locale-aware lowercasing:** Rust's
/// `str::to_lowercase` follows the Unicode default-case
/// mapping, not the locale-aware mapping that CLDR
/// prescribes. The two diverge for a handful of letters
/// (notably Turkish `İ` ↔ `i` / `I` ↔ `ı`, which Rust folds to
/// the wrong member of each pair under the default mapping).
/// We do not currently ship a Turkish (`tr`) lexicon. When we
/// do — likely Phase 2.x — we'll pull in `icu_casemap` (or
/// build a thin tr-specific override here) to handle the
/// dotted-vs-dotless `i` correctly. The signature of
/// `normalize_for_lookup` is already locale-aware via the
/// `primary_tag` parameter, so adding a special case there is
/// non-breaking.
pub fn normalize_for_lookup(text: &str, primary_tag: Option<&str>) -> String {
    let mut buf: String = text
        .trim()
        .chars()
        .filter(|c| !is_bidi_or_zwj_format(*c))
        .collect();

    if let Some(tag) = primary_tag {
        if is_arabic_script_primary_tag(tag) {
            buf = buf
                .chars()
                .filter(|c| !is_arabic_combining_or_tatweel(*c))
                .collect();
        }
    }

    let nfc: String = buf.nfc().collect();
    nfc.to_lowercase()
}

/// True for BCP-47 primary subtags whose canonical script is
/// the Arabic abjad (and which therefore need tashkeel /
/// tatweel stripped during normalisation). Conservative
/// list — only the subtags we have realistic recall on
/// (Arabic + Farsi + Urdu + Sorani Kurdish + Pashto + Sindhi).
/// Other Arabic-script languages can be added when we ship
/// lexicons for them.
fn is_arabic_script_primary_tag(primary_tag: &str) -> bool {
    matches!(primary_tag, "ar" | "fa" | "ur" | "ckb" | "ps" | "sd")
}

// ---------------------------------------------------------------------
// Token-extraction helpers used by the matchers
// ---------------------------------------------------------------------

/// Iterator over the alphabetic-only tokens of `normalised`.
///
/// "Alphabetic" follows [`char::is_alphabetic`] — letters in
/// any script — so the same splitter works for Latin,
/// Cyrillic, Arabic (after tashkeel-strip), Devanagari, and
/// any other space-separated script. CJK / Thai consumers
/// should use [`MatchStrategy::Substring`] instead of relying
/// on tokenisation.
fn alphabetic_tokens(normalised: &str) -> impl Iterator<Item = &str> {
    normalised
        .split(|c: char| !c.is_alphabetic())
        .filter(|s| !s.is_empty())
}

/// First alphabetic token in the normalised sentence, or the
/// empty string if there is none.
pub fn first_alphabetic_token(normalised: &str) -> &str {
    alphabetic_tokens(normalised).next().unwrap_or("")
}

/// Space-joined first two alphabetic tokens, or `None` when
/// the sentence has fewer than two such tokens.
pub fn first_alphabetic_bigram(normalised: &str) -> Option<String> {
    let mut it = alphabetic_tokens(normalised);
    let a = it.next()?;
    let b = it.next()?;
    Some(format!("{a} {b}"))
}

/// True when **any** entry in the table matches the normalised
/// sentence under the requested strategy.
///
/// This is the unified matcher used by both the Phase 1.4
/// question detection path (via the registry's interrogative
/// lookup) and the new Phase 1.1 decision / task / imperative
/// paths. Bigram entries are written with a single ASCII space
/// and checked against the space-joined first two alphabetic
/// tokens (see [`first_alphabetic_bigram`]).
pub fn table_matches(table: &[&str], normalised: &str, strategy: MatchStrategy) -> bool {
    match strategy {
        MatchStrategy::FirstToken => {
            let first = first_alphabetic_token(normalised);
            !first.is_empty() && table.contains(&first)
        }
        MatchStrategy::FirstBigram => {
            let first = first_alphabetic_token(normalised);
            let first_matches = !first.is_empty() && table.contains(&first);
            if first_matches {
                return true;
            }
            let Some(bigram) = first_alphabetic_bigram(normalised) else {
                return false;
            };
            table.iter().any(|e| *e == bigram)
        }
        MatchStrategy::Substring => table.iter().any(|e| normalised.contains(*e)),
    }
}

// ---------------------------------------------------------------------
// Per-language lexicon definitions (12 BCP-47 primary subtags)
// ---------------------------------------------------------------------

/// English (`en`) — substrate default. Keyword entries
/// imported verbatim from the pre-Phase-1.1
/// [`crate::extractor::LexiconExtractor::english_default`] so
/// the migration is observably-identical for the en path
/// (English is the dominant test corpus).
const EN_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "en",
    display_name: "English",
    decision_keywords: &[
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
    decision_strategy: MatchStrategy::Substring,
    task_keywords: &[
        "todo",
        "action",
        "task",
        "please",
        "fyi action",
        "follow up",
        "follow-up",
    ],
    task_strategy: MatchStrategy::Substring,
    task_imperative_verbs: &[
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
    stop_words: &[
        "the",
        "this",
        "that",
        "these",
        "those",
        "it",
        "today",
        "tomorrow",
        "yesterday",
        "friday",
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "saturday",
        "sunday",
        "may",
        "june",
        "july",
    ],
};

/// Spanish (`es`).
///
/// Sources: Real Academia Española (DRAE / Nueva Gramática)
/// for vocabulary; substrate decision / task class names
/// (`decidir`, `acordar`, `aprobar`, `firmar`, `ratificar`,
/// `rechazar`) are everyday register, not court / legal
/// register. Imperative verbs (Spanish 2nd-person-singular
/// `tú` form) selected for substrate-typical actions
/// (drafting, sending, scheduling, reviewing). `por favor`
/// + `por favor,` covers the polite-request opener.
const ES_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "es",
    display_name: "Spanish",
    decision_keywords: &[
        "decidido",
        "decidida",
        "decidimos",
        "decisión",
        "acordado",
        "acordada",
        "acordamos",
        "aprobado",
        "aprobada",
        "ratificado",
        "ratificada",
        "firmado",
        "firmada",
        "rechazado",
        "rechazada",
    ],
    decision_strategy: MatchStrategy::Substring,
    task_keywords: &[
        "tarea",
        "pendiente",
        "por favor",
        "favor de",
        "acción",
        "seguimiento",
    ],
    task_strategy: MatchStrategy::Substring,
    task_imperative_verbs: &[
        "redacta",
        "envía",
        "programa",
        "revisa",
        "publica",
        "arregla",
        "despliega",
        "investiga",
        "prepara",
        "actualiza",
        "fusiona",
        "verifica",
    ],
    stop_words: &[
        "el", "la", "los", "las", "un", "una", "esto", "esta", "eso", "esa", "hoy", "mañana",
        "ayer",
    ],
};

/// French (`fr`).
///
/// Decision-class verbs cover the canonical
/// "decided / agreed / approved / signed / ratified /
/// rejected" register from Larousse + Le Robert.
/// Past-participle forms (`décidé`, `convenu`, `approuvé`,
/// `signé`, `ratifié`, `rejeté`) are deliberately included
/// because passive-perfect (`a été …`) and present-perfect
/// (`avons …`) are the dominant constructions for
/// announcement sentences.
const FR_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "fr",
    display_name: "French",
    decision_keywords: &[
        "décidé",
        "décidée",
        "décidons",
        "décision",
        "convenu",
        "convenue",
        "approuvé",
        "approuvée",
        "ratifié",
        "ratifiée",
        "signé",
        "signée",
        "rejeté",
        "rejetée",
    ],
    decision_strategy: MatchStrategy::Substring,
    task_keywords: &[
        "à faire",
        "tâche",
        "action",
        "merci de",
        "s'il vous plaît",
        "s'il te plaît",
        "suivi",
    ],
    task_strategy: MatchStrategy::Substring,
    task_imperative_verbs: &[
        "rédige",
        "envoie",
        "planifie",
        "vérifie",
        "publie",
        "corrige",
        "déploie",
        "lance",
        "investigue",
        "prépare",
        "mets",
        "fusionne",
    ],
    stop_words: &[
        "le",
        "la",
        "les",
        "un",
        "une",
        "des",
        "ce",
        "cette",
        "ces",
        "il",
        "elle",
        "aujourd'hui",
        "hier",
        "demain",
    ],
};

/// German (`de`).
///
/// Decision-class verbs follow Duden ("Beschlossen",
/// "Entschieden", "Vereinbart", "Genehmigt", "Bestätigt",
/// "Abgelehnt"). Task verbs are the dominant imperative
/// forms in German project-management copy (`schreibe`,
/// `sende`, `plane`, …). Past-participle forms are kept
/// because German passive ("wurde beschlossen") and present-
/// perfect ("haben beschlossen") are the most frequent
/// constructions for decision sentences.
const DE_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "de",
    display_name: "German",
    decision_keywords: &[
        "beschlossen",
        "entschieden",
        "vereinbart",
        "genehmigt",
        "bestätigt",
        "abgelehnt",
        "ratifiziert",
        "unterzeichnet",
    ],
    decision_strategy: MatchStrategy::Substring,
    task_keywords: &[
        "aufgabe",
        "bitte",
        "to-do",
        "todo",
        "nachverfolgung",
        "follow-up",
    ],
    task_strategy: MatchStrategy::Substring,
    task_imperative_verbs: &[
        "schreibe",
        "sende",
        "plane",
        "prüfe",
        "veröffentliche",
        "behebe",
        "deploye",
        "untersuche",
        "bereite",
        "aktualisiere",
        "merge",
    ],
    stop_words: &[
        "der", "die", "das", "den", "dem", "ein", "eine", "einen", "dieser", "diese", "dieses",
        "heute", "morgen", "gestern",
    ],
};

/// Portuguese (`pt`).
///
/// Both Brazilian and European Portuguese share these forms.
/// Decision verbs include both 1st-person-plural perfect
/// (`decidimos`, `aprovamos`) and past-participle
/// (`decidido`, `aprovado`) because both constructions are
/// common in announcements.
const PT_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "pt",
    display_name: "Portuguese",
    decision_keywords: &[
        "decidido",
        "decidida",
        "decidimos",
        "decisão",
        "acordado",
        "acordada",
        "acordamos",
        "aprovado",
        "aprovada",
        "ratificado",
        "ratificada",
        "assinado",
        "assinada",
        "rejeitado",
        "rejeitada",
    ],
    decision_strategy: MatchStrategy::Substring,
    task_keywords: &["tarefa", "pendente", "por favor", "ação", "acompanhamento"],
    task_strategy: MatchStrategy::Substring,
    task_imperative_verbs: &[
        "redige",
        "envia",
        "agenda",
        "revisa",
        "publica",
        "corrige",
        "implanta",
        "investiga",
        "prepara",
        "atualiza",
        "mescla",
        "lança",
    ],
    stop_words: &[
        "o", "a", "os", "as", "um", "uma", "este", "esta", "isso", "essa", "hoje", "amanhã",
        "ontem",
    ],
};

/// Italian (`it`).
///
/// Decision verbs from Treccani — `deciso`, `concordato`,
/// `approvato`, `ratificato`, `firmato`, `respinto`. The
/// imperative-verb list uses the 2nd-person-singular
/// `tu`-form which is the dominant project-management
/// register.
const IT_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "it",
    display_name: "Italian",
    decision_keywords: &[
        "deciso",
        "decisa",
        "decisione",
        "concordato",
        "concordata",
        "approvato",
        "approvata",
        "ratificato",
        "ratificata",
        "firmato",
        "firmata",
        "respinto",
        "respinta",
    ],
    decision_strategy: MatchStrategy::Substring,
    task_keywords: &["compito", "azione", "per favore", "seguimento", "follow-up"],
    task_strategy: MatchStrategy::Substring,
    task_imperative_verbs: &[
        "redigi",
        "invia",
        "pianifica",
        "rivedi",
        "pubblica",
        "correggi",
        "distribuisci",
        "investiga",
        "prepara",
        "aggiorna",
        "unisci",
        "lancia",
    ],
    stop_words: &[
        "il", "la", "lo", "i", "gli", "le", "un", "una", "questo", "questa", "quello", "oggi",
        "domani", "ieri",
    ],
};

/// Russian (`ru`).
///
/// Decision verbs cover both perfective (`решено`,
/// `согласовано`, `утверждено`) and imperfective forms.
/// All entries are pre-lowercased Cyrillic.
const RU_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "ru",
    display_name: "Russian",
    decision_keywords: &[
        "решено",
        "решили",
        "решение",
        "согласовано",
        "согласованы",
        "утверждено",
        "утвердили",
        "ратифицировано",
        "подписано",
        "отклонено",
    ],
    decision_strategy: MatchStrategy::Substring,
    task_keywords: &[
        "задача",
        "пожалуйста",
        "todo",
        "to-do",
        "последующие действия",
    ],
    task_strategy: MatchStrategy::Substring,
    task_imperative_verbs: &[
        "напиши",
        "отправь",
        "запланируй",
        "проверь",
        "опубликуй",
        "исправь",
        "разверни",
        "исследуй",
        "подготовь",
        "обнови",
        "слей",
    ],
    stop_words: &[
        "это",
        "эта",
        "этот",
        "эти",
        "те",
        "та",
        "тот",
        "сегодня",
        "завтра",
        "вчера",
    ],
};

/// Arabic (`ar`).
///
/// All entries are unvocalised (no tashkeel) because the
/// normalisation primitive strips tashkeel from input before
/// matching. Decision verbs cover both passive perfect
/// (`تم اعتماد …` — "the … was approved") and active perfect
/// (`قررنا` — "we decided"). Task class includes
/// `من فضلك` ("please") and `يرجى` ("kindly") which are the
/// canonical polite-request openers.
const AR_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "ar",
    display_name: "Arabic",
    decision_keywords: &[
        "تقرر",
        "قررنا",
        "قرار",
        "موافق",
        "اعتمد",
        "اعتماد",
        "صادق",
        "وقع",
        "رفض",
    ],
    decision_strategy: MatchStrategy::Substring,
    task_keywords: &["مهمة", "من فضلك", "يرجى", "متابعة", "إجراء"],
    task_strategy: MatchStrategy::Substring,
    // Note: Arabic verbs are unvocalised (no tashkeel) to
    // match the canonical table form. `وزع` ("distribute /
    // deploy") is used for the deploy semantic; `نشر` is
    // reserved for the publish semantic. `حضر` is the
    // unvocalised form of the imperative `حضّر` ("prepare")
    // — the matcher strips tashkeel from input before
    // comparison, so the table entry must also be unvocalised.
    task_imperative_verbs: &[
        "اكتب", "أرسل", "جدول", "راجع", "انشر", "أصلح", "وزع", "تحقق", "حضر", "حدث", "ادمج",
    ],
    stop_words: &[],
};

/// Vietnamese (`vi`).
///
/// Vietnamese is space-separated but with a productive set of
/// multi-syllable / multi-word collocations. Decision verbs
/// include both single (`quyết`, `chốt`) and bigram
/// (`quyết định`, `phê duyệt`, `đồng ý`) forms — the
/// Substring strategy on the decision class catches both
/// without us having to switch strategies. Task class adds
/// `vui lòng` ("please" / polite request) and `xin` (the
/// older polite particle, also used in `xin hãy` constructions).
///
/// Interrogatives like `tại sao` / `khi nào` are handled by
/// the Phase 1.4 `interrogatives` table — which is now driven
/// by [`MatchStrategy::FirstBigram`] via the registry — so
/// they are not duplicated here.
const VI_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "vi",
    display_name: "Vietnamese",
    decision_keywords: &[
        "quyết định",
        "đã quyết",
        "chốt",
        "đồng ý",
        "phê duyệt",
        "đã thông qua",
        "đã ký",
        "từ chối",
    ],
    decision_strategy: MatchStrategy::Substring,
    task_keywords: &[
        "nhiệm vụ",
        "vui lòng",
        "làm ơn",
        "xin hãy",
        "theo dõi",
        "follow-up",
    ],
    task_strategy: MatchStrategy::Substring,
    task_imperative_verbs: &[
        "soạn",
        "gửi",
        "lên",
        "duyệt",
        "đăng",
        "sửa",
        "triển khai",
        "điều tra",
        "chuẩn bị",
        "cập nhật",
        "hợp nhất",
    ],
    // Stop-words are single-token only (the capitalised-token
    // entity extractor compares each capitalised word
    // independently, so multi-word entries like `hôm nay` would
    // never match). Bigrams that are still entity stop-words in
    // intent ("hôm nay" = "today", "hôm qua" = "yesterday")
    // are handled by leaving the constituent single tokens out
    // and relying on the fact that they are lowercase in
    // running text — the capitalised-token entity heuristic
    // only fires on capitalised tokens, so lowercase `hôm` and
    // `nay` never become candidate entities in the first place.
    stop_words: &["này", "kia", "đó", "đây"],
};

/// Indonesian (`id`).
///
/// Indonesian and Malay share most vocabulary; the `id` and
/// `ms` lexicons are identical at this stage of Phase 1.1
/// because the decision / task lexicons we ship don't yet
/// differentiate the few register-specific entries between
/// the two (Phase 2 SLM-assisted extraction will handle the
/// register difference). Task class includes `mohon` /
/// `tolong` ("please") and `silakan` (formal "please go
/// ahead"). Decision verbs are mostly past-participle prefix
/// `di-` forms (`diputuskan`, `disetujui`, `disahkan`).
const ID_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "id",
    display_name: "Indonesian",
    decision_keywords: &[
        "diputuskan",
        "memutuskan",
        "keputusan",
        "disetujui",
        "menyetujui",
        "disahkan",
        "ditandatangani",
        "ditolak",
    ],
    decision_strategy: MatchStrategy::Substring,
    task_keywords: &["tugas", "tolong", "mohon", "silakan", "tindak lanjut"],
    task_strategy: MatchStrategy::Substring,
    task_imperative_verbs: &[
        "buat",
        "kirim",
        "jadwalkan",
        "tinjau",
        "terbitkan",
        "perbaiki",
        "terapkan",
        "selidiki",
        "siapkan",
        "perbarui",
        "gabungkan",
    ],
    // Single-token only — see Vietnamese stop-words note. `hari
    // ini` ("today") is multi-word and is handled by the
    // lowercase-fast-path observation above.
    stop_words: &["ini", "itu", "kemarin", "besok"],
};

/// Malay (`ms`). Currently aliases the Indonesian lexicon;
/// see the doc on [`ID_LEXICON`] for the rationale and the
/// Phase-2 follow-up. Kept as a distinct constant so that
/// when we differentiate the two in Phase 2, the change is
/// observable in this file rather than via an `alias` map.
const MS_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "ms",
    display_name: "Malay",
    decision_keywords: ID_LEXICON.decision_keywords,
    decision_strategy: ID_LEXICON.decision_strategy,
    task_keywords: ID_LEXICON.task_keywords,
    task_strategy: ID_LEXICON.task_strategy,
    task_imperative_verbs: ID_LEXICON.task_imperative_verbs,
    stop_words: ID_LEXICON.stop_words,
};

/// Hindi (`hi`).
///
/// Devanagari script. Decision verbs cover both
/// `निर्णय` ("decision" — noun) and `तय` / `सहमत` (verb
/// past-participles). Task class includes `कृपया`
/// ("please") which is the canonical polite-request opener.
const HI_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "hi",
    display_name: "Hindi",
    decision_keywords: &[
        "निर्णय",
        "तय",
        "सहमत",
        "स्वीकृत",
        "अनुमोदित",
        "अनुमोदन",
        "हस्ताक्षरित",
        "अस्वीकृत",
    ],
    decision_strategy: MatchStrategy::Substring,
    task_keywords: &["कार्य", "कृपया", "अनुरोध", "अनुवर्ती", "टू डू"],
    task_strategy: MatchStrategy::Substring,
    task_imperative_verbs: &[
        "लिखें",
        "भेजें",
        "अनुसूचित",
        "समीक्षा",
        "प्रकाशित",
        "ठीक",
        "तैनात",
        "जांच",
        "तैयार",
        "अद्यतन",
        "मर्ज",
    ],
    stop_words: &["यह", "वह", "ये", "वे", "आज", "कल", "परसों"],
};

/// Japanese (`ja`).
///
/// CJK script (no inter-word whitespace) → all classes use
/// [`MatchStrategy::Substring`]. Decision keywords cover
/// formal-register verbs (`決定`, `承認`, `合意`, `署名`,
/// `批准`, `却下`) plus the colloquial `決まり`.
/// Task class includes `〜してください` polite forms and the
/// shorter `お願い` opener.
const JA_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "ja",
    display_name: "Japanese",
    decision_keywords: &[
        "決定",
        "決まり",
        "決まりました",
        "合意",
        "承認",
        "批准",
        "署名",
        "却下",
    ],
    decision_strategy: MatchStrategy::Substring,
    task_keywords: &[
        "タスク",
        "アクション",
        "お願い",
        "してください",
        "フォローアップ",
    ],
    task_strategy: MatchStrategy::Substring,
    // CJK imperative verbs are not used (the matcher splits
    // on alphabetic tokens, which CJK doesn't have). The
    // imperative-verb table is therefore empty for CJK
    // languages; the substring task-keyword table is the
    // only path that fires for CJK task sentences.
    task_imperative_verbs: &[],
    stop_words: &[],
};

/// Korean (`ko`).
///
/// Hangul script (no inter-word whitespace in practice for
/// the purposes of substring matching). Decision verbs use
/// the formal `합니다` register: `결정합니다`, `합의합니다`,
/// `승인합니다`. Task class includes `해주세요` and
/// `부탁합니다` (polite request openers).
const KO_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "ko",
    display_name: "Korean",
    decision_keywords: &[
        "결정",
        "결정합니다",
        "합의",
        "합의합니다",
        "승인",
        "비준",
        "서명",
        "거부",
    ],
    decision_strategy: MatchStrategy::Substring,
    task_keywords: &["작업", "업무", "부탁합니다", "해주세요", "팔로업"],
    task_strategy: MatchStrategy::Substring,
    task_imperative_verbs: &[],
    stop_words: &[],
};

/// Mandarin Chinese (`zh`).
///
/// Hanzi script (no inter-word whitespace). Decision verbs
/// include both Simplified and Traditional variants where
/// they diverge (`决定` / `決定`, `批准` is shared,
/// `签字` / `簽字`).
const ZH_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "zh",
    display_name: "Chinese",
    decision_keywords: &[
        "决定", "決定", "同意", "批准", "通过", "通過", "签字", "簽字", "驳回", "駁回",
    ],
    decision_strategy: MatchStrategy::Substring,
    task_keywords: &["任务", "任務", "请", "請", "麻烦", "麻煩", "跟进", "跟進"],
    task_strategy: MatchStrategy::Substring,
    task_imperative_verbs: &[],
    stop_words: &[],
};

/// Thai (`th`).
///
/// Thai script (no inter-word whitespace). Decision verbs
/// cover formal `ตัดสิน` and the more colloquial `เห็นชอบ`
/// + `อนุมัติ`. Task class includes `กรุณา` ("please" —
///   formal) and `โปรด` (very formal opener).
const TH_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "th",
    display_name: "Thai",
    decision_keywords: &["ตัดสินใจ", "เห็นชอบ", "อนุมัติ", "ตกลง", "ลงนาม", "ปฏิเสธ"],
    decision_strategy: MatchStrategy::Substring,
    task_keywords: &["งาน", "กรุณา", "โปรด", "ติดตาม"],
    task_strategy: MatchStrategy::Substring,
    task_imperative_verbs: &[],
    stop_words: &[],
};

/// All built-in lexicons, in BCP-47-primary-tag order.
///
/// The exact set is the union of:
///
/// * Phase 1.4 [`SUPPORTED_PRIMARY_TAGS`] interrogative
///   coverage (16 languages: en/es/fr/de/pt/it/ru/vi/id/ms/ar/hi/ja/ko/zh/th).
/// * Phase 1.1 keyword-class coverage requirements: a
///   keyword bundle per language for the substrate's
///   built-in decision / task / imperative pipelines.
///
/// 16 languages ship in Phase 1.1 — the 12-language target
/// from the Phase 1.1 outline (en/ja/ko/zh/es/fr/de/pt/ar/vi/th/id)
/// plus the four Phase 1.4 add-ons (`it`, `ru`, `hi`, `ms`)
/// that already have interrogative tables, to avoid the
/// invariant-test failure that would result if the
/// interrogative table covered a language that the
/// LexiconRegistry did not.
pub const BUILTIN_LEXICONS: &[LanguageLexicon] = &[
    AR_LEXICON, DE_LEXICON, EN_LEXICON, ES_LEXICON, FR_LEXICON, HI_LEXICON, ID_LEXICON, IT_LEXICON,
    JA_LEXICON, KO_LEXICON, MS_LEXICON, PT_LEXICON, RU_LEXICON, TH_LEXICON, VI_LEXICON, ZH_LEXICON,
];

/// All BCP-47 primary tags shipped in the built-in
/// [`default_registry`]. Mirrors
/// [`crate::interrogatives::SUPPORTED_PRIMARY_TAGS`] by
/// design (one is the test invariant for the other).
pub const SUPPORTED_LEXICON_TAGS: &[&str] = &[
    "ar", "de", "en", "es", "fr", "hi", "id", "it", "ja", "ko", "ms", "pt", "ru", "th", "vi", "zh",
];

/// Return a reference to the process-wide built-in
/// [`LexiconRegistry`], constructed once via [`std::sync::OnceLock`].
pub fn default_registry() -> &'static LexiconRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<LexiconRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| LexiconRegistry::from_static(BUILTIN_LEXICONS))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Normalisation primitive tests
    // -----------------------------------------------------------------

    #[test]
    fn normalize_strips_arabic_tashkeel_when_tag_is_arabic() {
        // كَيْفَ (with fatha + sukun + fatha) → كيف
        let raw = "كَيْفَ";
        let normalised = normalize_for_lookup(raw, Some("ar"));
        assert_eq!(normalised, "كيف");
    }

    #[test]
    fn normalize_strips_arabic_tatweel_when_tag_is_arabic() {
        // كــــم with three tatweel U+0640 → كم
        let raw = "كــــم";
        let normalised = normalize_for_lookup(raw, Some("ar"));
        assert_eq!(normalised, "كم");
    }

    #[test]
    fn normalize_keeps_arabic_marks_when_tag_is_not_arabic() {
        // If someone (mis-)tags Arabic input as English, we
        // should NOT strip the tashkeel — that would be a silent
        // data-corruption hazard for callers who deliberately
        // pass `None` and want round-trip equality.
        let raw = "كَيْفَ";
        let normalised = normalize_for_lookup(raw, None);
        assert_eq!(normalised.chars().count(), raw.chars().count());
    }

    #[test]
    fn normalize_strips_bidi_and_zwj_for_any_language() {
        // Zero-width joiner U+200D, left-to-right mark U+200E.
        let raw = "hello\u{200D}world\u{200E}";
        let normalised = normalize_for_lookup(raw, Some("en"));
        assert_eq!(normalised, "helloworld");
    }

    #[test]
    fn normalize_nfc_composes_decomposed_input() {
        // qué as NFD: q + u + e + U+0301
        let nfd = "qu\u{0065}\u{0301}";
        let nfc = "qué";
        assert_eq!(
            normalize_for_lookup(nfd, Some("es")),
            normalize_for_lookup(nfc, Some("es"))
        );
    }

    #[test]
    fn normalize_lowercases() {
        assert_eq!(normalize_for_lookup("HELLO", Some("en")), "hello");
        assert_eq!(normalize_for_lookup("WAS", Some("de")), "was");
        // Cyrillic: capital Es Е → small es е.
        assert_eq!(
            normalize_for_lookup("РЕШЕНО", Some("ru")),
            normalize_for_lookup("решено", Some("ru"))
        );
    }

    #[test]
    fn first_alphabetic_token_skips_leading_punctuation() {
        assert_eq!(first_alphabetic_token("¿qué pasa?"), "qué");
        assert_eq!(first_alphabetic_token("  hello world"), "hello");
        assert_eq!(first_alphabetic_token(""), "");
        assert_eq!(first_alphabetic_token("!!!"), "");
    }

    #[test]
    fn first_alphabetic_bigram_returns_first_two_tokens() {
        assert_eq!(
            first_alphabetic_bigram("tại sao bạn"),
            Some("tại sao".to_string())
        );
        assert_eq!(
            first_alphabetic_bigram("por que sí"),
            Some("por que".to_string())
        );
        assert_eq!(first_alphabetic_bigram("hello"), None);
        assert_eq!(first_alphabetic_bigram(""), None);
    }

    // -----------------------------------------------------------------
    // table_matches behaviour
    // -----------------------------------------------------------------

    #[test]
    fn table_matches_first_token_exact_only() {
        let table = &["por", "qué"];
        // FirstToken: exact equality on the first alphabetic
        // token. `por favor` matches (first token `por` is in
        // the table), but `aporrear` does NOT (because `por` is
        // not a prefix of `aporrear` under the FirstToken rule).
        assert!(table_matches(table, "por favor", MatchStrategy::FirstToken));
        assert!(!table_matches(table, "aporrear", MatchStrategy::FirstToken));
    }

    #[test]
    fn table_matches_first_bigram_catches_collocations() {
        let table = &["por qué", "qué"];
        // First-token `por` is not in the table, but the
        // space-joined first-bigram `por qué` is, so FirstBigram
        // matches whereas FirstToken would not.
        assert!(!table_matches(
            table,
            "por qué pasa",
            MatchStrategy::FirstToken
        ));
        assert!(table_matches(
            table,
            "por qué pasa",
            MatchStrategy::FirstBigram
        ));
        // First-token `qué` (single-token entry in the table)
        // still matches under FirstBigram (the strategy ORs the
        // first-token + first-bigram checks).
        assert!(table_matches(table, "qué pasa", MatchStrategy::FirstBigram));
    }

    #[test]
    fn table_matches_substring_for_cjk() {
        let table = &["決定", "承認"];
        assert!(table_matches(
            table,
            "本日この件について決定しました",
            MatchStrategy::Substring,
        ));
        assert!(!table_matches(
            table,
            "本日この件について検討します",
            MatchStrategy::Substring,
        ));
    }

    // -----------------------------------------------------------------
    // Registry behaviour
    // -----------------------------------------------------------------

    #[test]
    fn default_registry_has_all_12_target_languages() {
        let reg = default_registry();
        for tag in [
            "en", "ja", "ko", "zh", "es", "fr", "de", "pt", "ar", "vi", "th", "id",
        ] {
            assert!(
                reg.lexicon_for(tag).is_some(),
                "default registry must contain lexicon for {tag}"
            );
        }
    }

    #[test]
    fn default_registry_supported_tags_matches_constant() {
        let reg = default_registry();
        let mut from_registry = reg.supported_tags();
        let mut from_const = SUPPORTED_LEXICON_TAGS.to_vec();
        from_registry.sort_unstable();
        from_const.sort_unstable();
        assert_eq!(from_registry, from_const);
    }

    #[test]
    fn lexicon_for_or_english_falls_back_to_english_on_unknown_tag() {
        let reg = default_registry();
        let lex = reg.lexicon_for_or_english(Some("xq"));
        assert_eq!(lex.primary_tag, "en");
        let lex = reg.lexicon_for_or_english(None);
        assert_eq!(lex.primary_tag, "en");
    }

    #[test]
    fn registry_interrogatives_delegate_to_phase_1_4_module() {
        let reg = default_registry();
        // English: FirstToken with `who`.
        let (en_list, en_strat) = reg.interrogatives_for("en").unwrap();
        assert!(en_list.contains(&"who"));
        assert_eq!(en_strat, MatchStrategy::FirstToken);
        // Japanese: Substring with `何`.
        let (ja_list, ja_strat) = reg.interrogatives_for("ja").unwrap();
        assert!(ja_list.contains(&"何"));
        assert_eq!(ja_strat, MatchStrategy::Substring);
    }

    // -----------------------------------------------------------------
    // Cross-language invariants
    // -----------------------------------------------------------------

    #[test]
    fn registry_covers_every_phase_1_4_interrogative_language() {
        // Every language in Phase 1.4's interrogative table
        // must ALSO appear in the Phase 1.1 registry, so the
        // per-sentence keyword matcher never finds itself
        // looking up decision/task keywords for a language it
        // can detect interrogatives for. This is the structural
        // invariant of Phase 1.1.
        use crate::interrogatives::SUPPORTED_PRIMARY_TAGS;
        let reg = default_registry();
        for tag in SUPPORTED_PRIMARY_TAGS {
            assert!(
                reg.lexicon_for(tag).is_some(),
                "Phase 1.4 supports interrogatives for {tag} but \
                 Phase 1.1 has no lexicon — add one to BUILTIN_LEXICONS"
            );
        }
    }

    #[test]
    fn no_keyword_contains_whitespace_unless_bigram_eligible() {
        // Substring strategies (CJK/Thai/Substring-mode Romance)
        // can contain whitespace (e.g. `por favor`, `お願い`),
        // because Substring match is just `contains`. FirstToken
        // strategies CANNOT contain whitespace — there is no way
        // for a whitespace-containing entry to equal a single
        // alphabetic token. FirstBigram entries MUST contain
        // exactly one ASCII space (and no other whitespace).
        let reg = default_registry();
        for lex in reg.iter() {
            for (class, strategy_label) in [
                (KeywordClass::Decision, "decision"),
                (KeywordClass::Task, "task"),
                (KeywordClass::TaskImperative, "task_imperative"),
                (KeywordClass::Stopword, "stopword"),
            ] {
                let Some((table, strategy)) = lex.entries(class) else {
                    continue;
                };
                for entry in table {
                    match strategy {
                        MatchStrategy::FirstToken => {
                            assert!(
                                !entry.chars().any(char::is_whitespace),
                                "{}/{strategy_label} entry {entry:?} is FirstToken but \
                                 contains whitespace (no token would ever equal it)",
                                lex.primary_tag,
                            );
                        }
                        MatchStrategy::FirstBigram => {
                            // FirstBigram is a strict superset of
                            // FirstToken: single-token entries are
                            // legal (they match via the first-token
                            // arm), bigram entries are legal (they
                            // match via the space-joined-bigram arm),
                            // and bigram entries MUST contain exactly
                            // one ASCII space and no other whitespace
                            // (the `first_alphabetic_bigram` helper
                            // joins with a single ASCII space).
                            let spaces: Vec<char> =
                                entry.chars().filter(|c| c.is_whitespace()).collect();
                            assert!(
                                spaces.is_empty() || spaces == [' '],
                                "{}/{strategy_label} FirstBigram entry {entry:?} must contain \
                                 zero or one ASCII space (and no other whitespace)",
                                lex.primary_tag,
                            );
                        }
                        // Substring entries are allowed any whitespace.
                        MatchStrategy::Substring => {}
                    }
                }
            }
        }
    }

    #[test]
    fn no_duplicate_keyword_within_a_class_within_a_language() {
        let reg = default_registry();
        for lex in reg.iter() {
            for (class, strategy_label) in [
                (KeywordClass::Decision, "decision"),
                (KeywordClass::Task, "task"),
                (KeywordClass::TaskImperative, "task_imperative"),
                (KeywordClass::Stopword, "stopword"),
            ] {
                let Some((table, _)) = lex.entries(class) else {
                    continue;
                };
                let mut seen: std::collections::HashSet<&&str> =
                    std::collections::HashSet::with_capacity(table.len());
                for entry in table {
                    assert!(
                        seen.insert(entry),
                        "{}/{strategy_label} table has duplicate entry {entry:?}",
                        lex.primary_tag,
                    );
                }
            }
        }
    }

    #[test]
    fn every_lexicon_has_at_least_one_decision_or_task_keyword() {
        // Sanity: a language with zero decision AND zero task
        // keywords adds no value (the only thing the extractor
        // could do for it is interrogatives, which are served
        // by `interrogatives.rs` directly). Force coverage.
        let reg = default_registry();
        for lex in reg.iter() {
            assert!(
                !lex.decision_keywords.is_empty() || !lex.task_keywords.is_empty(),
                "{} lexicon has neither decision nor task keywords",
                lex.primary_tag,
            );
        }
    }

    #[test]
    fn cjk_and_thai_lexicons_have_empty_imperative_verbs() {
        // The FirstToken imperative-verb path doesn't fire on
        // CJK / Thai (no alphabetic tokens), so those lexicons
        // must leave the imperative-verb list empty (any non-
        // empty list would be dead code).
        let reg = default_registry();
        for tag in ["ja", "ko", "zh", "th"] {
            let lex = reg.lexicon_for(tag).expect("configured");
            assert!(
                lex.task_imperative_verbs.is_empty(),
                "{tag} lexicon has imperative verbs but the matcher cannot fire on this script",
            );
        }
    }

    #[test]
    fn malay_and_indonesian_lexicons_are_identical() {
        // Documented design choice (see ID_LEXICON doc): until
        // Phase 2 differentiates them, ms aliases id. Pin so
        // accidental drift fails the test rather than silently
        // diverging the two lexicons.
        let reg = default_registry();
        let id = reg.lexicon_for("id").unwrap();
        let ms = reg.lexicon_for("ms").unwrap();
        assert_eq!(id.decision_keywords, ms.decision_keywords);
        assert_eq!(id.task_keywords, ms.task_keywords);
        assert_eq!(id.task_imperative_verbs, ms.task_imperative_verbs);
        assert_eq!(id.stop_words, ms.stop_words);
    }

    #[test]
    fn from_static_panics_without_english_fallback() {
        // The English fallback is part of the matcher contract.
        // Build a registry without `en` and confirm construction
        // panics rather than silently producing a registry that
        // would later panic at lookup time with a confusing
        // error.
        let result = std::panic::catch_unwind(|| {
            static NO_EN: &[LanguageLexicon] = &[JA_LEXICON];
            LexiconRegistry::from_static(NO_EN);
        });
        assert!(result.is_err(), "from_static must panic without en");
    }
}
