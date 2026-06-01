//! Per-language interrogative-word tables for question detection.
//!
//! Phase 1.4 of the multilingual roadmap: the lexicon extractor's
//! [`crate::extractor::looks_like_question`] check used to consult a
//! single English-only `INTERROGATIVES` constant — fine for the
//! initial English-only extractor, but it silently mis-classified
//! every non-English question that lacked an ASCII `?` terminator
//! (e.g. a Japanese sentence that ends in `ですか。` or a Korean
//! sentence with a sentence-final ` -까?` particle). This module
//! provides a per-BCP-47-primary-subtag table so the extractor can
//! pick the right interrogative list once a language tag has been
//! attached to the sentence by [`crate::language::detect_language`].
//!
//! ## Cross-references
//!
//! * `docs/DESIGN.md` §3.2 — observation extractor responsibilities.
//! * `docs/MULTILINGUAL.md` (Phase 1.1 spec, the multilingual
//!   `LexiconRegistry` Phase 1.1 will ship) — this table is the
//!   precursor / minimal viable version; once Phase 1.1 lands, the
//!   `LexiconRegistry` should subsume this map alongside the
//!   decision / task keyword lists.
//!
//! ## Matching strategy
//!
//! Languages differ in how questions are formed and where the
//! interrogative word lands in the sentence:
//!
//! * **Space-separated, sentence-initial interrogative** (English,
//!   German, Spanish, French, Portuguese, Italian, Russian, Arabic,
//!   Indonesian, Vietnamese): the question word is typically the
//!   first token (`What is …?`, `¿Quién es …?`, `Wer hat …?`,
//!   `Pourquoi …?`, `Por que …?`, `لماذا …؟`). Match
//!   first-alphabetic-token equality after lowercasing.
//! * **No word boundaries, interrogative can appear anywhere**
//!   (Japanese, Korean, Chinese, Thai): the question word may be
//!   in the middle of the sentence (`今日は何曜日ですか`, `오늘은
//!   무엇입니까`, `今天是星期几`, `วันนี้คุณเป็นอย่างไรบ้าง`). Match
//!   substring presence.
//!
//! [`interrogatives_for`] returns the slice for a given primary
//! subtag; [`matching_strategy_for`] returns whether to use
//! first-token or substring matching. The extractor consumes both.
//!
//! ## Source provenance
//!
//! Each language list is curated from canonical interrogative
//! tables in the language's reference grammar — not hand-rolled. The
//! source per language is documented inline alongside the table.

/// How to match interrogatives against a sentence for a given
/// language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterrogativeMatch {
    /// The sentence's first alphabetic token (case-folded) must
    /// exactly equal one of the interrogatives. Used for languages
    /// where the question word is canonically sentence-initial and
    /// word boundaries are clear from whitespace (English, German,
    /// Romance languages, Indonesian). Arabic used `FirstToken`
    /// before Phase 1.6 but now uses
    /// [`Self::FirstTokenWithArabicClitics`] to recover the
    /// proclitic prefix forms (`وكيف`, `فمتى`, `بأي`,
    /// `لمن`) that the bare FirstToken matcher misses.
    FirstToken,
    /// The first alphabetic token OR the space-joined first two
    /// alphabetic tokens (case-folded) must equal an entry.
    /// Strict superset of [`Self::FirstToken`]: single-word entries
    /// still match via the first-token arm; multi-word entries
    /// (`tại sao`, `khi nào`, `vì sao`) match via the bigram arm.
    /// Used for languages whose canonical interrogatives include
    /// short two-token collocations whose bare leading token is
    /// too high-frequency to use on its own (Vietnamese, where
    /// `tại` / `khi` / `vì` are common prepositions /
    /// conjunctions in declaratives — see Devin Review finding
    /// #ANALYSIS-0004 / #FLAG-0002d).
    FirstBigram,
    /// Any interrogative appearing as a substring of the
    /// case-folded sentence counts as a match. Used for languages
    /// where the question word can appear anywhere in the
    /// sentence (Japanese, Korean, Chinese, Thai) — typically
    /// because word boundaries are not whitespace-delimited
    /// and/or because the language permits non-initial interrogative
    /// placement.
    Substring,
    /// Phase 1.6 strategy for Arabic-script languages whose
    /// proclitics agglutinate to the host word: tries first-token
    /// equality, then iteratively peels the recognised Arabic
    /// proclitic prefixes (`و` "and", `ف` "then", `ب` "with",
    /// `ل` "to", and the 2-character definite article `ال` /
    /// `أل` "the") and re-checks equality after each peel. The
    /// preposition `ك` ("like / as") and the future marker `س`
    /// ("will") were initially in the peel set but were removed
    /// in sweep 1 (Devin Review #ANALYSIS-0004) for precision
    /// reasons. See
    /// [`crate::lexicon::MatchStrategy::FirstTokenWithArabicClitics`]
    /// for the full design notes (peel inventory, why `أ`
    /// interrogative hamza is excluded, why `Substring` is
    /// rejected for short Arabic interrogatives).
    FirstTokenWithArabicClitics,
    /// Phase 1.7 strategy for Hebrew: tries first-token equality,
    /// then iteratively peels the recognised Hebrew proclitic
    /// prefixes (`ו` "and", `ש` "that / which", `מ` "from",
    /// `ל` "to / for", `ב` "in / at / with") and re-checks
    /// equality after each peel. The definite article `ה` and
    /// the preposition `כ` are deliberately excluded from the
    /// peel set; see
    /// [`crate::lexicon::MatchStrategy::FirstTokenWithHebrewClitics`]
    /// for the full design notes (peel inventory, why `ה` /
    /// `כ` are excluded, why `Substring` is rejected for short
    /// Hebrew interrogatives).
    FirstTokenWithHebrewClitics,
}

