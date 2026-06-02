//! Multilingual `LexiconRegistry`.
//!
//! Earlier in the multilingual work the substrate gained
//! per-message + per-sentence language detection and
//! language-aware question detection (via
//! [`crate::interrogatives`]). This module is the structural
//! follow-on: a single typed registry that owns the
//! per-BCP-47-primary-subtag keyword tables for **all** lexicon
//! classes — decisions, tasks, task-imperative verbs, stop-words,
//! and (by delegation) interrogatives — together with the shared
//! normalisation primitive that callers use to compare keyword
//! tables against running text in a script-aware way.
//!
//! ## Why a registry, and what it replaces
//!
//! Before this registry, [`crate::extractor::LexiconExtractor`]
//! carried a single set of English decision / task / imperative
//! / stop-word lists on its struct (the only built-in
//! `english_default` set). Per-sentence language detection was
//! already wired in, but the sentence-level matcher still
//! consulted the same English keyword lists no matter what
//! language the sentence was detected as. That worked for the
//! per-sentence [`crate::interrogatives`] lookup (which IS
//! per-language) but left every non-English decision / task
//! sentence silently falling through.
//!
//! [`LexiconRegistry`] fixes that: it is a lookup-by-BCP-47-
//! primary-subtag map of [`LanguageLexicon`]s, each of which
//! bundles the keyword tables for every observation class. The
//! extractor resolves the right lexicon per sentence (using the
//! sentence's detected language, falling back to English when
//! detection produced `None` or the language has no configured
//! lexicon).
//!
//! ## Cross-cutting primitives this module closes
//!
//! * **NFC + locale-lowercase primitive.** An earlier question
//!   matcher applied NFC + lowercase ad-hoc inside
//!   [`crate::extractor::looks_like_question`] but the decision
//!   / task paths still used plain `to_lowercase`. That was safe
//!   only as long as those tables stayed ASCII. This module
//!   ships [`normalize_for_lookup`] as the single normalisation
//!   primitive every classifier path now uses, so Romance /
//!   Cyrillic / Arabic decision and task keywords match
//!   independently of the input's Unicode normalisation form.
//! * **Tashkeel-tolerant Arabic tokeniser.** Arabic running text
//!   often carries non-spacing combining marks (fatha, kasra,
//!   damma, sukun, shadda, …) and the elongation glyph *tatweel*
//!   (U+0640). The earlier FirstToken splitter broke on tashkeel
//!   because tashkeel codepoints are category `Mn`
//!   (non-alphabetic), which split tokens internally. The
//!   normalisation primitive in this module **strips** the
//!   Arabic combining marks and tatweel **before** lowercasing
//!   so that a tashkeel-decorated `كَيْفَ` matches the table
//!   entry `كيف`.
//! * **Bigram-prefix matching.** Several languages form question
//!   / decision / task openers from multi-word collocations:
//!   Vietnamese `tại sao` ("why"), `khi nào` ("when"),
//!   `làm sao` ("how"); French `est-ce que`; Arabic
//!   `هل ال…` (yes/no opener that binds to the following
//!   definite article); Portuguese `por que`. The FirstToken
//!   strategy can't see these collocations and the Substring
//!   strategy is too loose for space-separated scripts. This
//!   module adds [`MatchStrategy::FirstBigram`], which compares
//!   the space-joined first two alphabetic tokens against the
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
/// strategy (used by question detection) with the new
/// [`MatchStrategy::FirstBigram`] strategy to
/// cover multi-word collocations that the FirstToken /
/// Substring strategies can't express cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchStrategy {
    /// The first alphabetic token of the normalised sentence
    /// must exactly equal an entry in the table. Suitable for
    /// space-separated languages where the question / decision
    /// / task opener is canonically sentence-initial (English,
    /// German, Romance languages, Vietnamese, Indonesian). Arabic
    /// used FirstToken before but now uses
    /// [`MatchStrategy::FirstTokenWithArabicClitics`] so the
    /// proclitic prefix forms (`وكيف` = `و`+`كيف`,
    /// `فمتى` = `ف`+`متى`, `بأي` = `ب`+`أي`,
    /// `لمن` = `ل`+`من`, `واكتب` = `و`+`اكتب`) are recovered.
    /// `ك` ("like/as") and `س` (future marker) were initially
    /// in the peel set but later removed after they surfaced
    /// a false-positive on short interrogatives (`كمن` ➜ `من`,
    /// `سما` ➜ `ما`) AND a worse false-positive on the imperative
    /// path (`سأرسل` "I will send" ➜ `أرسل` imperative table
    /// entry) — both excluded after a follow-up; see the inventory
    /// comment on [`ARABIC_PROCLITIC_PREFIXES`].
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
    /// scripts where boundary-based token comparison doesn't
    /// apply — either because the script lacks inter-word
    /// whitespace (CJK Han, Thai, Lao, Khmer, Myanmar,
    /// Tibetan) or because the script uses non-alphabetic
    /// combining marks (Devanagari virama `U+094D`,
    /// Tibetan tsheg `U+0F0B`, Khmer coeng `U+17D2`, Myanmar
    /// virama/asat `U+1039`/`U+103A`) that `unicode61` treats
    /// as token boundaries, fragmenting any meaningful
    /// keyword. Hindi and the four
    /// lexicons (Tibetan / Khmer / Myanmar / Lao)
    /// all use this strategy. Adding any future script that
    /// shares either property (e.g. Tai Tham, Javanese,
    /// Cham) should default to `Substring` unless the script
    /// is provably whitespace-segmented at the word level.
    Substring,
    /// strategy for Arabic-script languages whose
    /// morphology agglutinates short proclitic particles to the
    /// front of the host word with no orthographic separator.
    /// The matcher tries first-token exact equality first; if
    /// that fails, it iteratively peels the recognised Arabic
    /// proclitic prefixes (the 1-character conjunction `و`
    /// "and", the 1-character connector `ف` "then", the
    /// 1-character prepositions `ب` "with/by" / `ل` "to/for",
    /// and the 2-character definite article `ال` / `أل` "the")
    /// from the front of the token and re-checks exact equality
    /// after each peel. Up to [`ARABIC_PROCLITIC_PEEL_BUDGET`]
    /// peels are attempted to bound worst-case cost on
    /// adversarial input; in practice 2 peels covers the
    /// realistic stack (e.g. `وللكتاب` = `و` + `ل` + `ل` +
    /// `كتاب`, which only needs to surface `كتاب` for downstream
    /// classes — interrogatives almost never stack more than one
    /// proclitic on top of `ال`).
    ///
    /// **Why not peel `ك` "like/as" and `س` "will" (later removed
    /// from the peel set)?** A follow-up precision audit noted that both
    /// could collide with short interrogatives (`كمن` peels `ك`
    /// to surface `من` "who"; `سما` peels `س` to surface `ما`
    /// "what"), and an internal audit then surfaced a far more
    /// dangerous interaction on the imperative path: `س` is the
    /// 1st-person-future marker, so `سأرسل` ("I will send" —
    /// 1st-person future verb, NOT an imperative) would peel to
    /// surface `أرسل` (which IS in the imperative table), giving
    /// a real false Task observation on plain declarative
    /// future-tense statements. The same risk exists for
    /// `سأكتب` ➜ `اكتب`, `سأصلح` ➜ `أصلح`, etc. — every
    /// `أ`-initial imperative has a 1st-person future counterpart
    /// that `س` peeling would conflate. Excluding `ك` / `س`
    /// from the peel set is a strict precision win with zero
    /// recall loss on the production tables: `ك` attaches
    /// predominantly to nouns (handled via Substring on
    /// [`LanguageLexicon::decision_strategy`] /
    /// [`LanguageLexicon::task_strategy`], which don't need
    /// prefix peeling), and `س` attaches predominantly to
    /// verbs but never to imperatives by morphological
    /// construction.
    ///
    /// **Why not `Substring`?** Arabic morphology is dense and
    /// short interrogatives (`من` "who", `ما` "what",
    /// `أي` "which") collide with arbitrary in-word substrings
    /// (`مَن` shares its three letters with `أمن` "safety",
    /// `يمن` "Yemen", `زمن` "time"; `ما` collides with
    /// `كما` "as", `لما` "because", `أما` "as for"). Bounded
    /// prefix peeling gives the proclitic recall we need without
    /// the per-letter false-positive blast radius that
    /// `Substring` would carry.
    ///
    /// **Why not strip `أ` (interrogative hamza)?** The
    /// single-character interrogative hamza `أ` shares its
    /// orthography with the prosthetic / radical hamza on a
    /// large open class of common Arabic nouns and pronouns
    /// (`أنا` "I", `أنت` "you-masc", `أب` "father",
    /// `أم` "mother", `أحمد` proper-name, `أخ` "brother", …)
    /// so peeling it would cause systematic over-classification
    /// of declarative sentences as questions. The Arabic
    /// interrogative table omits `أ` for exactly this reason
    /// (see the dedicated-omission comment on the Arabic
    /// interrogative table in
    /// [`crate::interrogatives::interrogatives_for`]).
    /// Yes/no questions with the hamza particle are recovered
    /// instead via the `؟` terminator short-circuit in
    /// [`crate::extractor::looks_like_question`].
    FirstTokenWithArabicClitics,
    /// strategy for Hebrew, whose morphology
    /// agglutinates short single-letter proclitics to the front
    /// of the host word with no orthographic separator — the
    /// same shape as Arabic's productive proclitic stack but
    /// with a Hebrew-specific peel inventory (`ו` "and",
    /// `ש` "that / which", `מ` "from", `ל` "to / for",
    /// `ב` "in / at / with"). The matcher tries first-token
    /// exact equality first; if that fails, it iteratively peels
    /// the recognised Hebrew proclitic prefixes (see
    /// [`HEBREW_PROCLITIC_PREFIXES`]) and re-checks exact
    /// equality after each peel. Up to
    /// [`HEBREW_PROCLITIC_PEEL_BUDGET`] peels are attempted to
    /// bound worst-case cost on adversarial input; in practice
    /// 2-3 peels covers the realistic stack (e.g.
    /// `ושבכיתה` = `ו` + `ש` + `ב` + `כיתה` = "and that in the
    /// classroom").
    ///
    /// **Why a Hebrew-specific peel set rather than reusing
    /// [`MatchStrategy::FirstTokenWithArabicClitics`]?** The
    /// Arabic peel inventory references Arabic-only orthography:
    /// the 2-character definite article `ال` / `أل` does not
    /// exist in Hebrew (Hebrew's definite article is the single
    /// letter `ה`), and the Arabic conjunction/connector `ف` is
    /// not a Hebrew letter. Conversely, the Hebrew relative
    /// pronoun `ש` and the preposition `מ` are not productive
    /// Arabic proclitics. Sharing one peel inventory across the
    /// two languages would cause silent no-ops in the best case
    /// and over-peeling false positives in the worst case. The
    /// per-language strategy is the architectural lockstep with
    /// the per-language peel-inventory constant — see
    /// `first_token_with_arabic_clitics_languages_are_arabic_only_for_now`
    /// and the matching
    /// `first_token_with_hebrew_clitics_languages_are_hebrew_only_for_now`
    /// exclusivity test.
    ///
    /// **Why not peel `ה` (definite article) or `כ` (preposition
    /// "like / as")?** Both are excluded for precision reasons
    /// documented in detail on [`HEBREW_PROCLITIC_PREFIXES`].
    /// Briefly: `ה`-peeling would conflate passive participles
    /// (`הכתוב` "the written one") with imperatives (`כתוב`
    /// "write!") on the imperative path, producing a real Task
    /// false positive on declarative sentences that mention a
    /// written / sent / scheduled / etc. document. `כ`-peeling
    /// mirrors the Arabic-side exclusion of `ك` — short
    /// 1-character prepositions whose recall gain is limited
    /// while their false-positive blast radius on the verb table
    /// is real.
    ///
    /// **Why not `Substring`?** Hebrew morphology is dense and
    /// short interrogatives (`מי` "who" — 2 chars, `מה` "what" —
    /// 2 chars) collide with arbitrary in-word substrings
    /// (`מי` ⊂ `מים` "water" / `מילה` "word"; `מה` ⊂ `המה`
    /// "they" / `מהר` "fast"). Bounded prefix peeling gives the
    /// proclitic recall we need without the per-letter
    /// false-positive blast radius that `Substring` would carry.
    FirstTokenWithHebrewClitics,
}

