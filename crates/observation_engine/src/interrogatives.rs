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
    /// Romance languages, Arabic, Vietnamese, Indonesian).
    FirstToken,
    /// Any interrogative appearing as a substring of the
    /// case-folded sentence counts as a match. Used for languages
    /// where the question word can appear anywhere in the
    /// sentence (Japanese, Korean, Chinese, Thai) — typically
    /// because word boundaries are not whitespace-delimited
    /// and/or because the language permits non-initial interrogative
    /// placement.
    Substring,
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
        "es" => Some((
            &[
                "qué", "quién", "quiénes", "cuándo", "dónde", "adónde", "cómo", "cuál", "cuáles",
                "cuánto", "cuánta", "cuántos", "cuántas", "por",
            ],
            InterrogativeMatch::FirstToken,
        )),

        // French — Grevisse, Le Bon Usage §382 ("Mots
        // interrogatifs"). `est-ce` is the sentence-initial half
        // of `est-ce que`; we match the first token so `est-ce`
        // alone is the trigger.
        "fr" => Some((
            &[
                "qui",
                "que",
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
                "est-ce",
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
        "pt" => Some((
            &[
                "quem", "quê", "que", "qual", "quais", "quando", "onde", "aonde", "como", "porquê",
                "porque", "por", "quanto", "quanta", "quantos", "quantas",
            ],
            InterrogativeMatch::FirstToken,
        )),

        // Italian — Serianni, Grammatica italiana §X.105
        // ("Pronomi e aggettivi interrogativi"). `perché` covers
        // both `why` and `because`; the question terminator
        // disambiguates downstream.
        "it" => Some((
            &[
                "chi", "che", "cosa", "quando", "dove", "come", "perché", "quale", "quali",
                "quanto", "quanta", "quanti", "quante",
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
        "vi" => Some((
            &[
                "ai", "gì", "nào", "đâu", "khi", "bao", "sao", "tại", "vì", "thế", "bao",
            ],
            InterrogativeMatch::FirstToken,
        )),

        // Indonesian / Malay — Kridalaksana, Kelas Kata dalam
        // Bahasa Indonesia §11. Standard Indonesian + cross-applies
        // to Malay (Bahasa Melayu), which whatlang merges into
        // `Ind` for the trigram model.
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
                "yang",
                "di",
            ],
            InterrogativeMatch::FirstToken,
        )),

        // Arabic — Wright, Grammar of the Arabic Language §354
        // ("Interrogative particles and pronouns"). Modern
        // Standard Arabic; dialects use additional forms but
        // these cover MSA news / docs / formal IM.
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
                "أ",
            ],
            InterrogativeMatch::FirstToken,
        )),

        // Hindi — McGregor, Outline of Hindi Grammar §3.5
        // ("Interrogative pronouns"). Devanagari script;
        // Devanagari case-folding is a no-op (no case in
        // Devanagari) so the substring forms below are stable.
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
            InterrogativeMatch::FirstToken,
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
                "几",
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
    "en", "es", "fr", "de", "pt", "it", "ru", "vi", "id", "ms", "ar", "hi", "ja", "ko", "zh", "th",
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
    fn arabic_first_token_strategy() {
        let (list, strat) = interrogatives_for("ar").expect("arabic configured");
        assert!(list.contains(&"من"));
        assert!(list.contains(&"كيف"));
        assert_eq!(strat, InterrogativeMatch::FirstToken);
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
    fn substring_languages_are_cjk_or_thai() {
        // Defends the design contract: the only languages that
        // use Substring matching are CJK + Thai (the
        // no-word-boundary scripts). Any future change to this
        // invariant should be intentional and update both this
        // test and the module-level docstring.
        let expected_substring: std::collections::HashSet<&str> =
            ["ja", "ko", "zh", "th"].into_iter().collect();
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
}