/// Look up the interrogative-word list for a BCP-47 primary
/// language subtag. Returns `None` when no list is configured for
/// the tag — callers should fall back to the English list (or
/// `None` semantics, depending on whether they're trying to enrich
/// or to refuse).
///
/// Tag matching is exact on the primary subtag (`"en"`, `"ja"`,
/// `"zh"`, …). Region-tagged inputs should be reduced via
/// [`crate::language::LanguageTag::primary`] before lookup.
pub fn interrogatives_for(
    primary_tag: &str,
) -> Option<(&'static [&'static str], InterrogativeMatch)> {
    match primary_tag {
        // English — Cambridge Grammar of the English Language §10
        // ("Interrogative clauses"). The classical "wh-" set.
        "en" => Some((
            &[
                "who", "what", "when", "where", "why", "how", "which", "whose", "whom",
            ],
            InterrogativeMatch::FirstToken,
        )),

        // Spanish — Real Academia Española, Nueva gramática de la
        // lengua española §22 ("Interrogativos y exclamativos").
        // Note: Spanish interrogatives canonically carry an acute
        // accent (qué vs. que). We match the lowercase canonical
        // forms; callers feed lowercased text in so the diacritic
        // is preserved (Spanish lowercase keeps acutes).
        //
        // Deliberately omitted: `por`. The high-frequency RAE
        // construction `¿por qué?` ("why?") tokenises to a leading
        // `por`, but `por` is one of the most common prepositions
        // in Spanish (`por favor`, `por la mañana`, `por
        // supuesto`, `por ejemplo`, `por ahora`, ...) and a
        // FirstToken match on `por` would mis-classify every
        // declarative starting with the preposition as a
        // question. The `¿` opener and `?` terminator are strong
        // enough signals for `¿por qué?` on their own — the
        // sentence-shape gate elsewhere in the extractor already
        // surfaces these via the `?` terminator. See Devin Review
        // finding #FLAG-0003.
        "es" => Some((
            &[
                "qué", "quién", "quiénes", "cuándo", "dónde", "adónde", "cómo", "cuál", "cuáles",
                "cuánto", "cuánta", "cuántos", "cuántas",
            ],
            InterrogativeMatch::FirstToken,
        )),

        // French — Grevisse, Le Bon Usage §382 ("Mots
        // interrogatifs").
        //
        // Deliberately omitted: `est-ce`. The `est-ce que ...?`
        // construction is the most common French question opener,
        // but the FirstToken strategy splits on non-alphabetic
        // characters (including the hyphen), so `est-ce` would
        // tokenise to `est` — not to the multi-character literal
        // we'd be looking up. Adding `est-ce` to this list is
        // unreachable dead code. `est-ce que` questions almost
        // always end with `?` and the `?` terminator alone is
        // sufficient signal; the alternative (a hyphen-tolerant
        // tokeniser) would degrade the strategy for every other
        // language. See Devin Review finding #BUG-0001.
        //
        // Deliberately omitted: `que`. Unlike Spanish (which has
        // the accented interrogative `qué` vs. the unaccented
        // conjunction `que`), French has no orthographic
        // distinction between the interrogative `que` (`Que veux-tu?`
        // — "What do you want?") and the high-frequency conjunction
        // / relative pronoun `que` (`Je crois que tu as raison`, `Que
        // la lumière soit` — subjunctive opener, `Le livre que je
        // lis`, ...). A FirstToken match on bare `que` would
        // mis-classify a large fraction of subjunctive openers,
        // exclamations (`Que c'est beau!`), and embedded-clause
        // sentences as questions. The alternative `quoi` is
        // interrogative-only and stays in the list; combined with
        // the `?` terminator on `que ...?` openers, recall stays
        // adequate. Same class of bug as Spanish / Portuguese
        // `por` (FLAG-0003), Indonesian / Malay `di` / `yang`
        // (FLAG-0005b). See Devin Review finding #FLAG-0001c.
        "fr" => Some((
            &[
                "qui",
                "quoi",
                "quand",
                "où",
                "pourquoi",
                "comment",
                "quel",
                "quelle",
                "quels",
                "quelles",
                "combien",
                "lequel",
                "laquelle",
                "lesquels",
                "lesquelles",
            ],
            InterrogativeMatch::FirstToken,
        )),

        // German — Duden, Die Grammatik §1140 ("Interrogativpronomen
        // und -adverbien"). Includes both interrogative pronouns
        // (wer/was) and adverbs (wann/wo/wie/warum).
        "de" => Some((
            &[
                "wer", "wen", "wem", "wessen", "was", "wann", "wo", "wohin", "woher", "wie",
                "warum", "weshalb", "wieso", "welche", "welcher", "welches", "welchen", "welchem",
                "wieviel", "wieviele",
            ],
            InterrogativeMatch::FirstToken,
        )),

        // Portuguese — Cunha & Cintra, Nova Gramática do
        // Português Contemporâneo §13.5 ("Pronomes
        // interrogativos"). Brazilian and European share these.
        //
        // Deliberately omitted: `por`. Identical reasoning to
        // Spanish above — the `por que ...?` construction is a
        // real question opener, but `por` is an extremely common
        // Portuguese preposition (`por favor`, `por agora`, `por
        // enquanto`, `por isso`, `por aqui`, ...) and a FirstToken
        // match on `por` causes high-volume false positives on
        // declaratives. The `?` terminator is sufficient signal
        // for `por que ...?` (and the bare `porque` /
        // accented-`porquê` interrogative variants below cover
        // the cases where the preposition fuses into a single
        // word). See Devin Review finding #FLAG-0003.
        //
        // Deliberately omitted: bare `que`. Portuguese has the
        // accented `quê` (kept) as the canonical sentence-final
        // / stressed-position interrogative, but unaccented `que`
        // is overwhelmingly used as a relative pronoun /
        // conjunction / exclamation opener (`Que pena!` — "What a
        // shame!", `Que dia chato!` — "What a boring day!",
        // `Que ele venha amanhã` — "(I hope) he comes tomorrow",
        // `O livro que li`, ...). The FirstToken false-positive
        // surface is the same as French `que` and Italian `che`
        // (FLAG-0001c). The accented `quê` and the bare `o que`
        // (which tokenises to `o`, missed regardless) combined
        // with the `?` terminator on `que ...?` openers give
        // adequate recall. See Devin Review finding #FLAG-0001c.
        "pt" => Some((
            &[
                "quem", "quê", "qual", "quais", "quando", "onde", "aonde", "como", "porquê",
                "porque", "quanto", "quanta", "quantos", "quantas",
            ],
            InterrogativeMatch::FirstToken,
        )),

        // Italian — Serianni, Grammatica italiana §X.105
        // ("Pronomi e aggettivi interrogativi"). `perché` covers
        // both `why` and `because`; the question terminator
        // disambiguates downstream.
        //
        // Deliberately omitted: `che`. Italian has no orthographic
        // distinction between interrogative `che` (`Che fai?` —
        // "What are you doing?") and the high-frequency
        // conjunction / relative pronoun `che` (`Penso che sia
        // vero`, `Il libro che leggo`, `Che bello!` — exclamation,
        // `Che peccato!` — "What a pity!"). A FirstToken match on
        // bare `che` would mis-classify exclamations, embedded
        // clauses, and relative-pronoun openers as questions. The
        // bare interrogative `cosa` (`Cosa fai?` is exactly
        // equivalent to `Che fai?`) stays in the list and catches
        // the common interrogative form; combined with the `?`
        // terminator on `che ...?` openers, recall stays adequate.
        // Same class of bug as French `que` and Portuguese `que`.
        // See Devin Review finding #FLAG-0001c.
        "it" => Some((
            &[
                "chi", "cosa", "quando", "dove", "come", "perché", "quale", "quali", "quanto",
                "quanta", "quanti", "quante",
            ],
            InterrogativeMatch::FirstToken,
        )),

        // Russian — Academy Grammar of the Russian Language §1656
        // ("Вопросительные местоимения и наречия"). The Cyrillic
        // forms; case-folding handles uppercase Cyrillic correctly
        // via `char::to_lowercase`.
        "ru" => Some((
            &[
                "кто",
                "что",
                "когда",
                "где",
                "куда",
                "откуда",
                "почему",
                "зачем",
                "как",
                "какой",
                "какая",
                "какое",
                "какие",
                "который",
                "сколько",
            ],
            InterrogativeMatch::FirstToken,
        )),

        // Vietnamese — Diệp Quang Ban, Ngữ pháp tiếng Việt §IV.3
        // ("Đại từ nghi vấn"). Vietnamese is space-separated and
        // permits both sentence-initial and sentence-final
        // interrogatives; we use FirstToken which catches the
        // initial case and rely on `?`/`?` terminator for the
        // sentence-final case.
        //
        // Phase 1.1 (Devin Review finding #ANALYSIS-0004,
        // closing the deferred #FLAG-0002d): Vietnamese now uses
        // FirstBigram so the high-frequency leading prepositions
        // / conjunctions `tại` / `khi` / `vì` recover their
        // interrogative readings via the two-token collocations
        // (`tại sao` "why?", `khi nào` "when?", `vì sao` "why?")
        // without re-introducing the false positives the bare
        // forms caused (`Khi tôi đến...` "When I arrived...",
        // `Tại Hà Nội...` "At Hanoi...", `Vì tôi bận...`
        // "Because I'm busy..."). FirstBigram is a strict
        // superset of FirstToken — the bare unambiguous
        // interrogatives (`ai`, `gì`, `nào`, `đâu`, `bao`,
        // `sao`, `thế`) still match via the first-token arm; the
        // bigram entries `tại sao` / `khi nào` / `vì sao` match
        // via the bigram arm. Bigram entries are written with a
        // single ASCII space and checked against the space-joined
        // first two alphabetic tokens; see
        // [`crate::lexicon::first_alphabetic_bigram`].
        "vi" => Some((
            &[
                "ai",
                "gì",
                "nào",
                "đâu",
                "bao",
                "sao",
                "thế",
                "tại sao",
                "khi nào",
                "vì sao",
            ],
            InterrogativeMatch::FirstBigram,
        )),

        // Indonesian / Malay — Kridalaksana, Kelas Kata dalam
        // Bahasa Indonesia §11. Standard Indonesian + cross-applies
        // to Malay (Bahasa Melayu), which whatlang merges into
        // `Ind` for the trigram model.
        //
        // Deliberately omitted: `di` and `yang`. The compound
        // forms `di mana` ("where") and `yang mana` ("which one")
        // are real question openers, but `di` is one of the most
        // common Indonesian / Malay prepositions ("in / at" —
        // `Di Jakarta...`, `Di kantor...`, `Di rumah...`) and
        // `yang` is an extremely common relative pronoun
        // ("that / which" — `Yang penting...`, `Yang menarik...`,
        // `Yang lebih baik...`). A FirstToken match on either
        // would mis-classify a large fraction of declarative
        // sentences that happen to start with these high-frequency
        // function words. The bare interrogative `mana` is still
        // in the list, which catches the canonical
        // `Mana yang lebih baik?` form; the `?` terminator handles
        // the sentence-final cases (`Bagus, di mana?`) on its own.
        // See Devin Review finding #FLAG-0005b.
        "id" | "ms" => Some((
            &[
                "siapa",
                "apa",
                "kapan",
                "mengapa",
                "kenapa",
                "bagaimana",
                "mana",
                "berapa",
            ],
            InterrogativeMatch::FirstToken,
        )),

        // Arabic — Wright, Grammar of the Arabic Language §354
        // ("Interrogative particles and pronouns"). Modern
        // Standard Arabic; dialects use additional forms but
        // these cover MSA news / docs / formal IM.
        //
        // Phase 1.6: promoted from
        // [`InterrogativeMatch::FirstToken`] to
        // [`InterrogativeMatch::FirstTokenWithArabicClitics`] so
        // the productive Arabic proclitic-prefix forms recover
        // their interrogative readings:
        //
        // * `وكيف يمكنني المساعدة؟` ("and how can I
        //   help?") — `و` + `كيف`.
        // * `فمتى سنلتقي؟` ("then when do we meet?")
        //   — `ف` + `متى`.
        // * `بأي طريقة نفعل ذلك؟` ("in which way do we
        //   do that?") — `ب` + `أي`.
        // * `لمن هذا الكتاب؟` ("to whom is this
        //   book?") — `ل` + `من`.
        //
        // Pre-Phase-1.6 these all bypassed the interrogative
        // table (the first alphabetic token was the prefixed
        // form `وكيف` / `فمتى` / … which never appeared in
        // the table), so question detection relied entirely on
        // the `؟` terminator short-circuit. With the prefix-
        // peeling matcher the table is consulted even when the
        // terminator is missing (e.g. a question rendered with
        // an ASCII `?` or accidentally terminated with `.`).
        //
        // `أ` (interrogative yes/no hamza, single character) is
        // deliberately **omitted** from the table. It shares
        // its orthography with the prosthetic / radical hamza on
        // a large open class of common Arabic nouns and
        // pronouns (`أنا` "I", `أنت` "you-masc", `أب` "father",
        // `أم` "mother", `أحمد` proper-name, `أخ` "brother",
        // …), so including it would over-classify a large
        // fraction of Arabic declarative sentences as questions.
        // Yes/no questions with the hamza particle are
        // recovered instead via the `؟` terminator short-
        // circuit in [`crate::extractor::looks_like_question`].
        //
        // Sweep-1 precision update (Devin Review
        // #ANALYSIS-0004): the proclitic peel set was
        // narrowed from 8 to 6 entries; `ك` ("like/as") and
        // `س` ("will") were excluded after surfacing both
        // interrogative-path false positives (`كمن` ➜ `من`,
        // `سما` ➜ `ما`) and a more dangerous imperative-path
        // false positive (`سأرسل` "I will send" ➜ imperative
        // `أرسل`). See the docstring on
        // [`crate::lexicon::MatchStrategy::FirstTokenWithArabicClitics`]
        // for the full omission rationale on `ك` / `س` / `أ`.
        "ar" => Some((
            &[
                "من",
                "ما",
                "ماذا",
                "متى",
                "أين",
                "لماذا",
                "كيف",
                "أي",
                "أية",
                "كم",
                "هل",
            ],
            InterrogativeMatch::FirstTokenWithArabicClitics,
        )),

        // Hebrew — Glinert, Modern Hebrew: An Essential Grammar
        // (Routledge, 4th ed. 2005), §6.1 ("Question words").
        //
        // Hebrew interrogatives are typically sentence-initial
        // and lowercased (Hebrew has no case distinction). The
        // canonical set covers `מי` "who", `מה` "what",
        // `מתי` "when", `איפה` / `היכן` "where", `איך` "how",
        // `למה` / `מדוע` "why", `איזה` / `איזו` / `אילו`
        // "which" (masc / fem / pl), `כמה` "how much / many",
        // and the formal/written yes-no particle `האם`.
        //
        // Phase 1.7: Hebrew uses
        // [`InterrogativeMatch::FirstTokenWithHebrewClitics`]
        // so the productive clitic-prefixed forms recover their
        // interrogative reading via iterative prefix peeling:
        //
        // * `ומתי` ("and when?") — `ו` + `מתי`.
        // * `שמה` ("that what?") — `ש` + `מה`.
        // * `מאיזה` ("from which?") — `מ` + `איזה`.
        // * `לאיזה` ("to which?") — `ל` + `איזה`.
        // * `באיזה` ("in which?") — `ב` + `איזה`.
        //
        // Pre-Phase-1.7 these all bypassed the interrogative
        // table (the first alphabetic token was the prefixed
        // form `ומתי` / `שמה` / … which never appeared in the
        // table), so Hebrew question detection without an
        // explicit `?` terminator was limited to the bare
        // interrogative-initial pattern.
        //
        // Deliberately omitted: `כי` (conjunction "because /
        // that") — high-frequency function word that would
        // mis-classify declarative subordinate clauses
        // (`אמרתי כי...` "I said that...", `יודע כי...` "knows
        // that...") as questions. Same class of omission as
        // Spanish / Portuguese `por`, French / Italian bare
        // `que` / `che`, Indonesian / Malay `di` / `yang`.
        //
        // Deliberately omitted: bare `ה` (definite article and
        // also the surface form of the colloquial yes-no
        // question particle in some registers). Including bare
        // `ה` would over-classify every NP-initial sentence as
        // a question. The formal yes-no particle `האם`
        // (`ה` + `אם`) is kept as the unambiguous, written-
        // register interrogative form.
        "he" => Some((
            &[
                "מי",   // "who"
                "מה",   // "what"
                "מתי",  // "when"
                "איפה", // "where"
                "היכן", // "where (formal)"
                "איך",  // "how"
                "למה",  // "why"
                "מדוע", // "why (formal)"
                "איזה", // "which (masc)"
                "איזו", // "which (fem)"
                "אילו", // "which (pl)"
                "כמה",  // "how much / many"
                "האם",  // "if / whether" — yes-no particle (formal)
            ],
            InterrogativeMatch::FirstTokenWithHebrewClitics,
        )),

        // Hindi — McGregor, Outline of Hindi Grammar §3.5
        // ("Interrogative pronouns"). Devanagari script;
        // Devanagari case-folding is a no-op (no case in
        // Devanagari) so the substring forms below are stable.
        //
        // Substring (not FirstToken) for two reasons: (1) Hindi
        // freely permits non-initial interrogative placement
        // (`तुम कहाँ जा रहे हो?` — "where are you going?",
        // with the interrogative in the middle), and (2) the
        // FirstToken extractor tokeniser splits on every
        // non-alphabetic codepoint, but the Devanagari virama
        // `्` (U+094D, category Mn) is not Unicode-alphabetic,
        // so conjunct interrogatives like `क्या` ("what") would
        // tokenise to `क` + `या` and never match the literal.
        // Substring matching sidesteps both problems and still
        // catches the canonical initial-position questions.
        "hi" => Some((
            &[
                "कौन",
                "क्या",
                "कब",
                "कहाँ",
                "क्यों",
                "कैसे",
                "कौनसा",
                "कितना",
                "कितनी",
                "किसका",
            ],
            InterrogativeMatch::Substring,
        )),

        // Japanese — 国語学大辞典 entry for 疑問詞. CJK has no
        // word boundaries, so we use substring matching to catch
        // mid-sentence interrogatives like `今日は何曜日ですか` (the
        // `何` lands mid-sentence). The sentence-final particle
        // `か` is included as a strong question marker (the
        // canonical Japanese question construction).
        "ja" => Some((
            &[
                "何",
                "誰",
                "いつ",
                "どこ",
                "なぜ",
                "どう",
                "どの",
                "どれ",
                "どんな",
                "いくつ",
                "いくら",
                "なに",
                "ですか",
                "でしょうか",
            ],
            InterrogativeMatch::Substring,
        )),

        // Korean — 표준국어대사전 entries for 의문사. Korean
        // typically marks questions with the sentence-final
        // ending `-까` / `-니` / `-나`, but the interrogative
        // root can land anywhere in the clause. Substring match.
        "ko" => Some((
            &[
                "누구",
                "무엇",
                "언제",
                "어디",
                "왜",
                "어떻게",
                "어느",
                "어떤",
                "얼마",
                "몇",
                "무슨",
                "뭐",
            ],
            InterrogativeMatch::Substring,
        )),

        // Mandarin / Simplified + Traditional — 现代汉语词典
        // 7th ed. interrogative entries. Mandarin has no word
        // boundaries; the `吗` sentence-final particle is the
        // canonical yes/no question marker and is included for
        // catching `这是…吗` constructions.
        "zh" => Some((
            &[
                "什么",
                "谁",
                "何时",
                "什么时候",
                "哪里",
                "哪儿",
                "为什么",
                "怎么",
                "哪个",
                "哪些",
                "如何",
                // Deliberately omitted: bare `几` ("how many / a few").
                // Substring matching on the single character `几`
                // would mis-classify common non-interrogative
                // compounds containing it: `几乎` ("almost"),
                // `几何` ("geometry"), `几率` ("probability"),
                // `几个月` ("a few months" — indefinite quantifier,
                // not interrogative). The interrogative readings of
                // `几` always occur in tight collocations
                // (`几点了？`, `几岁？`, `星期几？`), so we surface the
                // unambiguous canonical forms below instead, plus
                // retain `多少` ("how many / how much") for general
                // quantity questions and `吗` as the canonical
                // yes/no particle. Same class of precision-vs-recall
                // call as the Romance / Indonesian / Vietnamese
                // omissions documented above. See Devin Review
                // finding #FLAG-0001d.
                "几点", // "what time"
                "几岁", // "how old"
                "多少",
                "吗",
                "什麼",
                "誰",
                "為什麼",
                "怎麼",
                "哪個",
                "嗎",
            ],
            InterrogativeMatch::Substring,
        )),

        // Thai — Royal Institute of Thailand Grammar §3.7
        // (สรรพนามคำถาม). No word boundaries — substring match.
        "th" => Some((
            &[
                "ใคร",
                "อะไร",
                "เมื่อไหร่",
                "เมื่อไร",
                "ที่ไหน",
                "ไหน",
                "ทำไม",
                "อย่างไร",
                "ยังไง",
                "อันไหน",
                "เท่าไหร่",
                "เท่าไร",
                "กี่",
                "ไหม",
            ],
            InterrogativeMatch::Substring,
        )),

        // Tibetan — Phase 1.5. Tibetan script uses the tsheg
        // (་, U+0F0B) as a syllable separator rather than a
        // word boundary; the interrogative root can land
        // anywhere in the clause. Sources: Goldstein, *The
        // New Tibetan-English Dictionary of Modern Tibetan*
        // (UC Press 2001) entries for ག ("which / what"),
        // སུ ("who"), and the regular interrogative compounds
        // built on those roots. Substring match because the
        // tsheg-segmented syllables do not align with
        // whitespace word boundaries.
        "bo" => Some((
            &[
                "སུ",    // "who"
                "ག་རེ",  // "what"
                "ག་གི",  // "which / what (alternative spelling)"
                "ནམ",   // "when"
                "ག་དུས", // "when (alternative)"
                "ག་པར", // "where"
                "ག་སར", // "where (alternative)"
                "ག་འདྲ", // "how / what kind"
                "ག་སྟེ",  // "how"
                "ཅིའི",   // "why" (literally "of what")
                "ག་ཚོད", // "how much / how many"
            ],
            InterrogativeMatch::Substring,
        )),

        // Khmer — Phase 1.5. Khmer script has no inter-word
        // whitespace; interrogatives can land anywhere in the
        // clause. Sources: Headley et al., *Khmer-English
        // Dictionary* (Dunwoody Press 1997) entries for
        // អ្វី ("what"), នរណា ("who"), etc.
        //
        // Deliberately omitted: bare `ទេ` (the sentence-final
        // negation + yes/no question particle). Under
        // substring matching `ទេ` collides with the very
        // common noun `ប្រទេស` ("country / nation") — the
        // first two codepoints of the compound are U+1791 +
        // U+17C1, identical to the particle — and would
        // mis-classify any sentence about a country
        // (`ប្រទេសកម្ពុជា`, `ប្រទេសបារាំង`, …) as a Question.
        // Same class of precision-vs-recall trade-off as the
        // Mandarin omission of bare `几` documented above.
        // The unambiguous wh-compounds below cover the
        // canonical interrogative shapes; the yes/no
        // construction in Khmer also commonly uses the
        // sentence-final `ឬទេ` ("or not?") which we could add
        // in a future sweep if a Phase-2 Khmer corpus needs
        // it.
        "km" => Some((
            &[
                "នរណា",   // "who"
                "អ្នកណា",   // "who" (informal / collective)
                "អ្វី",      // "what"
                "ពេលណា",  // "when"
                "កាលណា",  // "when (alternative)"
                "ឯណា",    // "where"
                "កន្លែងណា", // "where (alternative)"
                "ហេតុអ្វី",   // "why"
                "យ៉ាងណា",  // "how"
                "ដូចម្តេច",  // "how (alternative)"
                "មួយណា",   // "which one"
                "ប៉ុន្មាន",   // "how much / how many"
            ],
            InterrogativeMatch::Substring,
        )),

        // Myanmar / Burmese — Phase 1.5. Myanmar script has
        // no inter-word whitespace; interrogatives can land
        // anywhere in the clause. The sentence-final particle
        // လား is the canonical yes/no question marker (`-la`
        // or `-laa`) and is included to catch yes/no
        // constructions. Sources: Department of the Myanmar
        // Language Commission, *Myanmar-English Dictionary*
        // (Yangon 1993) entries for ဘယ် ("what / which")
        // and the regular compounds built on it.
        "my" => Some((
            &[
                "ဘယ်သူ",    // "who"
                "ဘာ",     // "what"
                "ဘယ်အရာ",  // "what (alternative)"
                "ဘယ်တုန်းက", // "when (past)"
                "ဘယ်အချိန်", // "when (general)"
                "ဘယ်နေရာ", // "where"
                "ဘယ်မှာ",   // "where (alternative)"
                "ဘာဖြစ်လို့", // "why"
                "ဘယ်လို",    // "how"
                "ဘယ်ဟာ",   // "which"
                "ဘယ်လောက်", // "how much / how many"
                "လား",    // canonical yes/no sentence-final particle
                "သလား",   // formal yes/no particle
            ],
            InterrogativeMatch::Substring,
        )),

        // Lao — Phase 1.5. Lao script is structurally
        // parallel to Thai: no inter-word whitespace,
        // interrogatives can appear anywhere. Sources:
        // Reinhorn, *Dictionnaire Laotien-Français*
        // (Larousse 2001) entries for ໃຜ ("who"),
        // ຫຍັງ ("what"), etc.
        //
        // Deliberately omitted: bare `ບໍ` (the sentence-final
        // yes/no question particle). Under substring matching
        // `ບໍ` (U+0E9A U+0ECD) is a strict 2-codepoint prefix
        // of the Lao negation particle `ບໍ່`
        // (U+0E9A U+0ECD U+0EC8, "not") and of the extremely
        // common nouns `ບໍລິສັດ` ("company") and `ບໍລິການ`
        // ("service") — every Lao negative sentence
        // (`ບໍ່ມີ`, `ບໍ່ແມ່ນ`, `ບໍ່ໄດ້`, …) and every clause
        // mentioning a company or a service would
        // mis-classify as a Question. Same class of
        // precision-vs-recall trade-off as the Khmer
        // omission of bare `ទេ` documented above and the
        // Mandarin omission of bare `几`. The unambiguous
        // wh-words below cover the canonical interrogative
        // shapes; the yes/no construction in Lao also
        // commonly uses the A-not-A form (e.g.
        // `ມີ...ບໍ່ມີ` "have or not have") which a future
        // sweep can add as a multi-token rule if a Phase-2
        // Lao corpus needs it.
        "lo" => Some((
            &[
                "ໃຜ",     // "who"
                "ຫຍັງ",    // "what"
                "ເມື່ອໃດ",  // "when"
                "ຕອນໃດ",  // "when (alternative)"
                "ຢູ່ໃສ",    // "where"
                "ບ່ອນໃດ",  // "where (alternative)"
                "ເປັນຫຍັງ", // "why"
                "ແນວໃດ",  // "how"
                "ຄືແນວໃດ", // "how (alternative)"
                "ໃດ",     // "which"
                "ອັນໃດ",   // "which one"
                "ເທົ່າໃດ",  // "how much / how many"
            ],
            InterrogativeMatch::Substring,
        )),

        _ => None,
    }
}