impl MatchStrategy {
    /// Bridge from the interrogative-strategy enum to
    /// the registry strategy enum. Used by
    /// [`LexiconRegistry::interrogatives_for`] to expose the
    /// per-language interrogative matcher through the unified
    /// [`table_matches`] entry point.
    ///
    /// The mapping also routes
    /// [`InterrogativeMatch::FirstBigram`] (Vietnamese) to
    /// [`MatchStrategy::FirstBigram`] so the Vietnamese
    /// bigram interrogatives (`tại sao`, `khi nào`, `vì sao`)
    /// reach the matcher, and routes
    /// [`InterrogativeMatch::FirstTokenWithHebrewClitics`]
    /// (Hebrew) to the matching registry strategy so the
    /// Hebrew clitic-stacked interrogatives (`ומתי`, `שמה`,
    /// `מאיזה`) reach the matcher.
    pub fn from_interrogative_match(strategy: InterrogativeMatch) -> Self {
        match strategy {
            InterrogativeMatch::FirstToken => MatchStrategy::FirstToken,
            InterrogativeMatch::FirstBigram => MatchStrategy::FirstBigram,
            InterrogativeMatch::Substring => MatchStrategy::Substring,
            InterrogativeMatch::FirstTokenWithArabicClitics => {
                MatchStrategy::FirstTokenWithArabicClitics
            }
            InterrogativeMatch::FirstTokenWithHebrewClitics => {
                MatchStrategy::FirstTokenWithHebrewClitics
            }
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
    /// [`KeywordClass::TaskImperative`] using
    /// [`Self::task_imperative_strategy`].
    ///
    /// Bigram entries (for the [`MatchStrategy::FirstBigram`]
    /// arm) must be written with a single ASCII space
    /// separating the two alphabetic tokens; see
    /// [`first_alphabetic_bigram`](crate::lexicon::first_alphabetic_bigram).
    pub task_imperative_verbs: &'static [&'static str],
    /// Strategy for matching [`Self::task_imperative_verbs`].
    ///
    /// Most languages use [`MatchStrategy::FirstBigram`] (a
    /// strict superset of [`MatchStrategy::FirstToken`]: single-
    /// word imperatives still match via the first-token arm,
    /// while multi-syllable imperatives such as Vietnamese
    /// `triển khai` / `chuẩn bị` / `cập nhật` match via the
    /// bigram arm). Languages with scripts that put
    /// non-alphabetic combining marks *inside* the imperative
    /// verb — notably Devanagari (Hindi) with the virama
    /// `U+094D` (Category Mn) embedded in `मर्ज` / `समीक्षा` /
    /// `प्रकाशित` / `अद्यतन` — must use
    /// [`MatchStrategy::Substring`] instead, because the
    /// alphabetic-tokeniser ([`alphabetic_tokens`]) splits at
    /// every non-alphabetic char and would never produce the
    /// virama-spanning token. Per-language override structurally
    /// prevents the unreachable-entry class of bug for future
    /// languages —
    pub task_imperative_strategy: MatchStrategy,
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
    /// avoid duplicating the data).
    pub fn entries(&self, class: KeywordClass) -> Option<(&'static [&'static str], MatchStrategy)> {
        match class {
            KeywordClass::Decision => Some((self.decision_keywords, self.decision_strategy)),
            KeywordClass::Task => Some((self.task_keywords, self.task_strategy)),
            // TaskImperative now uses the per-language
            // `task_imperative_strategy` field. Most languages
            // set this to `FirstBigram` (a strict superset of
            // `FirstToken`: it tries `FirstToken` first, then
            // the bigram), so single-word imperative verbs in
            // en / es / fr / de / pt / it / ru still match via
            // the FirstToken arm, while Vietnamese-style
            // multi-syllable imperative verbs (`triển khai`,
            // `chuẩn bị`, `cập nhật`) match via the bigram arm.
            // Devanagari (Hindi) sets this to `Substring`
            // because the virama `U+094D` is non-alphabetic and
            // splits intra-word imperatives like `मर्ज` /
            // `समीक्षा` that no first-token / first-bigram check
            // could ever reassemble.
            KeywordClass::TaskImperative => {
                Some((self.task_imperative_verbs, self.task_imperative_strategy))
            }
            // Stopwords AND Interrogatives both return `None`
            // here, but for different reasons documented per-
            // class below:
            //
            // * `Stopword`: the capitalised-token entity
            //   extractor checks each candidate directly via
            //   [`crate::extractor::LexiconExtractor::is_stop_word`],
            //   which uses Unicode-aware [`str::to_lowercase`]
            //   equality. Stop-words are deliberately NOT
            //   routed through `table_matches` /
            //   `sentence_matches_class` because the
            //   sentence-shaped matcher's semantics
            //   (`FirstToken` against the SENTENCE START)
            //   don't match what stop-word filtering needs
            //   (per-candidate equality, anywhere in text).
            //   Returning `None` here makes that contract
            //   explicit at the type level so a future caller
            //   can't accidentally route stop-words through
            //   the wrong matcher (per the prior
            //   guidance).
            // * `Interrogative`: served by
            //   [`LexiconRegistry::interrogatives_for`] —
            //   the interrogative tables live in
            //   [`crate::interrogatives`] for historical
            //   reasons and to avoid duplicating the //
            // 1.4 data on the [`LanguageLexicon`] struct.
            KeywordClass::Stopword | KeywordClass::Interrogative => None,
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
    ///
    /// Side effect: bumps the
    /// [`crate::lexicon_telemetry`] `hits_<tag>` counter for the
    /// resolved lexicon's `primary_tag`, and additionally bumps
    /// [`crate::lexicon_telemetry::LexiconTelemetrySnapshot::
    /// unknown_tag_fallbacks_total`] when an input
    /// `primary_tag = Some(t)` failed the lookup and fell back
    /// to English. See [`crate::lexicon_telemetry::
    /// record_lexicon_hit`] for the counter semantics.
    pub fn lexicon_for_or_english(&self, primary_tag: Option<&str>) -> &'static LanguageLexicon {
        let resolved = primary_tag
            .and_then(|t| self.lexicon_for(t))
            .or_else(|| self.lexicon_for("en"))
            .expect("English fallback lexicon must exist in registry");
        crate::lexicon_telemetry::record_lexicon_hit(primary_tag, resolved.primary_tag);
        resolved
    }

    /// Look up the interrogative table + matching strategy for
    /// a BCP-47 primary subtag. Delegates to the
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
/// fully deterministic.
pub fn is_arabic_combining_or_tatweel(c: char) -> bool {
    matches!(c,
        '\u{0610}'..='\u{061A}'
        | '\u{064B}'..='\u{065F}'
        | '\u{0670}'
        | '\u{06D6}'..='\u{06ED}'
        | '\u{0640}'
    )
}

/// True for Hebrew niqqud (vowel pointing) and cantillation
/// (te'amim) marks. Counterpart to
/// [`is_arabic_combining_or_tatweel`].
///
/// Ranges from the Unicode 15 Hebrew block:
///
/// * `U+0591..=U+05AF` — Hebrew cantillation marks
///   (`te'amim`, used in liturgical / Biblical text to mark
///   recitation melody and prosodic structure).
/// * `U+05B0..=U+05BD` — Hebrew points (`niqqud`: sheva,
///   hataf-segol, hataf-patah, hataf-qamats, hiriq, tsere,
///   segol, patah, qamats, holam, holam-haser, qubuts, dagesh,
///   meteg). Category `Mn`.
/// * `U+05BF` — Hebrew point rafe. Category `Mn`.
/// * `U+05C1..=U+05C2` — Hebrew shin / sin dot. Category `Mn`.
/// * `U+05C4..=U+05C5` — Hebrew mark upper / lower dot.
///   Category `Mn`.
/// * `U+05C7` — Hebrew point qamats qatan. Category `Mn`.
///
/// These codepoints are stripped during
/// [`normalize_for_lookup`] when the language tag is `"he"`
/// (or any other Hebrew-script primary subtag we add later)
/// so that a niqqud-decorated word like `מָתַי` matches the
/// canonical table entry `מתי`. Without the strip, the
/// FirstToken splitter (which treats non-alphabetic
/// codepoints as token boundaries — `Mn` is not alphabetic
/// per [`char::is_alphabetic`]) would break the word into
/// single-letter pieces that never match.
///
/// We strip rather than NFKD-decompose for the same reason as
/// Arabic (see [`is_arabic_combining_or_tatweel`]): NFKD
/// would also touch unrelated codepoints (e.g. fullwidth
/// Latin), but strip-by-range is targeted and fully
/// deterministic. The maqaf (`U+05BE`, Hebrew hyphen — used
/// to join words orthographically) is **not** stripped:
/// maqaf is `Po` (punctuation), behaves as a hard tokeniser
/// boundary by design, and stripping it would conflate
/// orthographically-distinct phrases.
pub fn is_hebrew_combining(c: char) -> bool {
    matches!(c,
        '\u{0591}'..='\u{05AF}'
        | '\u{05B0}'..='\u{05BD}'
        | '\u{05BF}'
        | '\u{05C1}'..='\u{05C2}'
        | '\u{05C4}'..='\u{05C5}'
        | '\u{05C7}'
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
/// do — likely .x — we'll pull in `icu_casemap` (or
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
        } else if is_hebrew_script_primary_tag(tag) {
            // strip niqqud + cantillation marks so
            // that pointed Hebrew text (`מָתַי`) matches the
            // unpointed canonical table entries (`מתי`). Without
            // this strip, the FirstToken splitter would break
            // pointed words into single-letter fragments at every
            // combining mark, because the marks are category `Mn`
            // (not alphabetic per `char::is_alphabetic`).
            buf = buf.chars().filter(|c| !is_hebrew_combining(*c)).collect();
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

/// True for BCP-47 primary subtags whose canonical script is
/// the Hebrew abjad (and which therefore need niqqud +
/// cantillation marks stripped during normalisation).
/// Conservative list — only `he` (Modern Hebrew) ships a
/// lexicon today. Yiddish (`yi`) and Ladino (`lad`) also use
/// the Hebrew alphabet but have language-specific spelling
/// conventions (Yiddish uses `ייִ`/`וֹ` digraphs with different
/// vowel semantics; Ladino uses different niqqud placement)
/// and would benefit from their own peel inventories; they're
/// excluded from this predicate until lexicons land for them.
fn is_hebrew_script_primary_tag(primary_tag: &str) -> bool {
    matches!(primary_tag, "he")
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
/// This is the unified matcher used by both the
/// question detection path (via the registry's interrogative
/// lookup) and the new decision / task / imperative
/// paths. Bigram entries are written with a single ASCII space
/// and checked against the space-joined first two alphabetic
/// tokens (see [`first_alphabetic_bigram`]).
pub fn table_matches(table: &[&str], normalised: &str, strategy: MatchStrategy) -> bool {
    // Telemetry: every invocation bumps the per-strategy counter
    // (regardless of whether the call ultimately returns `true`
    // or `false`). The counter measures *strategy fires* — how
    // often each variant is consulted on the hot path — which is
    // the right signal for tuning lexicon / extractor logic. If
    // future need ever arises to separate "fired AND matched"
    // from "fired AND missed", a sibling pair of counters can be
    // added without touching this site.
    crate::lexicon_telemetry::record_match_strategy_fire(strategy);
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
        MatchStrategy::FirstTokenWithArabicClitics => {
            let first = first_alphabetic_token(normalised);
            first_token_matches_after_arabic_clitic_strip(table, first)
        }
        MatchStrategy::FirstTokenWithHebrewClitics => {
            let first = first_alphabetic_token(normalised);
            first_token_matches_after_hebrew_clitic_strip(table, first)
        }
    }
}

/// Arabic proclitic prefixes that the
/// [`MatchStrategy::FirstTokenWithArabicClitics`] matcher will
/// peel from the front of the first alphabetic token. Listed
/// **longest-first** so the iterative peeler tries the
/// definite-article forms before the 1-character particles
/// (peeling `ال` from `الكتاب` produces `كتاب`; peeling the
/// 1-character `ا` from `الكتاب` would produce the meaningless
/// `لكتاب` and could mask a genuine match further down).
///
/// **Source** for the inventory: Ryding, *A Reference Grammar of
/// Modern Standard Arabic* (Cambridge, 2005), §10.1
/// ("Proclitics — short particles that attach to the next word
/// with no orthographic separator"). The five recognised
/// proclitics (`ال` / `أل` counted as one definite article with
/// two spelling variants, plus the four 1-character productive
/// particles `و` / `ف` / `ب` / `ل`) cover every proclitic in
/// MSA news / docs / formal IM register that the substrate's
/// lexicons target, *minus* the two surfaces (`ك`, `س`)
/// excluded for precision reasons documented on
/// [`MatchStrategy::FirstTokenWithArabicClitics`].
///
/// Three additional Arabic proclitics from the linguistic
/// inventory are **deliberately omitted** from this set, each
/// for a different reason; the omissions are pinned by tests
/// so a future contributor can't silently add them back:
///
/// * **`أ`** (interrogative hamza, 1 char) — would over-
///   classify the open class of `أ`-initial declaratives as
///   questions (`أنا` "I", `أحمد` proper-name, …). Yes/no
///   questions with the hamza particle are recovered instead
///   via the `؟` terminator short-circuit in
///   [`crate::extractor::looks_like_question`].
/// * **`ك`** (preposition "like / as", 1 char) — collides with
///   short interrogatives (`كمن` ➜ `من` "who") and with the
///   imperative table (`ك`-prefixing never produces a real
///   imperative form in MSA but the peel could spuriously
///   surface one). `ك` attaches predominantly to nouns, which
///   are handled via Substring on
///   [`LanguageLexicon::decision_strategy`] /
///   [`LanguageLexicon::task_strategy`] without needing prefix
///   peeling.
/// * **`س`** (future marker "will", 1 char) — every `أ`-initial
///   imperative in the AR table (`أرسل`, `أصلح`, …) has a
///   1st-person future counterpart (`سأرسل`, `سأصلح`, …)
///   that `س` peeling would conflate with the imperative. The
///   future marker also never attaches to interrogatives in
///   MSA. The trade-off (zero realistic recall loss against
///   real false-positive risk on declarative future-tense
///   statements) makes exclusion the conservative default.
const ARABIC_PROCLITIC_PREFIXES: &[&str] = &[
    // 2-character definite-article forms (longest first).
    "ال", // alif-lam — canonical definite article (NFC form).
    "أل", // hamza-on-alif + lam — common spelling variant.
    // 1-character productive proclitic particles.
    "و", // conjunction "and".
    "ف", // connector "then / so".
    "ب", // preposition "with / by / in".
    "ل", // preposition "to / for".
         // NOTE: `ك` (preposition "like / as") and `س` (future
         // marker "will") are deliberately NOT in this list; see
         // the docstring above for the precision rationale, and
         // `table_matches_arabic_clitic_strip_drops_unproductive_k_and_s_prefixes`
         // for the regression tests that pin the omissions.
];

/// Worst-case number of proclitic peels the Arabic clitic-aware
/// matcher will attempt on a single token before giving up.
///
/// Three peels covers the realistic stack-depth in MSA — e.g.
/// `وللكتاب` is `و` + `ل` + `ل` + `كتاب` (3 peels to surface
/// the bare noun), and `وبالكتاب` is `و` + `ب` + `ال` +
/// `كتاب` (3 peels). MSA does not stack more than three
/// productive proclitics in front of a single host word, so this
/// bound is generous against realistic corpora while still
/// putting a hard ceiling on adversarial inputs (a pathological
/// token like `وووووووووو...كتاب` exits after 3 peels of `و`
/// without exhausting the iterator).
pub const ARABIC_PROCLITIC_PEEL_BUDGET: usize = 3;

/// Helper for [`MatchStrategy::FirstTokenWithArabicClitics`].
///
/// Returns `true` when `first` exact-matches an entry in `table`
/// either directly OR after up to
/// [`ARABIC_PROCLITIC_PEEL_BUDGET`] iterative peels of the
/// recognised Arabic proclitic prefixes (see
/// [`ARABIC_PROCLITIC_PREFIXES`]).
///
/// The peeler is greedy and longest-first: at each step it
/// tries each prefix in [`ARABIC_PROCLITIC_PREFIXES`] order and
/// takes the first one that strips off non-empty alphabetic
/// residual. A residual that collapses to empty (e.g. `و` alone
/// stripped to `""`) is rejected — there is no host word left
/// to match against the table.
fn first_token_matches_after_arabic_clitic_strip(table: &[&str], first: &str) -> bool {
    // Empty input is a guard against a routing bug — bail
    // without any telemetry side effect (an empty first token
    // is NOT a real "peel attempt").
    if first.is_empty() {
        return false;
    }
    let outcome = arabic_clitic_peel_outcome(table, first);
    crate::lexicon_telemetry::record_arabic_peel_depth(outcome);
    matches!(
        outcome,
        crate::lexicon_telemetry::PeelOutcome::MatchedAtDepth(_)
    )
}

/// Compute the peel-depth outcome for an Arabic clitic-aware
/// table check. Separated from
/// [`first_token_matches_after_arabic_clitic_strip`] so that the
/// telemetry counter increment lives at exactly one site, and
/// the depth value is unambiguous (depth 0 = matched without
/// peeling, depth 1..=`ARABIC_PROCLITIC_PEEL_BUDGET` = matched
/// after that many peels, `BudgetExhausted` = budget consumed
/// without a match OR no more stripable proclitics left).
///
/// The "no more stripable proclitics" sub-case (peel_one returns
/// `None` mid-loop) is folded into `BudgetExhausted` rather than
/// given its own bucket — both share the semantic "matcher gave
/// up without finding a table entry", and the count of such
/// cases combined with the `MatchedAtDepth(N)` distribution is
/// already enough to diagnose routing problems (high exhausted
/// rate => non-Arabic tokens reaching the matcher).
fn arabic_clitic_peel_outcome(
    table: &[&str],
    first: &str,
) -> crate::lexicon_telemetry::PeelOutcome {
    use crate::lexicon_telemetry::PeelOutcome;
    debug_assert!(!first.is_empty(), "caller must filter empty `first`");
    if table.contains(&first) {
        return PeelOutcome::MatchedAtDepth(0);
    }
    let mut current = first;
    for depth in 1..=ARABIC_PROCLITIC_PEEL_BUDGET {
        let Some(stripped) = peel_one_arabic_proclitic(current) else {
            return PeelOutcome::BudgetExhausted;
        };
        if table.contains(&stripped) {
            // depth is in 1..=ARABIC_PROCLITIC_PEEL_BUDGET (== 3
            // today), which fits in u8 trivially.
            return PeelOutcome::MatchedAtDepth(
                u8::try_from(depth).expect("peel budget fits in u8"),
            );
        }
        current = stripped;
    }
    PeelOutcome::BudgetExhausted
}

/// Attempt one greedy longest-first peel of the recognised
/// Arabic proclitic prefixes (see [`ARABIC_PROCLITIC_PREFIXES`])
/// from the front of `token`.
///
/// Returns `Some(residual)` when a prefix was successfully
/// peeled AND the residual is non-empty (we never propagate an
/// empty-string "match"). Returns `None` when no recognised
/// prefix matched OR when the only matching prefix would leave
/// an empty residual.
fn peel_one_arabic_proclitic(token: &str) -> Option<&str> {
    for prefix in ARABIC_PROCLITIC_PREFIXES {
        if let Some(rest) = token.strip_prefix(prefix) {
            if !rest.is_empty() {
                return Some(rest);
            }
        }
    }
    None
}

/// Hebrew proclitic prefixes that the
/// [`MatchStrategy::FirstTokenWithHebrewClitics`] matcher will
/// peel from the front of the first alphabetic token.
///
/// **Source** for the inventory: Glinert, *Modern Hebrew: An
/// Essential Grammar* (Routledge, 4th ed. 2005), §3.5
/// ("Bound prefixes — the 'attaching' particles"). Modern
/// Hebrew has a small productive inventory of single-letter
/// proclitics that attach to the next word with no
/// orthographic separator and which can stack (e.g.
/// `ושבכיתה` = `ו` "and" + `ש` "that" + `ב` "in" + `כיתה`
/// "classroom"). The five recognised proclitics below
/// (`ו` conjunction, `ש` relative pronoun, `מ` preposition
/// "from", `ל` preposition "to / for", `ב` preposition
/// "in / at / with") cover every productive proclitic in
/// modern Hebrew news / docs / formal IM register that the
/// substrate's lexicons target, *minus* the two surfaces
/// (`ה`, `כ`) excluded for precision reasons documented below.
///
/// Unlike Arabic, **every Hebrew proclitic is exactly one
/// codepoint** — Hebrew has no 2-character article analogous
/// to Arabic `ال` / `أل`. The peel loop therefore strips one
/// codepoint per iteration in the worst case.
///
/// Two additional Hebrew proclitics from the linguistic
/// inventory are **deliberately omitted** from this set, each
/// for a different reason; the omissions are pinned by tests
/// so a future contributor can't silently add them back:
///
/// * **`ה`** (definite article "the", 1 char) — would conflate
///   passive participles with imperatives on the imperative
///   path. Hebrew's masculine-singular imperative for the most
///   common verbs (`כתוב` "write!", `שלח` "send!", `סקור`
///   "review!", `פרסם` "publish!", `הצב` "deploy!") is
///   morphologically a single string, but the same trilateral
///   root forms a passive participle that takes the article
///   `ה` in NP context (`הכתוב במסמך` "the (one) written in
///   the document"). Peeling `ה` from `הכתוב` would surface
///   the imperative table entry `כתוב` and emit a false Task
///   observation on a plain declarative noun phrase. The
///   article also doesn't attach to interrogatives in Hebrew
///   (interrogatives are NPs themselves, not arguments of an
///   NP head), so the recall gain on the interrogative path
///   is zero. Net: real false-positive risk, zero realistic
///   recall benefit — exclude.
/// * **`כ`** (preposition "like / as", 1 char) — mirrors the
///   Arabic-side exclusion of `ك` (see the docstring on
///   [`ARABIC_PROCLITIC_PREFIXES`]): a 1-character preposition
///   attached predominantly to nouns, which are matched via
///   `Substring` on the decision / task classes. Peeling `כ`
///   from common Hebrew openers (`כתב לי שלום` "he wrote me
///   hello" — first token `כתב` past-tense of "write") could
///   surface `תב` (not a word, safe) but also from `כסף`
///   "money" → `סף`, `כלב` "dog" → `לב`, neither a word in
///   the table so the peel is also safe in practice — BUT
///   the recall gain is essentially nil (no productive Hebrew
///   `כ`-prefixed interrogative; the imperative table is also
///   not normally prefixed with `כ`). Conservative default:
///   exclude.
const HEBREW_PROCLITIC_PREFIXES: &[&str] = &[
    "ו", // conjunction "and".
    "ש", // relative pronoun / complementiser "that / which".
    "מ", // preposition "from".
    "ל", // preposition "to / for".
    "ב", // preposition "in / at / with".
         // NOTE: `ה` (definite article) and `כ` (preposition
         // "like / as") are deliberately NOT in this list; see
         // the docstring above for the precision rationale, and
         // `table_matches_hebrew_clitic_strip_drops_unproductive_h_and_k_prefixes`
         // for the regression tests that pin the omissions.
];

/// Worst-case number of proclitic peels the Hebrew clitic-aware
/// matcher will attempt on a single token before giving up.
///
/// Three peels covers the realistic stack-depth in modern
/// Hebrew — the typical productive stacks are `ש` + `ה` +
/// nominal (`שהילד` "that the child"), `ו` + `ש` + `ה`
/// nominal (`ושהילד` "and that the child"), and `ו` + `ש` +
/// `ב` nominal (`ושבכיתה` "and that in the classroom"). With
/// `ה` excluded from our peel inventory (see the
/// [`HEBREW_PROCLITIC_PREFIXES`] docstring) the realistic
/// stack reduces further to 1-2 peels in nearly every case,
/// but the budget matches Arabic's [`ARABIC_PROCLITIC_PEEL_BUDGET`]
/// for symmetry and to leave headroom for any future addition
/// to the peel set. Pathological tokens like
/// `וווווו...מתי` exit after 3 peels of `ו` without
/// exhausting the iterator.
pub const HEBREW_PROCLITIC_PEEL_BUDGET: usize = 3;

/// Helper for [`MatchStrategy::FirstTokenWithHebrewClitics`].
///
/// Returns `true` when `first` exact-matches an entry in `table`
/// either directly OR after up to
/// [`HEBREW_PROCLITIC_PEEL_BUDGET`] iterative peels of the
/// recognised Hebrew proclitic prefixes (see
/// [`HEBREW_PROCLITIC_PREFIXES`]).
///
/// Each iteration tries each prefix in
/// [`HEBREW_PROCLITIC_PREFIXES`] order and takes the first one
/// that strips off a non-empty alphabetic residual. A residual
/// that collapses to empty (e.g. `ו` alone stripped to `""`) is
/// rejected — there is no host word left to match against the
/// table.
fn first_token_matches_after_hebrew_clitic_strip(table: &[&str], first: &str) -> bool {
    // Empty input is a routing-bug guard — bail without any
    // telemetry side effect.
    if first.is_empty() {
        return false;
    }
    let outcome = hebrew_clitic_peel_outcome(table, first);
    crate::lexicon_telemetry::record_hebrew_peel_depth(outcome);
    matches!(
        outcome,
        crate::lexicon_telemetry::PeelOutcome::MatchedAtDepth(_)
    )
}

/// Compute the peel-depth outcome for a Hebrew clitic-aware
/// table check. See [`arabic_clitic_peel_outcome`] for the
/// bucketing semantics — this is the Hebrew mirror.
fn hebrew_clitic_peel_outcome(
    table: &[&str],
    first: &str,
) -> crate::lexicon_telemetry::PeelOutcome {
    use crate::lexicon_telemetry::PeelOutcome;
    debug_assert!(!first.is_empty(), "caller must filter empty `first`");
    if table.contains(&first) {
        return PeelOutcome::MatchedAtDepth(0);
    }
    let mut current = first;
    for depth in 1..=HEBREW_PROCLITIC_PEEL_BUDGET {
        let Some(stripped) = peel_one_hebrew_proclitic(current) else {
            return PeelOutcome::BudgetExhausted;
        };
        if table.contains(&stripped) {
            return PeelOutcome::MatchedAtDepth(
                u8::try_from(depth).expect("peel budget fits in u8"),
            );
        }
        current = stripped;
    }
    PeelOutcome::BudgetExhausted
}

/// Attempt one peel of the recognised Hebrew proclitic prefixes
/// (see [`HEBREW_PROCLITIC_PREFIXES`]) from the front of `token`.
///
/// Returns `Some(residual)` when a prefix was successfully
/// peeled AND the residual is non-empty (we never propagate an
/// empty-string "match"). Returns `None` when no recognised
/// prefix matched OR when the only matching prefix would leave
/// an empty residual.
fn peel_one_hebrew_proclitic(token: &str) -> Option<&str> {
    for prefix in HEBREW_PROCLITIC_PREFIXES {
        if let Some(rest) = token.strip_prefix(prefix) {
            if !rest.is_empty() {
                return Some(rest);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------
// Per-language lexicon definitions (13 BCP-47 primary subtags after
// adds `he`; the count tracks BUILTIN_LEXICONS.len()).
// ---------------------------------------------------------------------

/// English (`en`) — substrate default. Keyword entries
/// imported verbatim from the earlier
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
    task_imperative_strategy: MatchStrategy::FirstBigram,
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
    task_imperative_strategy: MatchStrategy::FirstBigram,
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
    task_imperative_strategy: MatchStrategy::FirstBigram,
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
    task_imperative_strategy: MatchStrategy::FirstBigram,
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
    task_imperative_strategy: MatchStrategy::FirstBigram,
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
    task_imperative_strategy: MatchStrategy::FirstBigram,
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
    task_imperative_strategy: MatchStrategy::FirstBigram,
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
///
/// **:** `task_imperative_strategy` promoted from
/// [`MatchStrategy::FirstBigram`] to
/// [`MatchStrategy::FirstTokenWithArabicClitics`] so the
/// productive proclitic-prefix forms of the imperative verbs
/// recover their Task readings:
///
/// * `واكتب التقرير` ("and write the report") — `و` + `اكتب`.
/// * `وأرسل البريد` ("and send the email") — `و` + `أرسل`.
/// * `فجدول الاجتماع` ("then schedule the meeting") — `ف` +
///   `جدول`.
/// * `وراجع الخطة` ("and review the plan") — `و` + `راجع`.
///
/// earlier these all bypassed the imperative table (the
/// `FirstBigram` matcher compared the bare first-token /
/// first-bigram against entries verbatim, but `واكتب` is
/// neither `اكتب` nor `و اكتب`), so multi-clause Arabic task
/// directives that chained `و`/`ف` between verbs lost recall on
/// every clause after the first. The `Substring` strategy was
/// rejected for imperatives because the verb stems are
/// 3-character roots whose substrings are exceptionally
/// promiscuous in Arabic morphology (`جدول` "schedule" shares
/// its three letters with `جداول` "tables" and `الجدول` "the
/// table"; clitic-aware prefix peeling is precise enough to
/// recover the prefixed verb form without these false
/// positives).
///
/// The proclitic peel set was reduced from
/// 8 to 6 entries after `س` (1st-person future marker) was
/// shown to falsely surface imperatives on plain declarative
/// future-tense statements (`سأرسل البريد غدا` "I will send
/// the email tomorrow" peeled `س` to surface the imperative
/// table entry `أرسل`, producing a phantom Task observation).
/// `ك` was also removed for symmetric precision reasons. See
/// [`MatchStrategy::FirstTokenWithArabicClitics`] for the full
/// omission rationale.
// ---------------------------------------------------------------------
// Per-class match-strategy asymmetry in AR_LEXICON
// ---------------------------------------------------------------------
//
// The four match-strategy fields below intentionally do NOT use a
// uniform strategy across classes. The asymmetry is positional, not
// arbitrary:
//
// * `decision_strategy` / `task_strategy` = `Substring`. Decision
//   and task keywords are full multi-character lexical items (≥3
//   chars on average — `تقرر` "decided", `موافق` "approved",
//   `يرجى` "please", `من فضلك` "please / kindly") that can appear
//   anywhere in a sentence. The Substring strategy catches them
//   wherever they fall — including inside clitic-prefixed forms —
//   without needing a separate peel pass. Keywords starting with
//   what would otherwise be a productive clitic (e.g. `وقع` "signed"
//   begins with `و` which is also the conjunction proclitic) are
//   handled correctly by Substring because the matcher looks for
//   the literal token anywhere in the input — no peel is needed,
//   no false positive is created. The short-keyword false-positive
//   risk that motivates `FirstTokenWithArabicClitics` for the
//   imperative class does not apply here because decision / task
//   keywords are long enough that substring collisions are rare in
//   real Arabic text.
//
// * `task_imperative_strategy` = `FirstTokenWithArabicClitics`.
//   Arabic imperatives are positional — they must be the FIRST
//   alphabetic token in a sentence to be classified as a directive
//   (otherwise `أرسل` inside a longer sentence might just mean
//   "send" as a verb in declarative voice, not as a directive).
//   The proclitic-aware first-token strategy is the only place in
//   AR_LEXICON where positional+prefix-aware matching is needed,
//   because clitic-stacked imperatives (`واكتب التقرير` "and write
//   the report", `فجدول الاجتماع` "then schedule the meeting")
//   must surface the bare imperative root from the prefixed first
//   token without false-positively matching the same root buried
//   in a declarative sentence further downstream. Substring would
//   over-match (any sentence containing `أرسل` anywhere would emit
//   Task); plain FirstToken would miss the clitic-prefixed forms.
//
// * Interrogatives use `FirstTokenWithArabicClitics` for the same
//   positional reason — Arabic question words `كيف`/`متى`/`من`/`ما`
//   are short enough that Substring would collide with arbitrary
//   in-word fragments (`من` ⊂ `أمن` "safety" / `يمن` "Yemen" /
//   `زمن` "time"). The interrogative lockstep lives in
//   `interrogatives_for("ar")` (the per-language strategy is stored
//   on the interrogative table rather than the language lexicon).
//
// This per-class asymmetry IS the architectural design — uniform
// `Substring` across all classes would break imperatives, uniform
// `FirstTokenWithArabicClitics` would miss decision/task keywords
// that don't sit at the first token. Each class gets the strategy
// that matches its lexical and positional properties. A future
// contributor who reads this should NOT attempt to harmonise the
// strategies — see the test
// `arabic_lexicon_strategy_per_class_is_intentional` for the
// invariant this design pins down.
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
    // Substring: long-form keywords, anywhere in sentence.
    // See per-class asymmetry block above.
    decision_strategy: MatchStrategy::Substring,
    task_keywords: &["مهمة", "من فضلك", "يرجى", "متابعة", "إجراء"],
    // Substring: long-form keywords + bigram phrases
    // (`من فضلك`), anywhere in sentence. See per-class
    // asymmetry block above.
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
    // FirstTokenWithArabicClitics: positional (must be the
    // first alphabetic token) + clitic-aware (peels و/ف/ب/ل/ال/أل
    // before re-checking equality). See per-class asymmetry
    // block above.
    task_imperative_strategy: MatchStrategy::FirstTokenWithArabicClitics,
    stop_words: &[],
};

/// Hebrew (`he`).
///
/// Modern Hebrew (Israel) — the lexicon target for the
/// Hebrew language.
/// Hebrew is a right-to-left abjad: consonants are written as
/// independent letters, vowels are typically omitted in everyday
/// IM / news / business text (niqqud-less spelling), and short
/// proclitic particles clitically attach to the next word with
/// no orthographic separator (`ה` "the", `ו` "and", `ש` "that",
/// `מ` "from", `ל` "to/for", `ב` "in/at/with", `כ` "like/as").
///
/// Sources for the keyword inventory:
///
/// * Glinert, *Modern Hebrew: An Essential Grammar* (Routledge,
///   4th ed. 2005), §11.4 ("Sentence types: declarative,
///   imperative, interrogative"), §10.2 ("Polite requests and
///   imperatives — `נא` / `בבקשה` / `אנא`").
/// * Even-Shoshan, *Ha-Milon He-Hadash* (Kiryat Sefer, 2003) —
///   canonical entries for decision verbs (`הוחלט`, `החלטה`,
///   `אושר`, `נחתם`, `נדחה`) and task nouns (`משימה`,
///   `מטלה`).
///
/// **Per-class match-strategy asymmetry** (mirrors AR_LEXICON,
/// see the dedicated commentary block above `AR_LEXICON`):
///
/// * `decision_strategy` / `task_strategy` = `Substring`.
///   Decision and task keywords are full multi-character
///   lexical items (≥3 chars on average — `הוחלט`,
///   `החלטה`, `אושר`, `משימה`, `בבקשה`) that can appear
///   anywhere in a sentence. Substring catches them wherever
///   they fall, including inside clitic-prefixed forms
///   (`וההחלטה` "and the decision" still contains the literal
///   `החלטה`), without needing a separate peel pass.
/// * `task_imperative_strategy` = `FirstTokenWithHebrewClitics`.
///   Hebrew imperatives are positional — they must be the
///   FIRST alphabetic token in a sentence to be classified as
///   a directive (otherwise `שלח` inside a longer sentence
///   might just mean "send" as a past-tense verb form, not as
///   an imperative directive). The proclitic-aware first-token
///   strategy is the only place in HE_LEXICON where
///   positional+prefix-aware matching is needed, because
///   clitic-stacked imperatives (`ושלח את הדוח` "and send the
///   report", `ובדוק את הקובץ` "and check the file") must
///   surface the bare imperative root from the prefixed first
///   token without false-positively matching the same root
///   buried in a declarative sentence further downstream.
///   Substring would over-match (any sentence containing
///   `שלח` anywhere would emit Task); plain FirstToken would
///   miss the clitic-prefixed forms.
/// * Interrogatives use `FirstTokenWithHebrewClitics` for the
///   same positional reason — Hebrew question words `מי`
///   ("who", 2 chars) / `מה` ("what", 2 chars) / `מתי`
///   ("when", 3 chars) are short enough that Substring would
///   collide with arbitrary in-word fragments (`מי` ⊂ `מים`
///   "water" / `מילה` "word"; `מה` ⊂ `המה` "they" / `מהר`
///   "fast"). The interrogative lockstep lives in
///   `interrogatives_for("he")` (the per-language strategy is
///   stored on the interrogative table rather than the
///   language lexicon).
///
/// All keyword data is unpointed (no niqqud), because the
/// [`normalize_for_lookup`] normalisation primitive strips
/// niqqud + cantillation from input before comparison
/// (see [`is_hebrew_combining`]), so pointed input like
/// `מָתַי` matches the unpointed table entry `מתי`.
const HE_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "he",
    display_name: "Hebrew",
    decision_keywords: &[
        "הוחלט",  // "it was decided" (passive past)
        "החלטה",  // "decision" (noun)
        "החליטו", // "they decided" (active past 3pl)
        "אושר",   // "was approved" (passive past)
        "אישור",  // "approval" (noun)
        "אישרו",  // "they approved"
        "נחתם",   // "was signed" (passive past)
        "נדחה",   // "was rejected" (passive past)
        "סוכם",   // "was agreed / summarised"
    ],
    // Substring: long-form keywords, anywhere in sentence. See
    // per-class asymmetry block above and the matching block on
    // `AR_LEXICON`.
    decision_strategy: MatchStrategy::Substring,
    task_keywords: &[
        "משימה", // "task" (noun)
        "מטלה",  // "assignment / task" (noun, more formal)
        "בבקשה", // "please" (canonical polite request)
        "אנא",   // "please" (formal / written request)
        "מעקב",  // "follow-up" (noun)
    ],
    // Substring: long-form keywords, anywhere in sentence. See
    // per-class asymmetry block above.
    task_strategy: MatchStrategy::Substring,
    // Imperatives are unpointed masculine-singular forms (the
    // canonical Hebrew imperative shape). The matcher strips
    // niqqud + cantillation from input before comparison, so
    // pointed input (e.g. `כְּתֹב`) still matches the unpointed
    // table entry `כתוב`. Verbs cover the same English
    // imperative semantics shipped by EN_LEXICON for parity:
    // write / send / schedule / review / publish / fix / deploy /
    // investigate / prepare / update / merge.
    task_imperative_verbs: &[
        "כתוב", // "write"
        "שלח",  // "send"
        "קבע",  // "schedule / set"
        "סקור", // "review"
        "בדוק", // "check"
        "פרסם", // "publish"
        "תקן",  // "fix"
        "הצב",  // "deploy / set up"
        "חקור", // "investigate"
        "הכן",  // "prepare"
        "עדכן", // "update"
        "מזג",  // "merge"
    ],
    // FirstTokenWithHebrewClitics: positional (must be the
    // first alphabetic token) + clitic-aware (peels ו/ש/מ/ל/ב
    // before re-checking equality). See per-class asymmetry
    // block above and the matching block on AR_LEXICON.
    task_imperative_strategy: MatchStrategy::FirstTokenWithHebrewClitics,
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
/// the `interrogatives` table — which is now driven
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
    task_imperative_strategy: MatchStrategy::FirstBigram,
    stop_words: &["này", "kia", "đó", "đây"],
};

/// Indonesian (`id`).
///
/// Indonesian and Malay share most vocabulary; the `id` and
/// `ms` lexicons are identical at this stage because the
/// decision / task lexicons we ship don't yet differentiate
/// the few register-specific entries between the two (later
/// SLM-assisted extraction will handle the register
/// difference). Task class includes `mohon` /
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
    task_imperative_strategy: MatchStrategy::FirstBigram,
    stop_words: &["ini", "itu", "kemarin", "besok"],
};

/// Malay (`ms`). Currently aliases the Indonesian lexicon;
/// see the doc on [`ID_LEXICON`] for the rationale and the
/// earlier follow-up. Kept as a distinct constant so that
/// when we differentiate the two in , the change is
/// observable in this file rather than via an `alias` map.
const MS_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "ms",
    display_name: "Malay",
    decision_keywords: ID_LEXICON.decision_keywords,
    decision_strategy: ID_LEXICON.decision_strategy,
    task_keywords: ID_LEXICON.task_keywords,
    task_strategy: ID_LEXICON.task_strategy,
    task_imperative_verbs: ID_LEXICON.task_imperative_verbs,
    task_imperative_strategy: ID_LEXICON.task_imperative_strategy,
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
    task_imperative_strategy: MatchStrategy::Substring,
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
    task_imperative_strategy: MatchStrategy::FirstBigram,
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
    task_imperative_strategy: MatchStrategy::FirstBigram,
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
    task_imperative_strategy: MatchStrategy::FirstBigram,
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
    task_imperative_strategy: MatchStrategy::FirstBigram,
    stop_words: &[],
};

/// Tibetan (`bo`).
///
/// Tibetan script (`U+0F00..=U+0FFF`) uses the `tsheg` (`་`,
/// `U+0F0B`) as a *syllable* separator, not a word boundary,
/// and stacks consonants via subscript / superscript marks
/// that fall outside `unicode61`'s letter category. As with
/// Hindi the strategy for every class is
/// [`MatchStrategy::Substring`]: token-based matchers would
/// fragment intra-word stacks (e.g. `བཀའ་ཤོག་` "decree" splits
/// into pieces around the tsheg + subjoined consonants and
/// no keyword ever lines up with a token boundary).
///
/// Whatlang 0.18 does NOT ship a Tibetan classifier
/// ([`Lang::Bod`](https://docs.rs/whatlang/0.18.0/whatlang/enum.Lang.html)
/// is absent), so the language tag will normally be `None`
/// for Tibetan bodies. The FTS5 routing in is
/// body-based (see [`crate::script::is_cjk_or_thai_codepoint`])
/// so recall still works, and callers that explicitly know
/// the language can pass the `"bo"` tag to this registry to
/// get keyword extraction. The lexicon is intentionally
/// shipped despite the missing detector so the substrate can
/// be wired into Tibetan corpora via explicit tagging.
///
/// Sources:
/// * Decision / task vocabulary curated from
///   monlam.org and Tibetan-English dictionary entries
///   (Goldstein, *The New Tibetan-English Dictionary of
///   Modern Tibetan*, UC Press 2001).
const BO_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "bo",
    display_name: "Tibetan",
    decision_keywords: &[
        "ཐག་གཅོད",  // "decide / determine"
        "གཏན་འཁེལ", // "settle / finalise"
        "ཆོད",      // "decided" (past)
        "མོས་མཐུན",  // "agreed / consensus"
        "ཁས་ལེན",   // "accept"
        "གནང་བ",   // "approval / sanction" (honorific)
        "ཕྱིར་འདོར",  // "reject / dismiss"
        "མིང་རྟགས",  // "signature"
    ],
    decision_strategy: MatchStrategy::Substring,
    task_keywords: &[
        "ལས་ཀ",    // "work / task"
        "བྱ་བ",     // "action / activity"
        "ཐུགས་རྗེ་ཆེ", // "thank you / please (polite opener)"
        "ཞུ",       // "request" (honorific verb root)
        "རྗེས་འཇུག",  // "follow-up"
    ],
    task_strategy: MatchStrategy::Substring,
    // Tibetan word boundaries are syllable-level (tsheg
    // separated), not whitespace-separated, so the
    // `FirstBigram` token-based matcher cannot fire. We use
    // `Substring` here for the same reason as Hindi: stacked
    // consonants and combining vowel signs are non-alphabetic
    // (Unicode category Mn) and would split intra-syllable
    // under any alphabetic-token matcher.
    task_imperative_verbs: &[
        "འབྲི",   // "write" (imperative)
        "གཏོང",  // "send"
        "བཤེར",  // "review / check"
        "སྤེལ",   // "publish / spread"
        "གཟིགས", // "examine / inspect" (honorific)
    ],
    task_imperative_strategy: MatchStrategy::Substring,
    stop_words: &[],
};

/// Khmer (`km`).
///
/// Khmer script (`U+1780..=U+17FF`) lacks inter-word
/// whitespace and stacks subscript consonants via the
/// invisible `coeng` (`U+17D2`, the Khmer virama).
/// `unicode61` reduces any Khmer body to zero tokens. As
/// with Tibetan above the strategy for every class is
/// [`MatchStrategy::Substring`]: an alphabetic-token matcher
/// would split `សម្រេច` ("decide", with coeng + subscript
/// `m`) into pieces and never match the canonical form.
///
/// Whatlang 0.18 ships [`Lang::Khm`] so language detection
/// works for Khmer bodies; this lexicon is reachable both
/// via auto-detection and via explicit tag.
///
/// Sources:
/// * Decision / task vocabulary curated from
///   `headwordkhmer.com` and the *Khmer-English Dictionary*
///   (Headley et al., Dunwoody Press 1997).
const KM_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "km",
    display_name: "Khmer",
    decision_keywords: &[
        "សម្រេច",    // "decide"
        "ឯកភាព",    // "agreement / consensus"
        "យល់ព្រម",    // "agree / consent"
        "អនុម័ត",     // "approve / ratify"
        "ច្បាស់",     // "definite / settled"
        "ចុះហត្ថលេខា", // "sign (a document)"
        "បដិសេធ",    // "reject"
        "ទទួលយក",    // "accept"
    ],
    decision_strategy: MatchStrategy::Substring,
    task_keywords: &[
        "កិច្ចការ",  // "task / duty"
        "ការងារ", // "work / job"
        "សូម",     // "please" (canonical polite opener)
        "ស្នើ",     // "request / propose"
        "តាមដាន", // "follow up / track"
    ],
    task_strategy: MatchStrategy::Substring,
    task_imperative_verbs: &[
        "សរសេរ",   // "write"
        "ផ្ញើ",      // "send"
        "ត្រួតពិនិត្យ", // "review / inspect"
        "បោះពុម្ព",   // "publish / print"
        "អនុវត្ត",    // "implement / execute"
    ],
    task_imperative_strategy: MatchStrategy::Substring,
    stop_words: &[],
};

/// Myanmar / Burmese (`my`).
///
/// Myanmar script (`U+1000..=U+109F`) uses combining vowels,
/// subscript consonants attached via the visible `asat`
/// (`U+103A`) and `virama` (`U+1039`), and stacked
/// consonants — none of which fall inside `unicode61`'s
/// letter category. The script lacks inter-word whitespace.
/// As with Tibetan and Khmer above the strategy for every
/// class is [`MatchStrategy::Substring`].
///
/// Whatlang 0.18 ships [`Lang::Mya`] (Burmese) so language
/// detection works for Myanmar bodies. The substrate's
/// observation engine reaches this lexicon both via
/// auto-detection and via explicit tag.
///
/// Sources:
/// * Decision / task vocabulary curated from
///   `myanmar-language.com` and the *Myanmar-English
///   Dictionary* (Department of the Myanmar Language
///   Commission, Yangon 1993).
const MY_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "my",
    display_name: "Burmese",
    decision_keywords: &[
        "ဆုံးဖြတ်",     // "decide"
        "သဘောတူ",     // "agree / consent"
        "သဘောတူခွင့်ပြု", // "agree and permit / approve"
        "ခွင့်ပြု",      // "permit / approve"
        "အတည်ပြု",     // "confirm / ratify"
        "လက်မှတ်ထိုး",    // "sign (a document)"
        "ပယ်ချ",      // "reject / dismiss"
        "လက်ခံ",       // "accept"
    ],
    decision_strategy: MatchStrategy::Substring,
    task_keywords: &[
        "တာဝန်",     // "duty / task"
        "လုပ်ငန်း",    // "work / business"
        "ကျေးဇူးပြု", // "please" (canonical polite opener)
        "တောင်းဆို",   // "request"
        "လိုက်လံ",      // "follow up"
    ],
    task_strategy: MatchStrategy::Substring,
    task_imperative_verbs: &[
        "ရေး",         // "write"
        "ပို့",           // "send"
        "စစ်ဆေး",       // "check / review"
        "ထုတ်ဝေ",        // "publish / release"
        "အကောင်အထည်ဖော်", // "implement"
    ],
    task_imperative_strategy: MatchStrategy::Substring,
    stop_words: &[],
};

/// Lao (`lo`).
///
/// Lao script (`U+0E80..=U+0EFF`) is structurally parallel to
/// Thai: it lacks inter-word whitespace, uses combining
/// vowel signs and tone marks, and `unicode61` reduces any
/// Lao body to zero tokens. As with Tibetan / Khmer /
/// Myanmar above the strategy for every class is
/// [`MatchStrategy::Substring`].
///
/// Whatlang 0.18 does NOT ship a Lao classifier
/// ([`Lang::Lao`](https://docs.rs/whatlang/0.18.0/whatlang/enum.Lang.html)
/// is absent — the closest detection is Thai, which can
/// mis-tag Lao bodies). The FTS5 routing in is
/// body-based so recall still works, and callers can pass
/// `"lo"` explicitly. Same shipping rationale as Tibetan
/// above.
///
/// Sources:
/// * Decision / task vocabulary curated from
///   `laodictionary.net` and the *Lao-English Dictionary*
///   (Reinhorn, Larousse 2001).
const LO_LEXICON: LanguageLexicon = LanguageLexicon {
    primary_tag: "lo",
    display_name: "Lao",
    decision_keywords: &[
        "ຕັດສິນໃຈ",   // "decide"
        "ເຫັນດີ",     // "agree / consent"
        "ອະນຸມັດ",    // "approve / authorise"
        "ຍອມຮັບ",    // "accept"
        "ລົງລາຍເຊັນ", // "sign (a document)"
        "ປະຕິເສດ",   // "reject"
    ],
    decision_strategy: MatchStrategy::Substring,
    task_keywords: &[
        "ວຽກ",   // "work / task"
        "ຫນ້າທີ່",  // "duty / responsibility"
        "ກະລຸນາ", // "please" (canonical polite opener)
        "ຮ້ອງຂໍ",  // "request"
        "ຕິດຕາມ", // "follow up / track"
    ],
    task_strategy: MatchStrategy::Substring,
    task_imperative_verbs: &[
        "ຂຽນ",   // "write"
        "ສົ່ງ",    // "send"
        "ກວດກາ", // "check / review"
        "ເຜີຍແຜ່", // "publish / disseminate"
        "ປະຕິບັດ", // "implement / execute"
    ],
    task_imperative_strategy: MatchStrategy::Substring,
    stop_words: &[],
};

/// All built-in lexicons, in BCP-47-primary-tag order.
///
/// The exact set is the union of:
///
/// * [`SUPPORTED_PRIMARY_TAGS`] interrogative
///   coverage (originally 16 languages: en/es/fr/de/pt/it/ru/
///   vi/id/ms/ar/hi/ja/ko/zh/th; extended by to
///   add bo/km/my/lo).
/// * keyword-class coverage requirements: a
///   keyword bundle per language for the substrate's
///   built-in decision / task / imperative pipelines.
///
/// 20 languages ship today — the 12-language
/// base target (en/ja/ko/zh/es/fr/de/pt/ar/vi/th/id) plus four
/// add-ons (`it`, `ru`, `hi`, `ms`) that already have
/// interrogative tables, plus four script-coverage add-ons
/// (`bo`, `km`, `my`, `lo`) that close the
/// FTS5-tokeniser-blind / no-whitespace-word-boundary script
/// gap. The interrogative-table-vs-LexiconRegistry coverage
/// invariant test
/// ([`crate::interrogatives::SUPPORTED_PRIMARY_TAGS`]) holds
/// for every language listed here.
pub const BUILTIN_LEXICONS: &[LanguageLexicon] = &[
    AR_LEXICON, BO_LEXICON, DE_LEXICON, EN_LEXICON, ES_LEXICON, FR_LEXICON, HE_LEXICON, HI_LEXICON,
    ID_LEXICON, IT_LEXICON, JA_LEXICON, KM_LEXICON, KO_LEXICON, LO_LEXICON, MS_LEXICON, MY_LEXICON,
    PT_LEXICON, RU_LEXICON, TH_LEXICON, VI_LEXICON, ZH_LEXICON,
];

/// All BCP-47 primary tags shipped in the built-in
/// [`default_registry`]. Mirrors
/// [`crate::interrogatives::SUPPORTED_PRIMARY_TAGS`] by
/// design (one is the test invariant for the other).
pub const SUPPORTED_LEXICON_TAGS: &[&str] = &[
    "ar", "bo", "de", "en", "es", "fr", "he", "hi", "id", "it", "ja", "km", "ko", "lo", "ms", "my",
    "pt", "ru", "th", "vi", "zh",
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
    // Arabic proclitic-aware matcher.
    // -----------------------------------------------------------------

    #[test]
    fn table_matches_arabic_clitic_strip_finds_bare_first_token() {
        // the FirstTokenWithArabicClitics strategy
        // must remain a strict superset of FirstToken — a bare
        // unprefixed interrogative still matches via the
        // exact-equality fast path.
        let table = &["كيف", "متى", "أين", "لماذا"];
        for sentence in ["كيف الحال", "متى تأتي", "أين الكتاب", "لماذا تأخرت"]
        {
            assert!(
                table_matches(table, sentence, MatchStrategy::FirstTokenWithArabicClitics),
                "bare Arabic interrogative in {sentence:?} must match via the \
                 exact-equality fast path before any peel is attempted"
            );
        }
    }

    #[test]
    fn table_matches_arabic_clitic_strip_recovers_single_proclitic_prefix() {
        // main payload: each of the 4 single-character
        // productive proclitic prefixes (`و`, `ف`, `ب`, `ل`)
        // recovers the bare interrogative under one peel. The
        // later removal of `ك` and `س` is exercised by the
        // dedicated negative-assertion test
        // `table_matches_arabic_clitic_strip_drops_unproductive_k_and_s_prefixes`.
        let table = &["كيف", "متى", "أي", "من", "أين", "ما"];
        for (sentence, prefix, residual) in [
            ("وكيف يمكنني المساعدة", "و", "كيف"),
            ("فمتى نلتقي", "ف", "متى"),
            ("بأي طريقة نفعل ذلك", "ب", "أي"),
            ("لمن هذا الكتاب", "ل", "من"),
        ] {
            assert!(
                table_matches(table, sentence, MatchStrategy::FirstTokenWithArabicClitics),
                "Arabic proclitic-prefixed interrogative in {sentence:?} (prefix {prefix:?}, \
                 residual {residual:?}) must match via the proclitic peel"
            );
        }
    }

    #[test]
    fn table_matches_arabic_clitic_strip_drops_unproductive_k_and_s_prefixes() {
        // Precision guard: `ك` and `س`
        // were initially in the peel set but caused false
        // positives on both the
        // interrogative path and (more dangerously) the
        // imperative path. They are now deliberately omitted
        // from [`ARABIC_PROCLITIC_PREFIXES`]; this test pins
        // that omission so a future contributor can't silently
        // re-add them without re-running the false-positive
        // analysis.

        // Interrogative-path negatives: a leading `ك` or `س`
        // on a non-interrogative host must NOT surface an
        // interrogative residual via the peel.
        let interrogative_table = &["من", "ما", "أين", "كيف"];
        for sentence in [
            "كمن تريد أن أكون", // "like who do you want me to be" — `ك` + `من`,
            // pre-fix peeled to `من` and falsely matched the interrogative table.
            "سما الذي سيحدث غدا", // "so what will happen tomorrow" —
                                  // `س` + `ما`; pre-fix peeled to `ما` and falsely matched.
        ] {
            assert!(
                !table_matches(
                    interrogative_table,
                    sentence,
                    MatchStrategy::FirstTokenWithArabicClitics
                ),
                "precision guard: {sentence:?} must NOT match the interrogative \
                 table — the peel set no longer includes `ك` / `س`"
            );
        }

        // Imperative-path negative — the architectural reason
        // for the removal. `سأرسل` is the 1st-person future
        // ("I will send"), a plain declarative statement; pre-
        // fix it peeled `س` to surface `أرسل` and falsely
        // matched the imperative table, producing a phantom
        // Task observation on every Arabic future-tense
        // declarative whose verb shared a root with an `أ`-
        // initial imperative. Same risk on `سأصلح` ➜ `أصلح`.
        let imperative_table = &["أرسل", "أصلح", "اكتب"];
        for sentence in [
            "سأرسل البريد غدا",          // "I will send the email tomorrow".
            "سأصلح الخلل الأسبوع القادم", // "I will fix the bug next week".
        ] {
            assert!(
                !table_matches(
                    imperative_table,
                    sentence,
                    MatchStrategy::FirstTokenWithArabicClitics
                ),
                "precision guard: {sentence:?} (1st-person future, NOT \
                 imperative) must NOT match the imperative table — the peel set no \
                 longer includes `س`"
            );
        }
    }

    #[test]
    fn table_matches_arabic_clitic_strip_recovers_definite_article() {
        // the 2-character definite article `ال` peels
        // before the 1-character proclitics so a leading `الكتاب`
        // can surface as `كتاب`. The interrogative table doesn't
        // contain nouns, but the imperative use case does: the
        // future-marker compound `سال…` is contrived; the real
        // win is for verbs preceded by `و` + `ال` is rare. The
        // representative case here uses the `الكتاب`-style noun
        // surfacing because that's the cleanest test of the
        // longest-first peel ordering.
        let table = &["كتاب", "اجتماع"];
        assert!(
            table_matches(
                table,
                "الكتاب على المنضدة",
                MatchStrategy::FirstTokenWithArabicClitics
            ),
            "`الكتاب` must peel `ال` → `كتاب` and match the table"
        );
        assert!(
            table_matches(
                table,
                "الاجتماع في الساعة الثالثة",
                MatchStrategy::FirstTokenWithArabicClitics
            ),
            "`الاجتماع` must peel `ال` → `اجتماع` and match the table"
        );
    }

    #[test]
    fn table_matches_arabic_clitic_strip_iterates_stacked_prefixes() {
        // real Arabic stacks up to 3 proclitics on
        // a single host word. The peel budget
        // (ARABIC_PROCLITIC_PEEL_BUDGET = 3) must accommodate
        // the realistic stack-depth.
        let table = &["كتاب"];
        // `وللكتاب` = `و` + `ل` + `ل` + `كتاب` (3 peels).
        // `و` and `ل` are both 1-char proclitics, so the peel
        // iterates 3 times to surface `كتاب`.
        assert!(
            table_matches(
                table,
                "وللكتاب قيمة",
                MatchStrategy::FirstTokenWithArabicClitics
            ),
            "`وللكتاب` must peel `و` then `ل` then `ل` → `كتاب` within the 3-peel budget"
        );
        // `وبالكتاب` = `و` + `ب` + `ال` + `كتاب` (3 peels;
        // `ال` is the 2-char definite article).
        assert!(
            table_matches(
                table,
                "وبالكتاب نتعلم",
                MatchStrategy::FirstTokenWithArabicClitics
            ),
            "`وبالكتاب` must peel `و` then `ب` then `ال` → `كتاب` within the 3-peel budget"
        );
    }

    #[test]
    fn table_matches_arabic_clitic_strip_rejects_unrelated_first_token() {
        // false-positive guard: a sentence whose
        // first token contains zero proclitic prefixes and is
        // not itself a table entry must NOT match.
        let table = &["كيف", "متى", "أين", "لماذا"];
        for sentence in [
            "أنا في المكتب", // "I am in the office" — `أنا` starts with `أ` (interrog hamza
            // orthography) but `أ` is deliberately NOT in the peel set.
            "أحمد قادم غدا", // "Ahmad is coming tomorrow" — proper-name `أحمد`.
            "محمد ذهب",      // "Muhammad went" — proper-name `محمد`.
            "هذا كتاب جديد", // "This is a new book" — demonstrative `هذا`.
        ] {
            assert!(
                !table_matches(table, sentence, MatchStrategy::FirstTokenWithArabicClitics),
                "Arabic declarative {sentence:?} must NOT match the interrogative table \
                 under the proclitic-peeling strategy (no peel produces an interrogative \
                 residual)"
            );
        }
    }

    #[test]
    fn table_matches_arabic_clitic_strip_rejects_bare_proclitic_token() {
        // edge case: a first token consisting only of
        // a proclitic prefix (e.g. just `و` with no host word)
        // must NOT match — there is no residual to compare
        // against the table. This guards against accidentally
        // matching the empty string against a table containing
        // the empty string (defence in depth; no table actually
        // does that today).
        let table = &["كيف", "متى"];
        // Pure proclitic-only first token: stripping `و` would
        // leave "", which the peel helper explicitly rejects.
        assert!(
            !table_matches(table, "و كيف", MatchStrategy::FirstTokenWithArabicClitics),
            "bare proclitic `و` (separated from `كيف` by whitespace) must not falsely \
             surface `كيف` via the peel — `peel_one_arabic_proclitic` rejects empty residuals"
        );
    }

    #[test]
    fn peel_one_arabic_proclitic_longest_first_priority() {
        // ordering invariant: 2-char `ال` peels before
        // 1-char `ا`/`ل` so leading `الكتاب` surfaces `كتاب`,
        // not the meaningless `لكتاب` that a `ا`-first peel
        // would produce. (`ا` is not in the peel set, so this
        // is technically belt-and-braces — but the ordering test
        // pins the priority so a future contributor can't
        // accidentally re-order the constant.)
        assert_eq!(peel_one_arabic_proclitic("الكتاب"), Some("كتاب"));
        assert_eq!(
            peel_one_arabic_proclitic("أل"), /* def-art only */
            None
        );
        // Definite-article variant `أل` (hamza-on-alif + lam):
        // peels to non-empty residual.
        assert_eq!(peel_one_arabic_proclitic("ألكتاب"), Some("كتاب"));
        // 1-char productive proclitics return the bare residual.
        assert_eq!(peel_one_arabic_proclitic("وكيف"), Some("كيف"));
        assert_eq!(peel_one_arabic_proclitic("فمتى"), Some("متى"));
        assert_eq!(peel_one_arabic_proclitic("بأي"), Some("أي"));
        assert_eq!(peel_one_arabic_proclitic("لمن"), Some("من"));
        // A later review :
        // `ك` and `س` are NO LONGER in the peel set, so a
        // leading `ك` / `س` does not peel — the residual `كأين`
        // / `سيكون` is returned as-None (no other recognised
        // prefix at position 0 either). See
        // `table_matches_arabic_clitic_strip_drops_unproductive_k_and_s_prefixes`
        // for the host-level negative assertions, and the
        // docstring on `ARABIC_PROCLITIC_PREFIXES` for the
        // false-positive rationale (`سأرسل` ➜ imperative
        // `أرسل` was the architectural trigger).
        assert_eq!(peel_one_arabic_proclitic("كأين"), None);
        assert_eq!(peel_one_arabic_proclitic("سيكون"), None);
        // No recognised prefix → None.
        assert_eq!(peel_one_arabic_proclitic("أحمد"), None);
        assert_eq!(peel_one_arabic_proclitic("هذا"), None);
        assert_eq!(peel_one_arabic_proclitic("محمد"), None);
        // Pure proclitic with empty residual → None.
        assert_eq!(peel_one_arabic_proclitic("و"), None);
        assert_eq!(peel_one_arabic_proclitic("ال"), None);
    }

    #[test]
    fn arabic_clitic_peel_budget_bounds_worst_case_iteration() {
        // budget invariant: the helper must give up
        // after ARABIC_PROCLITIC_PEEL_BUDGET peels even on a
        // pathological input that could otherwise loop. A
        // string of N `و` characters followed by a non-matching
        // host produces N peels before exhausting the prefixes
        // — the budget caps at ARABIC_PROCLITIC_PEEL_BUDGET = 3.
        let table = &["كيف"];
        // `وووووووووو…كيف` with 10 leading `و` — 10 > 3, so the
        // budget exits before reaching `كيف`. This guards the
        // structural bound (an attacker can't force the matcher
        // to iterate proportional to input length).
        let mut adversarial = String::new();
        for _ in 0..10 {
            adversarial.push('و');
        }
        adversarial.push_str("كيف");
        assert!(
            !table_matches(
                table,
                adversarial.as_str(),
                MatchStrategy::FirstTokenWithArabicClitics
            ),
            "pathological 10-`و` prefix must exit via the peel budget without surfacing `كيف`"
        );
        // Sanity: exactly 3 leading `و` still matches (the
        // budget is inclusive — 3 peels are attempted).
        assert!(
            table_matches(
                table,
                "وووكيف يحدث ذلك",
                MatchStrategy::FirstTokenWithArabicClitics
            ),
            "3-`و` prefix `وووكيف` must surface `كيف` within the 3-peel budget"
        );
    }

    #[test]
    fn arabic_clitic_strategy_is_strict_superset_of_first_token() {
        // invariant: the FirstTokenWithArabicClitics
        // strategy must be a *strict superset* of FirstToken —
        // i.e. (a) every sentence that matches under FirstToken
        // must also match under FirstTokenWithArabicClitics
        // (no recall is removed), AND (b) some sentences match
        // under FirstTokenWithArabicClitics that do NOT match
        // under FirstToken (the new strategy genuinely adds
        // recall, not just preserves it).
        //
        // A later review: the
        // original implementation only checked direction (a) with
        // bare-first-token inputs (where both strategies trivially
        // match), making the assertion `!bare || clitic`
        // vacuously true if FirstTokenWithArabicClitics had been
        // broken to silently fall through to a copy of FirstToken.
        // The current implementation adds direction (b) with
        // proclitic-prefixed inputs, so a regression that made
        // the new strategy degrade to bare FirstToken would now
        // fail the second loop instead of passing the first
        // loop trivially.
        let table = &["كيف", "متى", "أين", "ما"];

        // Direction (a): bare-first-token cases — both strategies
        // must match because the fast-path equality fires before
        // any peel is attempted.
        for sentence in ["كيف الحال", "متى الاجتماع", "أين المفتاح", "ما هذا"]
        {
            let bare = table_matches(table, sentence, MatchStrategy::FirstToken);
            let clitic = table_matches(table, sentence, MatchStrategy::FirstTokenWithArabicClitics);
            assert!(
                bare && clitic,
                "FirstTokenWithArabicClitics must preserve all FirstToken matches: \
                 sentence {sentence:?} matched FirstToken={bare} but \
                 FirstTokenWithArabicClitics={clitic} (both must be true for the \
                 bare-first-token fast path)"
            );
        }

        // Direction (b): proclitic-prefixed cases — FirstToken
        // must NOT match (the prefixed surface is not in the
        // table) while FirstTokenWithArabicClitics MUST match
        // (the peel surfaces the bare interrogative). This is
        // the architecturally meaningful direction that pins
        // the recall gain was introduced to deliver.
        for (sentence, prefix, residual) in [
            ("وكيف يمكنني المساعدة", "و", "كيف"),
            ("فمتى نلتقي", "ف", "متى"),
            ("بأين", "ب", "أين"), // synthetic — pins the prefix peel even though
            // `بأين` isn't idiomatic Arabic.
            ("لما هذا", "ل", "ما"),
        ] {
            let bare = table_matches(table, sentence, MatchStrategy::FirstToken);
            let clitic = table_matches(table, sentence, MatchStrategy::FirstTokenWithArabicClitics);
            assert!(
                !bare,
                "FirstToken must NOT match the proclitic-prefixed surface form: \
                 sentence {sentence:?} (prefix {prefix:?}, residual {residual:?}) \
                 unexpectedly matched bare FirstToken — this would mean the strict \
                 superset assertion is vacuous"
            );
            assert!(
                clitic,
                "FirstTokenWithArabicClitics MUST match the proclitic-prefixed surface: \
                 sentence {sentence:?} (prefix {prefix:?}, residual {residual:?}) \
                 did not match — the strict-superset direction is broken"
            );
        }
    }

    #[test]
    fn arabic_clitic_strip_handles_nfd_hamza_alif_via_dual_prefix_entries() {
        // Regression guard:
        // `normalize_for_lookup` strips Arabic combining
        // marks (including U+0654 ARABIC HAMZA ABOVE) BEFORE NFC
        // composition, so an NFD-encoded `أل` (U+0627 ALEF +
        // U+0654 HAMZA ABOVE + U+0644 LAM) collapses to bare `ال`
        // (U+0627 + U+0644). An NFC-encoded `أل` (U+0623
        // HAMZA-ON-ALEF + U+0644) survives the strip because
        // U+0623 is a precomposed base character. Both forms
        // must successfully peel the definite article.
        //
        // The ARABIC_PROCLITIC_PREFIXES constant correctly
        // includes BOTH `ال` and `أل` entries, so the peel
        // succeeds regardless of the input's original NFC/NFD
        // encoding. This test pins the dual-entry design against
        // a future contributor who looks at `أل` and `ال` in the
        // constant and assumes one is redundant (it is NOT —
        // removing either would silently break recall on one of
        // the two encodings).
        // The production call path normalises via
        // `normalize_for_lookup` BEFORE invoking `table_matches`,
        // so this test mirrors that contract — anything else
        // wouldn't exercise the NFD-collapse interaction the
        // dual-entry design protects against.
        let table = &["كتاب"];
        let ar = Some("ar");

        // Case 1: NFC-encoded `أل` (precomposed U+0623 + U+0644).
        // The hamza-on-alif is a base character (not a combining
        // mark), so it survives `normalize_for_lookup`'s tashkeel-
        // strip pass intact; the peel matches via the `أل` entry
        // in the proclitic prefix list.
        let nfc_alef_hamza_lam = "\u{0623}\u{0644}\u{0643}\u{062A}\u{0627}\u{0628}"; // أل + كتاب
        let nfc_normalised = normalize_for_lookup(nfc_alef_hamza_lam, ar);
        assert!(
            table_matches(
                table,
                &nfc_normalised,
                MatchStrategy::FirstTokenWithArabicClitics
            ),
            "NFC-encoded `أل` (U+0623 + U+0644) must peel via the `أل` proclitic entry \
             after normalisation; got normalised={nfc_normalised:?}"
        );

        // Case 2: NFD-encoded `أل` (decomposed U+0627 ALEF +
        // U+0654 ARABIC HAMZA ABOVE + U+0644 LAM). The combining-
        // hamza-above is in the U+064B..U+065F tashkeel range, so
        // `normalize_for_lookup`'s tashkeel-strip pass removes it
        // BEFORE NFC composition runs, leaving bare `ال`
        // (U+0627 + U+0644). The peel then matches via the `ال`
        // entry — NOT via `أل` (which is no longer present after
        // the strip). Without BOTH entries in the proclitic list,
        // one of these two NFD/NFC encodings would silently fail.
        let nfd_alef_hamza_lam = "\u{0627}\u{0654}\u{0644}\u{0643}\u{062A}\u{0627}\u{0628}"; // alef + combining-hamza-above + lam + كتاب
        let nfd_normalised = normalize_for_lookup(nfd_alef_hamza_lam, ar);
        assert!(
            table_matches(
                table,
                &nfd_normalised,
                MatchStrategy::FirstTokenWithArabicClitics
            ),
            "NFD-encoded `أل` (U+0627 + U+0654 + U+0644) must peel via the `ال` entry \
             after tashkeel-strip collapses the combining hamza-above; got \
             normalised={nfd_normalised:?}"
        );

        // Case 3: canonical NFC `ال` (bare alef-lam, no hamza) —
        // sanity check that the `ال` entry handles the most
        // common Arabic definite article form.
        let canonical_alef_lam = "\u{0627}\u{0644}\u{0643}\u{062A}\u{0627}\u{0628}"; // ال + كتاب
        let canonical_normalised = normalize_for_lookup(canonical_alef_lam, ar);
        assert!(
            table_matches(
                table,
                &canonical_normalised,
                MatchStrategy::FirstTokenWithArabicClitics
            ),
            "Canonical `ال` (U+0627 + U+0644) must peel via the `ال` proclitic entry"
        );
    }

    // -----------------------------------------------------------------
    // FirstTokenWithHebrewClitics
    // -----------------------------------------------------------------

    #[test]
    fn table_matches_hebrew_clitic_strip_fast_path_bare_first_token() {
        // a bare interrogative-initial Hebrew sentence
        // (no proclitic prefixes on the first token) must surface
        // the matching table entry via the fast-path equality
        // check — no peel is required.
        let table = &["מי", "מה", "מתי", "איפה", "איך", "למה", "כמה", "האם"];
        for sentence in [
            "מי שלח את ההודעה", // "Who sent the message"
            "מה התוכנית שלך",   // "What is your plan"
            "מתי הישיבה הבאה",  // "When is the next meeting"
            "איפה הקובץ",       // "Where is the file"
            "איך הולך הפרויקט", // "How is the project going"
            "למה זה לא עובד",   // "Why doesn't this work"
            "כמה זמן זה ייקח",  // "How much time will this take"
            "האם זה אפשרי",     // "Is this possible"
        ] {
            assert!(
                table_matches(table, sentence, MatchStrategy::FirstTokenWithHebrewClitics),
                "bare Hebrew interrogative {sentence:?} must match via the FirstToken \
                 fast path of FirstTokenWithHebrewClitics"
            );
        }
    }

    #[test]
    fn table_matches_hebrew_clitic_strip_single_prefix_peel() {
        // single-prefix proclitic-attached interrogatives
        // (the architecturally meaningful direction — these would
        // NOT match under the bare FirstToken strategy).
        let table = &["מי", "מה", "מתי", "איפה", "איך", "למה"];
        for (sentence, prefix, residual) in [
            ("ומתי נתחיל את הפגישה", "ו", "מתי"), // "And when do we start the meeting"
            ("שמה הסיבה", "ש", "מה"),             // "That what's the reason"
            ("מאיפה הגיע המייל", "מ", "איפה"),    // synthetic — pins the מ-peel even though
            // colloquial Hebrew tends to use `מהיכן`
            // for "from where".
            ("לאיך נגיב לזה", "ל", "איך"), // synthetic — pins the ל-peel.
            ("בלמה התעכבנו", "ב", "למה"),  // synthetic — pins the ב-peel.
        ] {
            let bare = table_matches(table, sentence, MatchStrategy::FirstToken);
            let clitic = table_matches(table, sentence, MatchStrategy::FirstTokenWithHebrewClitics);
            assert!(
                !bare,
                "FirstToken must NOT match the proclitic-prefixed surface form: \
                 sentence {sentence:?} (prefix {prefix:?}, residual {residual:?}) \
                 unexpectedly matched bare FirstToken — this would mean the strict \
                 superset assertion is vacuous"
            );
            assert!(
                clitic,
                "FirstTokenWithHebrewClitics MUST match the proclitic-prefixed surface: \
                 sentence {sentence:?} (prefix {prefix:?}, residual {residual:?}) \
                 did not match — the strict-superset direction is broken"
            );
        }
    }

    #[test]
    fn table_matches_hebrew_clitic_strip_stacked_prefixes() {
        // realistic stacked proclitic forms must peel
        // within the 3-iteration budget. Hebrew commonly stacks
        // `ו` + `ש` + content (`ושמה` "and that what") and
        // `ו` + `ב` + content (`ובאיזה` "and in which").
        let table = &["מה", "איזה", "מתי"];
        // `ושמה` = `ו` + `ש` + `מה` (2 peels).
        assert!(
            table_matches(
                table,
                "ושמה היתרון",
                MatchStrategy::FirstTokenWithHebrewClitics
            ),
            "`ושמה` must peel `ו` then `ש` → `מה` within the 3-peel budget"
        );
        // `ובאיזה` = `ו` + `ב` + `איזה` (2 peels).
        assert!(
            table_matches(
                table,
                "ובאיזה אופן",
                MatchStrategy::FirstTokenWithHebrewClitics
            ),
            "`ובאיזה` must peel `ו` then `ב` → `איזה` within the 3-peel budget"
        );
        // `ושבמתי` = `ו` + `ש` + `ב` + `מתי` (3 peels — at the
        // budget limit; synthetic since this stack isn't
        // idiomatic but pins the budget edge).
        assert!(
            table_matches(
                table,
                "ושבמתי נחתום",
                MatchStrategy::FirstTokenWithHebrewClitics
            ),
            "`ושבמתי` must peel `ו` then `ש` then `ב` → `מתי` exactly at the 3-peel budget"
        );
    }

    #[test]
    fn table_matches_hebrew_clitic_strip_rejects_unrelated_first_token() {
        // false-positive guard: declarative Hebrew
        // sentences whose first token is not a recognised
        // interrogative (even after peeling) must NOT match.
        let table = &["מי", "מה", "מתי", "איפה"];
        for sentence in [
            "הילד הלך לבית הספר",      // "The child went to school" — `ה` not in peel set.
            "דניאל סיים את הפרויקט",   // proper-name first token.
            "ספר חדש פורסם",           // "A new book was published" — `ספר` doesn't peel.
            "תוצאת הפגישה היתה ברורה", // first token `תוצאת` is none of the peels.
        ] {
            assert!(
                !table_matches(table, sentence, MatchStrategy::FirstTokenWithHebrewClitics),
                "Hebrew declarative {sentence:?} must NOT match the interrogative table \
                 under FirstTokenWithHebrewClitics (no peel produces an interrogative \
                 residual)"
            );
        }
    }

    #[test]
    fn table_matches_hebrew_clitic_strip_rejects_bare_proclitic_token() {
        // edge case: a first token consisting only of a
        // proclitic prefix (e.g. just `ו` with no host word) must
        // NOT match — there is no residual to compare against the
        // table. This guards against accidentally matching the
        // empty string.
        let table = &["מי", "מה"];
        assert!(
            !table_matches(
                table,
                "ו מה התוכנית",
                MatchStrategy::FirstTokenWithHebrewClitics
            ),
            "bare proclitic `ו` (separated from `מה` by whitespace) must not falsely \
             surface `מה` via the peel — `peel_one_hebrew_proclitic` rejects empty residuals"
        );
    }

    #[test]
    fn table_matches_hebrew_clitic_strip_drops_unproductive_h_and_k_prefixes() {
        // deliberate-omission regression: `ה` (definite
        // article) and `כ` (preposition "like / as") are NOT in
        // HEBREW_PROCLITIC_PREFIXES, so they MUST NOT peel.
        //
        // The decisive `ה`-test: `הכתוב במסמך` ("the (one) written
        // in the document") is a noun phrase — a passive
        // participle with the definite article. If `ה` were in
        // the peel set, the matcher would surface `כתוב` (Hebrew
        // imperative "write!") on this declarative sentence,
        // emitting a false Task observation. Confirm it does NOT.
        let imperative_table = &["כתוב", "שלח", "סקור"];
        assert!(
            !table_matches(
                imperative_table,
                "הכתוב במסמך",
                MatchStrategy::FirstTokenWithHebrewClitics
            ),
            "definite-article `ה` MUST NOT peel — `הכתוב במסמך` (passive participle) \
             must not surface the imperative `כתוב`"
        );
        // Similarly, `כ` must not peel — `כתב לי` ("he wrote me")
        // begins with `כתב` (past-tense verb), not `כ` + a host;
        // even though no residual is in the imperative table here,
        // the negative regression pins the omission.
        assert!(
            !table_matches(
                imperative_table,
                "כתב לי הודעה",
                MatchStrategy::FirstTokenWithHebrewClitics
            ),
            "preposition `כ` MUST NOT peel — `כתב` (past-tense verb) must not surface as an \
             imperative via a `כ` peel"
        );
    }

    #[test]
    fn peel_one_hebrew_proclitic_longest_first_priority() {
        // ordering invariant: each Hebrew proclitic in
        // HEBREW_PROCLITIC_PREFIXES is exactly one codepoint, so
        // there is no "longest-first" ambiguity at the per-prefix
        // level (unlike Arabic's 2-char `ال` vs 1-char `ا`/`ل`).
        // The constant's iteration order DOES matter when an
        // input could match multiple recognised prefixes at the
        // same position — `ושמה` could in theory peel `ש` first
        // (yielding `שמה`-then-`מה`, 2 peels) or `ו` first
        // (yielding the same `מה` in 2 peels). The peel order
        // (ו, ש, מ, ל, ב) is deterministic; pin the surface
        // residuals one peel at a time.
        assert_eq!(peel_one_hebrew_proclitic("ומתי"), Some("מתי"));
        assert_eq!(peel_one_hebrew_proclitic("שמה"), Some("מה"));
        assert_eq!(peel_one_hebrew_proclitic("מאיזה"), Some("איזה"));
        assert_eq!(peel_one_hebrew_proclitic("לאיך"), Some("איך"));
        assert_eq!(peel_one_hebrew_proclitic("בלמה"), Some("למה"));
        // Definite article `ה` and preposition `כ` are deliberately
        // NOT in the peel set — see the docstring on
        // HEBREW_PROCLITIC_PREFIXES.
        assert_eq!(peel_one_hebrew_proclitic("הכתוב"), None);
        assert_eq!(peel_one_hebrew_proclitic("כתב"), None);
        // No recognised prefix → None.
        assert_eq!(peel_one_hebrew_proclitic("דניאל"), None);
        assert_eq!(peel_one_hebrew_proclitic("ספר"), None);
        // Pure proclitic with empty residual → None.
        assert_eq!(peel_one_hebrew_proclitic("ו"), None);
        assert_eq!(peel_one_hebrew_proclitic("ש"), None);
        assert_eq!(peel_one_hebrew_proclitic("מ"), None);
    }

    #[test]
    fn hebrew_clitic_peel_budget_bounds_worst_case_iteration() {
        // budget invariant: the helper must give up
        // after HEBREW_PROCLITIC_PEEL_BUDGET peels even on a
        // pathological input that could otherwise loop.
        let table = &["מתי"];
        // 10 leading `ו` followed by `מתי`. 10 > 3, so the budget
        // exits before reaching `מתי`. This guards the structural
        // bound (an attacker can't force the matcher to iterate
        // proportional to input length).
        let mut adversarial = String::new();
        for _ in 0..10 {
            adversarial.push('ו');
        }
        adversarial.push_str("מתי");
        assert!(
            !table_matches(
                table,
                adversarial.as_str(),
                MatchStrategy::FirstTokenWithHebrewClitics
            ),
            "pathological 10-`ו` prefix must exit via the peel budget without surfacing `מתי`"
        );
        // Sanity: exactly 3 leading `ו` still matches (the budget
        // is inclusive — 3 peels are attempted).
        assert!(
            table_matches(
                table,
                "ווומתי נתחיל",
                MatchStrategy::FirstTokenWithHebrewClitics
            ),
            "3-`ו` prefix `ווומתי` must surface `מתי` within the 3-peel budget"
        );
    }

    #[test]
    fn hebrew_clitic_strategy_is_strict_superset_of_first_token() {
        // invariant: the FirstTokenWithHebrewClitics
        // strategy must be a *strict superset* of FirstToken —
        // (a) every sentence matching FirstToken matches under
        // FirstTokenWithHebrewClitics, AND (b) some sentences
        // match under FirstTokenWithHebrewClitics that do NOT
        // match under FirstToken (the new strategy genuinely
        // adds recall).
        let table = &["מי", "מה", "מתי", "איפה"];

        // Direction (a): bare-first-token cases.
        for sentence in ["מי שלח את ההודעה", "מתי הישיבה", "איפה הקובץ", "מה התוכנית"]
        {
            let bare = table_matches(table, sentence, MatchStrategy::FirstToken);
            let clitic = table_matches(table, sentence, MatchStrategy::FirstTokenWithHebrewClitics);
            assert!(
                bare && clitic,
                "FirstTokenWithHebrewClitics must preserve all FirstToken matches: \
                 sentence {sentence:?} matched FirstToken={bare} but \
                 FirstTokenWithHebrewClitics={clitic} (both must be true)"
            );
        }

        // Direction (b): proclitic-prefixed cases.
        for (sentence, prefix, residual) in [
            ("ומתי נתחיל", "ו", "מתי"),
            ("שמה הסיבה", "ש", "מה"),
            ("מאיפה הגיע", "מ", "איפה"),
            ("ושמה התוצאה", "וש", "מה"), // 2-stack
        ] {
            let bare = table_matches(table, sentence, MatchStrategy::FirstToken);
            let clitic = table_matches(table, sentence, MatchStrategy::FirstTokenWithHebrewClitics);
            assert!(
                !bare,
                "FirstToken must NOT match the proclitic-prefixed surface form: \
                 sentence {sentence:?} (prefix {prefix:?}, residual {residual:?}) \
                 unexpectedly matched bare FirstToken — strict superset assertion would be vacuous"
            );
            assert!(
                clitic,
                "FirstTokenWithHebrewClitics MUST match the proclitic-prefixed surface: \
                 sentence {sentence:?} (prefix {prefix:?}, residual {residual:?}) \
                 did not match — the strict-superset direction is broken"
            );
        }
    }

    #[test]
    fn hebrew_normalisation_strips_niqqud_and_cantillation() {
        // pointed Hebrew (with niqqud or cantillation
        // marks) must collapse to the unpointed canonical form
        // via `normalize_for_lookup`. Without the strip, pointed
        // input like `מָתַי` would tokenise into single-letter
        // fragments at every combining mark.
        let table = &["מתי", "מי", "מה"];
        let he = Some("he");

        // Niqqud-decorated `מָתַי` (mem + qamats + tav + patah + yod).
        let pointed_matai = "\u{05DE}\u{05B8}\u{05EA}\u{05B7}\u{05D9}";
        let normalised = normalize_for_lookup(pointed_matai, he);
        assert_eq!(
            normalised, "מתי",
            "niqqud must be stripped: `מָתַי` ({pointed_matai:?}) → `מתי`, got {normalised:?}"
        );
        assert!(
            table_matches(
                table,
                &normalised,
                MatchStrategy::FirstTokenWithHebrewClitics
            ),
            "normalised pointed `מָתַי` must match the unpointed `מתי` table entry"
        );

        // Cantillation-decorated `מִ֖י` (mem + hiriq + tipeha + yod).
        // U+0596 = Hebrew accent tipeha (cantillation).
        let cantillated_mi = "\u{05DE}\u{05B4}\u{0596}\u{05D9}";
        let normalised = normalize_for_lookup(cantillated_mi, he);
        assert_eq!(
            normalised, "מי",
            "cantillation must be stripped: `מִ֖י` ({cantillated_mi:?}) → `מי`, got {normalised:?}"
        );
        assert!(
            table_matches(
                table,
                &normalised,
                MatchStrategy::FirstTokenWithHebrewClitics
            ),
            "normalised cantillated `מִ֖י` must match the unpointed `מי` table entry"
        );

        // Combined niqqud + cantillation + clitic prefix:
        // `וּמָתַ֖י` = `ו` + qubuts + `מ` + qamats + `ת` + patah +
        // tipeha + `י`. After strip + NFC: `ומתי`. Peel `ו` → `מתי`.
        let pointed_umatai = "\u{05D5}\u{05BB}\u{05DE}\u{05B8}\u{05EA}\u{05B7}\u{0596}\u{05D9}";
        let normalised = normalize_for_lookup(pointed_umatai, he);
        assert_eq!(normalised, "ומתי", "got {normalised:?}");
        assert!(table_matches(table,
                &normalised,
                MatchStrategy::FirstTokenWithHebrewClitics
            ),
            "pointed-and-prefixed `וּמָתַ֖י` must normalise to `ומתי` and then peel `ו` to surface `מתי`"
        );

        // Sanity: maqaf (U+05BE Hebrew hyphen, category Po) is
        // NOT stripped — it remains as a tokeniser boundary.
        let with_maqaf = "מי\u{05BE}שלח";
        let normalised = normalize_for_lookup(with_maqaf, he);
        assert!(
            normalised.contains('\u{05BE}'),
            "maqaf U+05BE must NOT be stripped (it's punctuation, not a combining mark); \
             got normalised={normalised:?}"
        );
    }

    #[test]
    fn hebrew_lexicon_has_expected_class_strategies() {
        // pin the per-class asymmetry for HE_LEXICON.
        // Decision / Task = Substring; TaskImperative =
        // FirstTokenWithHebrewClitics. See the per-class
        // asymmetry block in the HE_LEXICON docstring.
        let lex = default_registry()
            .lexicon_for("he")
            .expect("hebrew configured");
        let (_decision, decision_strat) = lex.entries(KeywordClass::Decision).unwrap();
        assert_eq!(
            decision_strat,
            MatchStrategy::Substring,
            "Hebrew decision strategy must be Substring (long-form keywords, anywhere)"
        );
        let (_task, task_strat) = lex.entries(KeywordClass::Task).unwrap();
        assert_eq!(
            task_strat,
            MatchStrategy::Substring,
            "Hebrew task strategy must be Substring"
        );
        let (imperatives, imperative_strat) = lex.entries(KeywordClass::TaskImperative).unwrap();
        assert_eq!(
            imperative_strat,
            MatchStrategy::FirstTokenWithHebrewClitics,
            "Hebrew imperative strategy must be FirstTokenWithHebrewClitics"
        );
        // Verify imperatives cover the substrate's standard
        // imperative semantics.
        for verb in ["כתוב", "שלח", "סקור", "פרסם", "תקן", "הצב"] {
            assert!(
                imperatives.contains(&verb),
                "Hebrew imperatives must include {verb:?}"
            );
        }
    }

    #[test]
    fn first_token_with_hebrew_clitics_languages_are_hebrew_only_for_now() {
        // pin exclusivity of MatchStrategy::FirstTokenWithHebrewClitics
        // to the `he` lexicon. Same architectural rationale as the
        // Arabic-side sibling test
        // (`first_token_with_arabic_clitics_languages_are_arabic_only_for_now`):
        // adding a second language that wants Hebrew-style
        // proclitic peeling must be an INTENTIONAL decision that
        // (a) verifies HEBREW_PROCLITIC_PREFIXES is the right peel
        // inventory for that language, AND (b) updates this test
        // to reflect the new membership. Yiddish (yi) and Ladino
        // (lad) use the Hebrew alphabet but have different
        // proclitic morphology and would need their own peel
        // inventories — silently sharing Modern Hebrew's would
        // introduce false positives.
        let reg = default_registry();
        let expected_hebrew_clitic_aware: std::collections::HashSet<&str> =
            ["he"].into_iter().collect();
        // Apply the same lockstep across both the LexiconRegistry
        // (task_imperative_strategy field) AND the interrogatives
        // module (matching_strategy_for) — both must agree.
        for lex in reg.iter() {
            let tag = lex.primary_tag;
            let (_, ti_strat) = lex.entries(KeywordClass::TaskImperative).unwrap();
            let is_clitic_aware = ti_strat == MatchStrategy::FirstTokenWithHebrewClitics;
            assert_eq!(is_clitic_aware,
                expected_hebrew_clitic_aware.contains(tag),
                "lexicon {tag}: hebrew-clitic-aware expected={}, got task_imperative_strategy={:?} \
                 — FirstTokenWithHebrewClitics must remain Hebrew-only \
                 (see test comment for rationale)",
                expected_hebrew_clitic_aware.contains(tag),
                ti_strat
            );
        }
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
    fn registry_interrogatives_delegate_to_module() {
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
    fn registry_covers_every_interrogative_language() {
        // Every language in the interrogative table
        // must ALSO appear in the registry, so the
        // per-sentence keyword matcher never finds itself
        // looking up decision/task keywords for a language it
        // can detect interrogatives for. This is the structural
        // invariant the registry contract relies on.
        use crate::interrogatives::SUPPORTED_PRIMARY_TAGS;
        let reg = default_registry();
        for tag in SUPPORTED_PRIMARY_TAGS {
            assert!(
                reg.lexicon_for(tag).is_some(),
                "interrogative table supports {tag} but the lexicon \
                 registry has no entry for it — add one to \
                 BUILTIN_LEXICONS"
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
                        // FirstTokenWithArabicClitics shares the
                        // exact-equality semantics of FirstToken
                        // on the first alphabetic token (plus
                        // peeled residuals), so entries must
                        // satisfy the same no-whitespace
                        // invariant as FirstToken — a
                        // whitespace-bearing entry could never
                        // equal a single alphabetic token (or a
                        // proclitic-stripped residual thereof).
                        MatchStrategy::FirstTokenWithArabicClitics => {
                            assert!(
                                !entry.chars().any(char::is_whitespace),
                                "{}/{strategy_label} entry {entry:?} is \
                                 FirstTokenWithArabicClitics but contains whitespace \
                                 (no token / peel-residual would ever equal it)",
                                lex.primary_tag,
                            );
                        }
                        // FirstTokenWithHebrewClitics shares the
                        // same first-alphabetic-token exact-
                        // equality semantics as FirstToken and
                        // FirstTokenWithArabicClitics, plus
                        // peeled residuals — entries with
                        // whitespace would never match.
                        MatchStrategy::FirstTokenWithHebrewClitics => {
                            assert!(
                                !entry.chars().any(char::is_whitespace),
                                "{}/{strategy_label} entry {entry:?} is \
                                 FirstTokenWithHebrewClitics but contains whitespace \
                                 (no token / peel-residual would ever equal it)",
                                lex.primary_tag,
                            );
                        }
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
        // differentiates them, ms aliases id. Pin so
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

    // -----------------------------------------------------------------
    // Tibetan / Khmer / Myanmar / Lao lexicon tests.
    // -----------------------------------------------------------------

    #[test]
    fn lexicons_all_use_substring_strategy() {
        // Tibetan / Khmer / Myanmar / Lao all lack inter-word
        // whitespace and use combining marks (virama, tsheg,
        // coeng, asat) that fall outside `unicode61`'s letter
        // category. A FirstToken or FirstBigram matcher would
        // either fragment intra-word stacks or fail to align
        // with any whitespace boundary. Substring matching is
        // the only strategy that fires on these scripts. Pin
        // this contract so a future contributor doesn't
        // silently switch a lexicon to FirstToken
        // and produce zero matches.
        let reg = default_registry();
        for tag in ["bo", "km", "my", "lo"] {
            let lex = reg.lexicon_for(tag).expect(" lexicon configured");
            assert_eq!(
                lex.decision_strategy,
                MatchStrategy::Substring,
                "{tag} decision_strategy must be Substring (no whitespace word boundaries)",
            );
            assert_eq!(
                lex.task_strategy,
                MatchStrategy::Substring,
                "{tag} task_strategy must be Substring (no whitespace word boundaries)",
            );
            assert_eq!(
                lex.task_imperative_strategy,
                MatchStrategy::Substring,
                "{tag} task_imperative_strategy must be Substring \
                 (combining marks split intra-word under any alphabetic-token matcher)",
            );
        }
    }

    #[test]
    fn tibetan_lexicon_keywords_are_in_tibetan_unicode_range() {
        // Defensive check: every Tibetan keyword must contain
        // at least one codepoint from the Tibetan block
        // (U+0F00..=U+0FFF). Catches accidental cut-and-paste
        // of non-Tibetan text (e.g. Devanagari that visually
        // resembles a Tibetan glyph) into the lexicon entries.
        let reg = default_registry();
        let lex = reg.lexicon_for("bo").expect("bo configured");
        let all_entries: Vec<&str> = lex
            .decision_keywords
            .iter()
            .chain(lex.task_keywords.iter())
            .chain(lex.task_imperative_verbs.iter())
            .copied()
            .collect();
        assert!(!all_entries.is_empty(), "bo lexicon must have entries");
        for entry in all_entries {
            assert!(
                entry
                    .chars()
                    .any(|c| ('\u{0F00}'..='\u{0FFF}').contains(&c)),
                "Tibetan lexicon entry {entry:?} contains no Tibetan codepoint",
            );
        }
    }

    #[test]
    fn khmer_lexicon_keywords_are_in_khmer_unicode_range() {
        let reg = default_registry();
        let lex = reg.lexicon_for("km").expect("km configured");
        let all_entries: Vec<&str> = lex
            .decision_keywords
            .iter()
            .chain(lex.task_keywords.iter())
            .chain(lex.task_imperative_verbs.iter())
            .copied()
            .collect();
        assert!(!all_entries.is_empty(), "km lexicon must have entries");
        for entry in all_entries {
            assert!(
                entry
                    .chars()
                    .any(|c| ('\u{1780}'..='\u{17FF}').contains(&c)),
                "Khmer lexicon entry {entry:?} contains no Khmer codepoint",
            );
        }
    }

    #[test]
    fn myanmar_lexicon_keywords_are_in_myanmar_unicode_range() {
        let reg = default_registry();
        let lex = reg.lexicon_for("my").expect("my configured");
        let all_entries: Vec<&str> = lex
            .decision_keywords
            .iter()
            .chain(lex.task_keywords.iter())
            .chain(lex.task_imperative_verbs.iter())
            .copied()
            .collect();
        assert!(!all_entries.is_empty(), "my lexicon must have entries");
        for entry in all_entries {
            assert!(
                entry.chars().any(|c| {
                    ('\u{1000}'..='\u{109F}').contains(&c)        // Myanmar main
                        || ('\u{AA60}'..='\u{AA7F}').contains(&c) // Myanmar Ext-A
                        || ('\u{A9E0}'..='\u{A9FF}').contains(&c) // Myanmar Ext-B
                }),
                "Myanmar lexicon entry {entry:?} contains no Myanmar codepoint",
            );
        }
    }

    #[test]
    fn lao_lexicon_keywords_are_in_lao_unicode_range() {
        let reg = default_registry();
        let lex = reg.lexicon_for("lo").expect("lo configured");
        let all_entries: Vec<&str> = lex
            .decision_keywords
            .iter()
            .chain(lex.task_keywords.iter())
            .chain(lex.task_imperative_verbs.iter())
            .copied()
            .collect();
        assert!(!all_entries.is_empty(), "lo lexicon must have entries");
        for entry in all_entries {
            assert!(
                entry
                    .chars()
                    .any(|c| ('\u{0E80}'..='\u{0EFF}').contains(&c)),
                "Lao lexicon entry {entry:?} contains no Lao codepoint",
            );
        }
    }

    #[test]
    fn arabic_lexicon_strategy_per_class_is_intentional() {
        // Regression guard:
        // the per-class strategy asymmetry in AR_LEXICON is the
        // architectural design, not an oversight. This test
        // pins each strategy at runtime so a contributor who
        // tries to "harmonise" them — e.g. push Substring up to
        // task_imperative, or push FirstTokenWithArabicClitics
        // down to decision/task — breaks a test rather than
        // silently degrading precision or recall.
        //
        // See the per-class asymmetry doc block on AR_LEXICON
        // (lexicon.rs:1265-1320) for the full rationale.
        let reg = default_registry();
        let lex = reg.lexicon_for("ar").expect("ar configured");

        // Decision class: Substring (long-form keywords,
        // anywhere in sentence). FirstTokenWithArabicClitics
        // would miss decision keywords that don't sit at the
        // first token (e.g. `هذا تقرر بالأمس` "this was decided
        // yesterday" puts `تقرر` at token position 2). Substring
        // is correct.
        assert_eq!(
            lex.decision_strategy,
            MatchStrategy::Substring,
            "AR_LEXICON.decision_strategy must be Substring — see per-class \
             asymmetry doc block on AR_LEXICON for rationale (long-form \
             keywords matched anywhere in sentence)"
        );

        // Task class: Substring (long-form keywords + bigram
        // phrases like `من فضلك`). Same rationale as decision.
        assert_eq!(
            lex.task_strategy,
            MatchStrategy::Substring,
            "AR_LEXICON.task_strategy must be Substring — see per-class \
             asymmetry doc block on AR_LEXICON for rationale (long-form \
             keywords + bigram phrases matched anywhere in sentence)"
        );

        // Imperative class: FirstTokenWithArabicClitics
        // (positional + clitic-aware). Substring would over-match
        // (any sentence containing `أرسل` anywhere would falsely
        // emit Task); plain FirstToken would miss clitic-stacked
        // imperatives like `واكتب التقرير` "and write the report"
        // that surface the bare imperative root only after peeling
        // the conjunction proclitic.
        assert_eq!(
            lex.task_imperative_strategy,
            MatchStrategy::FirstTokenWithArabicClitics,
            "AR_LEXICON.task_imperative_strategy must be \
             FirstTokenWithArabicClitics — see per-class asymmetry doc block \
             on AR_LEXICON for rationale (positional + clitic-aware matching \
             is what makes the imperative class architecturally correct)"
        );
    }

    #[test]
    fn lexicons_are_distinct_from_each_other() {
        // The four scripts are visually distinct but tooling
        // bugs (font-substitution-driven mis-copy, lossy NFC
        // round-trips, automated translation pipelines) can
        // produce eerily similar-looking byte sequences. Pin
        // pairwise distinctness so a regression that
        // accidentally aliases (e.g.) bo to lo fails the test
        // rather than silently degrading recall.
        let reg = default_registry();
        let tags = ["bo", "km", "my", "lo"];
        for (i, a_tag) in tags.iter().enumerate() {
            for b_tag in tags.iter().skip(i + 1) {
                let a = reg.lexicon_for(a_tag).expect("configured");
                let b = reg.lexicon_for(b_tag).expect("configured");
                assert_ne!(
                    a.decision_keywords, b.decision_keywords,
                    "{a_tag} and {b_tag} share identical decision_keywords \
                     — accidental aliasing?",
                );
                assert_ne!(
                    a.task_keywords, b.task_keywords,
                    "{a_tag} and {b_tag} share identical task_keywords",
                );
                assert_ne!(
                    a.task_imperative_verbs, b.task_imperative_verbs,
                    "{a_tag} and {b_tag} share identical task_imperative_verbs",
                );
            }
        }
    }

    #[test]
    fn decision_keyword_extracted_under_substring_matching() {
        // End-to-end smoke test: a sentence containing a
        // decision keyword must round-trip through
        // the `table_matches` Substring path. This is the
        // path the LexiconExtractor exercises when
        // classifying CJK / Indic sentences.
        let reg = default_registry();
        // Tibetan: "ཐུགས་རྗེ་ཆེ ་ ལས་ཀ་ འདི་ ཐག་གཅོད་ བྱེད་ ཐུབ་ པ ?"
        // — informal "Can we decide this task, please?" The
        // decision keyword ཐག་གཅོད must trip the Substring
        // matcher.
        let bo_lex = reg.lexicon_for("bo").unwrap();
        assert!(
            table_matches(
                bo_lex.decision_keywords,
                "ཐུགས་རྗེ་ཆེ་ལས་ཀ་འདི་ཐག་གཅོད་བྱེད་ཐུབ་པ",
                bo_lex.decision_strategy,
            ),
            "Tibetan decision keyword ཐག་གཅོད did not fire under Substring",
        );

        // Khmer: a sentence containing the decision verb
        // សម្រេច ("decide").
        let km_lex = reg.lexicon_for("km").unwrap();
        assert!(
            table_matches(
                km_lex.decision_keywords,
                "យើងសម្រេចចេញគោលនយោបាយថ្មីហើយ",
                km_lex.decision_strategy,
            ),
            "Khmer decision keyword សម្រេច did not fire under Substring",
        );

        // Myanmar: a sentence containing ဆုံးဖြတ် ("decide").
        let my_lex = reg.lexicon_for("my").unwrap();
        assert!(
            table_matches(
                my_lex.decision_keywords,
                "ကျွန်တော်တို့ဆုံးဖြတ်ပြီးပါပြီ",
                my_lex.decision_strategy,
            ),
            "Myanmar decision keyword ဆုံးဖြတ် did not fire under Substring",
        );

        // Lao: a sentence containing ຕັດສິນໃຈ ("decide").
        let lo_lex = reg.lexicon_for("lo").unwrap();
        assert!(
            table_matches(
                lo_lex.decision_keywords,
                "ພວກເຮົາຕັດສິນໃຈແລ້ວ",
                lo_lex.decision_strategy,
            ),
            "Lao decision keyword ຕັດສິນໃຈ did not fire under Substring",
        );
    }

    #[test]
    fn task_imperative_extracted_under_substring_matching() {
        // Cross-script imperative verb test. Every
        // lexicon's imperative-verb list is non-empty (unlike
        // ja/ko/zh/th) because Substring matching DOES fire
        // on no-whitespace scripts, so dropping the verbs
        // would lose recall.
        let reg = default_registry();

        // Tibetan: "འབྲི" ("write")
        let bo_lex = reg.lexicon_for("bo").unwrap();
        assert!(table_matches(
            bo_lex.task_imperative_verbs,
            "འདི་འབྲི་རོགས་གནང",
            bo_lex.task_imperative_strategy,
        ));

        // Khmer: "ផ្ញើ" ("send")
        let km_lex = reg.lexicon_for("km").unwrap();
        assert!(table_matches(
            km_lex.task_imperative_verbs,
            "សូមផ្ញើឯកសារ",
            km_lex.task_imperative_strategy,
        ));

        // Myanmar: "ပို့" ("send")
        let my_lex = reg.lexicon_for("my").unwrap();
        assert!(table_matches(
            my_lex.task_imperative_verbs,
            "ကျေးဇူးပြုပြီးပို့ပါ",
            my_lex.task_imperative_strategy,
        ));

        // Lao: "ສົ່ງ" ("send")
        let lo_lex = reg.lexicon_for("lo").unwrap();
        assert!(table_matches(
            lo_lex.task_imperative_verbs,
            "ກະລຸນາສົ່ງເອກະສານ",
            lo_lex.task_imperative_strategy,
        ));
    }

    #[test]
    fn no_keyword_substring_collides_with_test_declarative() {
        // Regression guard: the existing
        // `lao_khmer_myanmar_fact_shaped_without_whitespace`
        // test in extractor.rs uses three canonical
        // declarative sentences to verify Fact-shape
        // classification. Pin that none of the
        // lexicon entries OR interrogative entries
        // accidentally substring-match those declaratives.
        // If a future contributor adds a keyword that
        // collides, this test fires before
        // `lao_khmer_myanmar_fact_shaped_without_whitespace`
        // does, with a much more precise error message.
        //
        // Tibetan is included as a defense-in-depth case
        // even though whatlang 0.18 does not ship a
        // Lang::Bod classifier — explicit-tag callers via
        // FFI / connector pipelines that tag bodies as `bo`
        // would still invoke the Tibetan lexicon, so a
        // keyword-vs-declarative collision IS reachable in
        // production via that path. The canonical Tibetan
        // declarative "ལྷ་ས་ནི་བོད་ཀྱི་རྒྱལ་ས་ཡིན།"
        // ("Lhasa is the capital of Tibet") is the
        // structural parallel to the km/my/lo cases.
        use crate::interrogatives::interrogatives_for;
        let reg = default_registry();

        let cases = [
            ("bo", "ལྷ་ས་ནི་བོད་ཀྱི་རྒྱལ་ས་ཡིན"),
            ("km", "ភ្នំពេញគឺជារដ្ឋធានីនៃប្រទេសកម្ពុជា"),
            ("my", "ရန်ကုန်သည်မြန်မာနိုင်ငံ၏အကြီးဆုံးမြို့ဖြစ်သည်"),
            ("lo", "ວຽງຈັນເປັນນະຄອນຫຼວງຂອງປະເທດລາວ"),
        ];

        for (tag, declarative) in cases {
            // Interrogative table: NO entry should match the
            // declarative.
            let (interr, _) = interrogatives_for(tag).expect("configured");
            for entry in interr {
                assert!(
                    !declarative.contains(entry),
                    "{tag} interrogative entry {entry:?} substring-matches \
                     a multilingual declarative test sentence {declarative:?}",
                );
            }

            // Decision keywords: NO entry should match.
            let lex = reg.lexicon_for(tag).expect("configured");
            for entry in lex.decision_keywords {
                assert!(
                    !declarative.contains(entry),
                    "{tag} decision_keyword {entry:?} substring-matches \
                     a multilingual declarative test sentence {declarative:?}",
                );
            }

            // Task keywords: NO entry should match.
            for entry in lex.task_keywords {
                assert!(
                    !declarative.contains(entry),
                    "{tag} task_keyword {entry:?} substring-matches \
                     a multilingual declarative test sentence {declarative:?}",
                );
            }

            // Task imperative verbs: NO entry should match
            // (these declaratives are nominal statements, not
            // imperatives).
            for entry in lex.task_imperative_verbs {
                assert!(
                    !declarative.contains(entry),
                    "{tag} task_imperative_verb {entry:?} substring-matches \
                     a multilingual declarative test sentence {declarative:?}",
                );
            }
        }
    }

    #[test]
    fn lao_negation_and_business_nouns_do_not_match_interrogatives() {
        // Pin the deliberate omission of bare `ບໍ`
        // (U+0E9A U+0ECD) from the Lao interrogative
        // table. `ບໍ` is a strict 2-codepoint prefix of
        // both:
        //   * the negation particle `ບໍ່`
        //     (U+0E9A U+0ECD U+0EC8, "not")
        //   * the everyday nouns `ບໍລິສັດ` ("company") and
        //     `ບໍລິການ` ("service")
        // Under `InterrogativeMatch::Substring` (which the
        // Lao lexicon uses because Lao script has no
        // inter-word whitespace), including `ບໍ` in the
        // interrogative table would mis-classify every
        // negative Lao sentence and every clause about a
        // company / service as a Question. This regression
        // test fires before the production extractor would,
        // with a precise error message naming the
        // offending entry.
        use crate::interrogatives::interrogatives_for;
        let (interr, _) = interrogatives_for("lo").expect("Lao interrogatives configured");

        // Each sentence is grammatically declarative
        // (negation or simple subject + verb) and MUST NOT
        // match any Lao interrogative under substring.
        let declaratives = [
            // negation: "I don't have"
            "ຂ້ອຍບໍ່ມີ",
            // negation: "It isn't"
            "ມັນບໍ່ແມ່ນ",
            // negation: "I can't"
            "ຂ້ອຍບໍ່ໄດ້",
            // business noun: "I work at the company"
            "ຂ້ອຍເຮັດວຽກຢູ່ບໍລິສັດ",
            // business noun: "The service is good"
            "ບໍລິການດີ",
        ];

        for declarative in declaratives {
            for entry in interr {
                assert!(
                    !declarative.contains(entry),
                    "Lao interrogative entry {entry:?} substring-matches \
                     a Lao declarative {declarative:?} — this indicates \
                     the deliberate omission of bare `ບໍ` has \
                     been undone without solving the negation/business-noun \
                     collision documented in interrogatives.rs.",
                );
            }
        }
    }
}