/// Convenience: matching strategy for a language. `None` if no
/// interrogative list is configured.
pub fn matching_strategy_for(primary_tag: &str) -> Option<InterrogativeMatch> {
    interrogatives_for(primary_tag).map(|(_, s)| s)
}

/// All primary BCP-47 subtags this module has interrogative
/// tables for. Useful for tests + diagnostics that want to assert
/// coverage.
pub const SUPPORTED_PRIMARY_TAGS: &[&str] = &[
    "en", "es", "fr", "de", "pt", "it", "ru", "vi", "id", "ms", "ar", "he", "hi", "ja", "ko", "zh",
    "th", "bo", "km", "my", "lo",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_first_token_strategy() {
        let (list, strat) = interrogatives_for("en").expect("english configured");
        assert!(list.contains(&"who"));
        assert!(list.contains(&"what"));
        assert!(list.contains(&"why"));
        assert_eq!(strat, InterrogativeMatch::FirstToken);
    }

    #[test]
    fn japanese_substring_strategy() {
        let (list, strat) = interrogatives_for("ja").expect("japanese configured");
        assert!(list.contains(&"何"));
        assert!(list.contains(&"ですか"));
        assert_eq!(strat, InterrogativeMatch::Substring);
    }

    #[test]
    fn korean_substring_strategy() {
        let (list, strat) = interrogatives_for("ko").expect("korean configured");
        assert!(list.contains(&"무엇"));
        assert!(list.contains(&"누구"));
        assert_eq!(strat, InterrogativeMatch::Substring);
    }

    #[test]
    fn mandarin_substring_with_yesno_particle() {
        let (list, strat) = interrogatives_for("zh").expect("mandarin configured");
        assert!(list.contains(&"什么"));
        assert!(list.contains(&"什麼"));
        assert!(list.contains(&"吗"));
        assert!(list.contains(&"嗎"));
        assert_eq!(strat, InterrogativeMatch::Substring);
    }

    #[test]
    fn arabic_first_token_with_clitics_strategy() {
        // Phase 1.6: Arabic moved from `FirstToken` to
        // `FirstTokenWithArabicClitics` so the productive
        // proclitic-prefix forms (`وكيف` = `و` + `كيف`,
        // `فمتى` = `ف` + `متى`, etc.) recover their interrogative
        // readings via iterative prefix peeling. The bare entries
        // remain unchanged; only the lookup STRATEGY was promoted.
        // The interrogative-hamza particle `أ` is deliberately
        // omitted from the table per the inline comment on the
        // `ar` arm of `interrogatives_for` (over-classification
        // risk on the open class of `أ`-initial declaratives).
        let (list, strat) = interrogatives_for("ar").expect("arabic configured");
        assert!(list.contains(&"من"));
        assert!(list.contains(&"كيف"));
        assert!(
            !list.contains(&"أ"),
            "Phase 1.6: the bare interrogative-hamza `أ` must NOT appear in the Arabic \
             interrogative table — see the dedicated-omission comment in interrogatives_for"
        );
        assert_eq!(strat, InterrogativeMatch::FirstTokenWithArabicClitics);
    }

    #[test]
    fn malay_aliases_indonesian() {
        let (id_list, _) = interrogatives_for("id").expect("indonesian configured");
        let (ms_list, _) = interrogatives_for("ms").expect("malay configured");
        assert_eq!(id_list.len(), ms_list.len());
        for w in id_list {
            assert!(ms_list.contains(w), "malay missing {w}");
        }
    }

    #[test]
    fn unknown_tag_returns_none() {
        assert!(interrogatives_for("xx").is_none());
        assert!(interrogatives_for("").is_none());
        // Region-tagged inputs are not auto-reduced — callers must
        // call `LanguageTag::primary()` first.
        assert!(interrogatives_for("en-US").is_none());
    }

    #[test]
    fn supported_primary_tags_matches_configured_set() {
        // Round-trip the SUPPORTED_PRIMARY_TAGS list against
        // interrogatives_for to make sure no entry is documented
        // but unbacked.
        for tag in SUPPORTED_PRIMARY_TAGS {
            assert!(
                interrogatives_for(tag).is_some(),
                "tag {tag} is in SUPPORTED_PRIMARY_TAGS but has no interrogative entry"
            );
        }
    }

    #[test]
    fn matching_strategy_for_helper_agrees_with_interrogatives_for() {
        for tag in SUPPORTED_PRIMARY_TAGS {
            let (_, full) = interrogatives_for(tag).unwrap();
            let helper = matching_strategy_for(tag).unwrap();
            assert_eq!(full, helper, "strategy mismatch for {tag}");
        }
    }

    #[test]
    fn substring_languages_are_cjk_thai_or_indic_brahmic() {
        // Defends the design contract: Substring is used for
        // every language whose script lacks unambiguous
        // whitespace-aligned word boundaries OR whose
        // combining marks (virama, tsheg, coeng, asat)
        // interfere with the FirstToken tokeniser and which
        // additionally permits non-initial interrogative
        // placement. As of Phase 1.5 this is:
        // - CJK / Thai (no inter-word whitespace at all)
        // - Hindi (Devanagari virama)
        // - Tibetan (tsheg syllable-separator, stacked
        //   consonants)
        // - Khmer (no whitespace, coeng-stacked consonants)
        // - Myanmar (no whitespace, asat / virama)
        // - Lao (no whitespace, combining vowel signs)
        //
        // Any future change to this invariant should be
        // intentional and update both this test and the
        // module-level docstring.
        let expected_substring: std::collections::HashSet<&str> =
            ["ja", "ko", "zh", "th", "hi", "bo", "km", "my", "lo"]
                .into_iter()
                .collect();
        for tag in SUPPORTED_PRIMARY_TAGS {
            let strat = matching_strategy_for(tag).unwrap();
            let is_substring = strat == InterrogativeMatch::Substring;
            assert_eq!(
                is_substring,
                expected_substring.contains(tag),
                "tag {tag}: substring expected={}, got strategy={:?}",
                expected_substring.contains(tag),
                strat
            );
        }
    }

    #[test]
    fn first_bigram_languages_are_vietnamese_only_for_now() {
        // FirstBigram is the Phase 1.1 strategy introduced for
        // languages whose canonical interrogatives include
        // two-token collocations whose bare leading token is too
        // high-frequency to use on its own. Today only Vietnamese
        // uses it (`tại sao` / `khi nào` / `vì sao`). Pin the
        // contract so adding a second FirstBigram language is an
        // intentional decision.
        let expected_first_bigram: std::collections::HashSet<&str> = ["vi"].into_iter().collect();
        for tag in SUPPORTED_PRIMARY_TAGS {
            let strat = matching_strategy_for(tag).unwrap();
            let is_first_bigram = strat == InterrogativeMatch::FirstBigram;
            assert_eq!(
                is_first_bigram,
                expected_first_bigram.contains(tag),
                "tag {tag}: first-bigram expected={}, got strategy={:?}",
                expected_first_bigram.contains(tag),
                strat
            );
        }
    }

    #[test]
    fn first_token_with_arabic_clitics_languages_are_arabic_only_for_now() {
        // Phase 1.6 sweep-4 (Devin Review #3331706213): the
        // proclitic-aware first-token strategy was introduced
        // specifically for the Arabic agglutinative-prefix
        // morphology (و / ف / ب / ل / ال / أل clitically attaching
        // to the next word). It is NOT a generic "FirstToken with
        // some script-specific prefix peeling" — the peel inventory
        // is Arabic-specific, references Arabic-only orthography,
        // and would over-peel or no-op on any non-Arabic script.
        //
        // Pin exclusivity to Arabic for the same reason the
        // sibling `first_bigram_languages_are_vietnamese_only_for_now`
        // test pins FirstBigram to Vietnamese: adding a second
        // FirstTokenWithArabicClitics language must be an
        // intentional decision that updates both the peel inventory
        // (today's `ARABIC_PROCLITIC_PREFIXES` constant is named
        // explicitly "ARABIC_*") AND this test in lockstep. The
        // most likely incoming candidates (Farsi `fa`, Urdu `ur`,
        // Sorani Kurdish `ckb`, Pashto `ps`, Sindhi `sd`) share
        // SOME proclitic patterns with Arabic but have script-
        // specific differences that would require a per-language
        // peel inventory — silently sharing Arabic's would
        // introduce false positives on those languages.
        let expected_clitic_aware: std::collections::HashSet<&str> = ["ar"].into_iter().collect();
        for tag in SUPPORTED_PRIMARY_TAGS {
            let strat = matching_strategy_for(tag).unwrap();
            let is_clitic_aware = strat == InterrogativeMatch::FirstTokenWithArabicClitics;
            assert_eq!(
                is_clitic_aware,
                expected_clitic_aware.contains(tag),
                "tag {tag}: clitic-aware expected={}, got strategy={:?} \
                 — FirstTokenWithArabicClitics must remain Arabic-only \
                 (see test comment for rationale)",
                expected_clitic_aware.contains(tag),
                strat
            );
        }
    }

    #[test]
    fn first_token_with_hebrew_clitics_languages_are_hebrew_only_for_now() {
        // Phase 1.7: the Hebrew clitic-aware first-token strategy
        // was introduced specifically for the Hebrew agglutinative-
        // prefix morphology (ו / ש / מ / ל / ב clitically attaching
        // to the next word). It is NOT a generic "FirstToken with
        // some script-specific prefix peeling" — the peel inventory
        // is Hebrew-specific (HEBREW_PROCLITIC_PREFIXES), references
        // Hebrew-only orthography, and would no-op on any non-
        // Hebrew script.
        //
        // Pin exclusivity to Hebrew for the same reason the sibling
        // `first_token_with_arabic_clitics_languages_are_arabic_only_for_now`
        // test pins the Arabic variant to Arabic: adding a second
        // FirstTokenWithHebrewClitics language must be an
        // intentional decision that updates both the peel inventory
        // (today's `HEBREW_PROCLITIC_PREFIXES` constant is named
        // explicitly "HEBREW_*") AND this test in lockstep.
        //
        // The most likely incoming candidates (Yiddish `yi`, Ladino
        // `lad`) share the Hebrew alphabet but have language-
        // specific spelling conventions:
        // * Yiddish uses digraph vowels (`ייִ`, `וֹ`, `ױ`, `יִ`) with
        //   different vowel semantics from Modern Hebrew, and
        //   different proclitic productivity (`אַז` "that" is a free
        //   word, not a proclitic; `דער`/`די`/`דאָס` are the definite
        //   articles, not the proclitic `ה`).
        // * Ladino retains historical orthographic features
        //   (rafe-marked letters, different niqqud placement) and
        //   different proclitic morphology (`la`/`el` analogues
        //   transliterated from Spanish).
        //
        // Silently sharing Modern Hebrew's peel inventory would
        // introduce false positives on those languages — they each
        // need their own peel inventory and exclusivity-test
        // membership.
        let expected_he_clitic_aware: std::collections::HashSet<&str> =
            ["he"].into_iter().collect();
        for tag in SUPPORTED_PRIMARY_TAGS {
            let strat = matching_strategy_for(tag).unwrap();
            let is_he_clitic_aware = strat == InterrogativeMatch::FirstTokenWithHebrewClitics;
            assert_eq!(
                is_he_clitic_aware,
                expected_he_clitic_aware.contains(tag),
                "tag {tag}: hebrew-clitic-aware expected={}, got strategy={:?} \
                 — FirstTokenWithHebrewClitics must remain Hebrew-only \
                 (see test comment for rationale)",
                expected_he_clitic_aware.contains(tag),
                strat
            );
        }
    }

    #[test]
    fn hebrew_first_token_with_clitics_strategy() {
        // Phase 1.7: Hebrew moved from non-existent (no entry) to
        // `FirstTokenWithHebrewClitics` so the productive proclitic-
        // prefix forms (`ומתי` = `ו` + `מתי`, `שמה` = `ש` + `מה`,
        // `מאיזה` = `מ` + `איזה`, …) recover their interrogative
        // readings via iterative prefix peeling. The bare entries
        // must include the canonical Hebrew question words.
        //
        // The bare definite article `ה` is deliberately omitted
        // per the inline comment on the `he` arm of
        // `interrogatives_for` (over-classification risk).
        let (list, strat) = interrogatives_for("he").expect("hebrew configured");
        assert!(list.contains(&"מי"));
        assert!(list.contains(&"מה"));
        assert!(list.contains(&"מתי"));
        assert!(list.contains(&"איפה"));
        assert!(list.contains(&"איך"));
        assert!(list.contains(&"למה"));
        assert!(list.contains(&"כמה"));
        assert!(list.contains(&"האם"));
        assert!(
            !list.contains(&"ה"),
            "Phase 1.7: the bare definite article `ה` must NOT appear in the Hebrew \
             interrogative table — see the dedicated-omission comment in interrogatives_for"
        );
        assert!(
            !list.contains(&"כי"),
            "Phase 1.7: the bare conjunction `כי` must NOT appear in the Hebrew interrogative \
             table — see the dedicated-omission comment in interrogatives_for"
        );
        assert_eq!(strat, InterrogativeMatch::FirstTokenWithHebrewClitics);
    }

    #[test]
    fn no_first_token_entry_contains_tokeniser_boundary_chars() {
        // Devin Review #BUG-0001: an interrogative entry that
        // contains a non-alphabetic character is unreachable
        // under the FirstToken strategy, because the extractor's
        // tokeniser splits on every non-alphabetic char. Guard
        // against future regressions by scanning every FirstToken
        // language's entries.
        for tag in SUPPORTED_PRIMARY_TAGS {
            let (list, strat) = interrogatives_for(tag).unwrap();
            // Phase 1.6 sweep-1 extension (Devin Review
            // #3331604782): the invariant applies to every
            // strategy whose matcher consults the extractor's
            // alphabetic-only tokeniser — i.e. both bare
            // FirstToken AND FirstTokenWithArabicClitics, which
            // begins with a FirstToken-style exact-equality
            // check and whose peel only strips alphabetic Arabic
            // proclitics (never tokeniser-boundary chars). Pre-
            // fix, the new Arabic strategy bypassed this guard
            // entirely; including it here pins the no-boundary-
            // char invariant for the Arabic interrogative table
            // as well. Substring / FirstBigram strategies remain
            // exempt because their entries either span multiple
            // tokens (`tại sao`) or are intentionally matched as
            // substrings (`何ですか`).
            //
            // Phase 1.7 extension: same invariant applies to the
            // Hebrew clitic-aware strategy by identical reasoning
            // — the peel strips only alphabetic Hebrew proclitics
            // (never tokeniser-boundary chars), so any non-
            // alphabetic char in a Hebrew interrogative entry
            // would be unreachable.
            if strat != InterrogativeMatch::FirstToken
                && strat != InterrogativeMatch::FirstTokenWithArabicClitics
                && strat != InterrogativeMatch::FirstTokenWithHebrewClitics
            {
                continue;
            }
            for entry in list {
                assert!(
                    entry.chars().all(char::is_alphabetic),
                    "tag {tag}: interrogative {entry:?} contains a non-alphabetic char \
                     and is unreachable under FirstToken matching"
                );
            }
        }
    }

    #[test]
    fn first_bigram_entries_are_alphabetic_with_one_internal_space() {
        // FirstBigram entries are either a single alphabetic
        // token (matched via the FirstToken arm) or two
        // alphabetic tokens space-joined by a single ASCII
        // space (matched via the bigram arm). Anything else
        // is unreachable under the FirstBigram matcher.
        for tag in SUPPORTED_PRIMARY_TAGS {
            let (list, strat) = interrogatives_for(tag).unwrap();
            if strat != InterrogativeMatch::FirstBigram {
                continue;
            }
            for entry in list {
                let space_count = entry.chars().filter(|c| *c == ' ').count();
                assert!(
                    space_count <= 1,
                    "tag {tag}: FirstBigram entry {entry:?} has more than one space \
                     (would never match a two-token bigram)"
                );
                let valid = entry
                    .split(' ')
                    .all(|part| !part.is_empty() && part.chars().all(char::is_alphabetic));
                assert!(
                    valid,
                    "tag {tag}: FirstBigram entry {entry:?} contains a non-alphabetic char or \
                     an empty part — the matcher would never reach it"
                );
            }
        }
    }

    #[test]
    fn no_entry_is_duplicated_within_a_language() {
        // Devin Review #INFO-0002: Vietnamese previously listed
        // `bao` twice. Guard against future cut-and-paste
        // duplications across every language.
        for tag in SUPPORTED_PRIMARY_TAGS {
            let (list, _) = interrogatives_for(tag).unwrap();
            let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for entry in list {
                assert!(
                    seen.insert(*entry),
                    "tag {tag}: entry {entry:?} appears more than once in the table"
                );
            }
        }
    }

    #[test]
    fn spanish_and_portuguese_omit_preposition_por() {
        // Devin Review #FLAG-0003: `por` is too common a
        // preposition in both languages to use as a FirstToken
        // question trigger. Guard against accidental re-addition.
        let (es_list, _) = interrogatives_for("es").unwrap();
        assert!(
            !es_list.contains(&"por"),
            "spanish list must not contain 'por' (high false-positive risk)"
        );
        let (pt_list, _) = interrogatives_for("pt").unwrap();
        assert!(
            !pt_list.contains(&"por"),
            "portuguese list must not contain 'por' (high false-positive risk)"
        );
    }

    #[test]
    fn french_omits_unreachable_est_ce_entry() {
        // Devin Review #BUG-0001: the FirstToken tokeniser would
        // split `est-ce` on the hyphen, so the entry was dead
        // code. Verify it stays removed.
        let (fr_list, _) = interrogatives_for("fr").unwrap();
        assert!(
            !fr_list.contains(&"est-ce"),
            "french list must not contain unreachable 'est-ce' entry"
        );
    }

    #[test]
    fn indonesian_malay_omits_high_frequency_prepositions() {
        // Devin Review #FLAG-0005b: `di` ("in / at") and `yang`
        // ("that / which") are extremely common Indonesian / Malay
        // function words. FirstToken matching on either would
        // mis-classify every declarative starting with the
        // preposition or the relative pronoun as a question. Guard
        // against accidental re-addition.
        for tag in ["id", "ms"] {
            let (list, _) = interrogatives_for(tag).unwrap();
            assert!(
                !list.contains(&"di"),
                "{tag} list must not contain high-frequency preposition 'di'"
            );
            assert!(
                !list.contains(&"yang"),
                "{tag} list must not contain high-frequency relative pronoun 'yang'"
            );
            // The bare `mana` should remain so `Mana yang lebih baik?`
            // and similar canonical openers still classify.
            assert!(
                list.contains(&"mana"),
                "{tag} list must still contain bare interrogative 'mana'"
            );
        }
    }

    #[test]
    fn romance_languages_omit_bare_que_che_function_words() {
        // Devin Review #FLAG-0001c: bare `que` (French, Portuguese)
        // and `che` (Italian) are far more common as relative
        // pronouns / conjunctions / exclamation openers than as
        // interrogatives, and the FirstToken strategy can't
        // distinguish the uses. Spanish is safe because the
        // orthography distinguishes interrogative `qué` (kept) from
        // conjunction `que` (never in the table). Portuguese keeps
        // accented `quê`. French / Italian have no such
        // distinction. Guard against accidental re-addition of the
        // unaccented forms.
        let (fr_list, _) = interrogatives_for("fr").unwrap();
        assert!(
            !fr_list.contains(&"que"),
            "french list must not contain bare 'que' (high false-positive risk on \
             relative-pronoun / subjunctive-opener / exclamation declaratives)"
        );
        assert!(
            fr_list.contains(&"quoi"),
            "french list must still contain interrogative-only 'quoi'"
        );

        let (pt_list, _) = interrogatives_for("pt").unwrap();
        assert!(
            !pt_list.contains(&"que"),
            "portuguese list must not contain bare 'que' (high false-positive risk on \
             relative-pronoun / subjunctive-opener / exclamation declaratives)"
        );
        assert!(
            pt_list.contains(&"quê"),
            "portuguese list must still contain accented interrogative 'quê'"
        );

        let (it_list, _) = interrogatives_for("it").unwrap();
        assert!(
            !it_list.contains(&"che"),
            "italian list must not contain bare 'che' (high false-positive risk on \
             relative-pronoun / conjunction / exclamation declaratives)"
        );
        assert!(
            it_list.contains(&"cosa"),
            "italian list must still contain bare interrogative 'cosa' (equivalent to 'che')"
        );

        // Spanish is the orthographically distinct case: `qué` is
        // kept, bare `que` is never present.
        let (es_list, _) = interrogatives_for("es").unwrap();
        assert!(
            es_list.contains(&"qué"),
            "spanish list must still contain accented interrogative 'qué'"
        );
        assert!(
            !es_list.contains(&"que"),
            "spanish list must not contain unaccented 'que' (only 'qué' is interrogative)"
        );
    }

    #[test]
    fn chinese_omits_ambiguous_numeral_几() {
        // Devin Review #FLAG-0001d: bare `几` is an ambiguous
        // morpheme \u2014 it is genuinely interrogative in
        // collocations like `几点了？`, `几岁？`, `星期几？`, but it
        // also appears in extremely common non-interrogative
        // compounds: `几乎` ("almost"), `几何` ("geometry"), `几率`
        // ("probability"), `几个月` (indefinite "a few months").
        // Under Substring matching, a single-character entry
        // mis-classifies any sentence containing those compounds
        // as a question. The replacement is to surface the
        // unambiguous canonical collocations (`几点`, `几岁`) and
        // rely on `多少` for general quantity questions plus `吗`
        // for yes/no.
        let (zh_list, strat) = interrogatives_for("zh").unwrap();
        assert_eq!(strat, InterrogativeMatch::Substring);
        assert!(
            !zh_list.contains(&"几"),
            "chinese list must not contain bare '几' (substring match would \
             mis-classify '几乎' / '几何' / '几率' / '几个月' declaratives as questions)"
        );
        // The replacements: tight collocations that are
        // unambiguously interrogative.
        assert!(
            zh_list.contains(&"几点"),
            "chinese list must contain canonical interrogative collocation '几点' (\"what time\")"
        );
        assert!(
            zh_list.contains(&"几岁"),
            "chinese list must contain canonical interrogative collocation '几岁' (\"how old\")"
        );
        // The general quantity fallback `多少` should remain so
        // `多少钱？` ("how much money?") and similar still classify.
        assert!(
            zh_list.contains(&"多少"),
            "chinese list must still contain general quantity interrogative '多少'"
        );
        // The yes/no particle `吗` should remain so any `…吗？`
        // sentence still classifies.
        assert!(
            zh_list.contains(&"吗"),
            "chinese list must still contain yes/no particle '吗'"
        );
        // Sanity-check a sample of other canonical interrogatives
        // to guard against accidental wholesale list edits.
        assert!(
            zh_list.contains(&"什么"),
            "chinese list must still contain interrogative '什么' (\"what\")"
        );
        assert!(
            zh_list.contains(&"为什么"),
            "chinese list must still contain interrogative '为什么' (\"why\")"
        );
    }

    #[test]
    fn vietnamese_omits_high_frequency_bare_conjunctions_but_keeps_bigrams() {
        // Devin Review #FLAG-0002d / #ANALYSIS-0004 (Phase 1.1):
        // `khi`, `tại`, `vì` are extremely common Vietnamese
        // conjunctions / prepositions whose interrogative
        // readings only manifest as part of bigrams (`khi nào`,
        // `tại sao`, `vì sao`). The bare forms remain absent so
        // `Khi tôi đến...` / `Tại Hà Nội...` / `Vì tôi bận...`
        // declaratives do not mis-classify, but Phase 1.1 added
        // the bigram entries themselves so `Tại sao bạn buồn?` /
        // `Khi nào chúng ta đi?` / `Vì sao trời mưa?` recover
        // their interrogative reading under FirstBigram.
        let (vi_list, strat) = interrogatives_for("vi").unwrap();
        assert_eq!(strat, InterrogativeMatch::FirstBigram);
        for bare in ["khi", "tại", "vì"] {
            assert!(
                !vi_list.contains(&bare),
                "vietnamese list must not contain bare high-frequency function word {bare:?} \
                 (matches all declaratives starting with the conjunction/preposition)"
            );
        }
        for bigram in ["tại sao", "khi nào", "vì sao"] {
            assert!(
                vi_list.contains(&bigram),
                "vietnamese list must contain bigram interrogative {bigram:?} (Phase 1.1 \
                 #ANALYSIS-0004 closure)"
            );
        }
        // The bare unambiguous interrogatives should remain so
        // sentence-initial `Ai là...?` / `Gì xảy ra...?` /
        // `Nào là...?` / `Đâu là...?` / `Sao thế?` / `Bao nhiêu?`
        // still classify (via the FirstToken arm of FirstBigram).
        for kept in ["ai", "gì", "nào", "đâu", "bao", "sao", "thế"] {
            assert!(
                vi_list.contains(&kept),
                "vietnamese list must still contain bare interrogative {kept:?}"
            );
        }
    }
}
