//! Language detection for observation ingestion.
//!
//! The substrate is multilingual (B2C and B2B tenants ingest chat,
//! mail, docs in dozens of languages). Every downstream consumer
//! that historically assumed English — the lexicon classifier in
//! [`evidence_store::ImportanceClassifier`], the lexicon extractor
//! in [`crate::extractor::LexiconExtractor`], the FTS5 tokenizer
//! selection in [`evidence_store::EvidenceStore`] — needs a
//! per-observation language tag to pick the right per-locale
//! assets.
//!
//! This module wraps the [`whatlang`] crate (pure Rust, MIT,
//! trigram-statistics based, no I/O, ~2 MB embedded model)
//! behind a thin substrate-shaped surface:
//!
//! * [`LanguageTag`] — newtype around a BCP-47 primary subtag
//!   (`"en"`, `"ja"`, `"zh"`, …). Constructed only via [`LanguageTag::new`]
//!   which normalises and rejects empty input.
//! * [`LanguageDetection`] — a `(tag, confidence, is_reliable)`
//!   triple returned by [`detect_language`].
//! * [`detect_language`] — `text -> Option<LanguageDetection>`.
//!   Returns `None` when the detector either refuses to classify
//!   (empty / pure-punctuation / pure-emoji input) or marks the
//!   result as unreliable. Callers MUST treat `None` as
//!   "language unknown" rather than substitute a default —
//!   silently mis-classifying short inputs as English would
//!   derail downstream lexicon / tokenizer selection.
//!
//! The mapping from whatlang's ISO 639-3 enum to BCP-47 primary
//! subtags is exhaustive across whatlang's 70 supported
//! languages; new whatlang releases that add languages we do not
//! yet have a tag for surface as `None` until [`whatlang_lang_to_bcp47`]
//! is extended — fail-closed rather than silently emit an
//! unmapped 3-letter code into [`crate::types::Observation::language_tag`].

use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// BCP-47 primary language subtag (e.g. `"en"`, `"ja"`, `"zh"`).
///
/// Stored as a small newtype so callers cannot accidentally pass a
/// raw String into a place that expects a normalised tag. Inputs
/// are trimmed and lower-cased on construction; equality and
/// hashing are case-sensitive on the normalised form.
///
/// ## Internal storage: `Arc<str>` (not `String`)
///
/// The inner field is an [`Arc<str>`] rather than a [`String`] so
/// that [`Clone`] is an O(1) refcount bump rather than a heap
/// allocation + memcpy of the underlying 2-5 byte tag. The
/// extractor clones the dominant `Option<LanguageTag>` once
/// per entity-class observation (6+ classes per call: `@mentions`,
/// capitalised words, URLs, emails, date refs, numeric refs), and
/// it also clones the per-sentence tag once per sentence in the
/// inner loop. With `String` storage every clone allocates on the
/// global allocator — measurable in bulk document ingestion. With
/// `Arc<str>` storage every clone is a relaxed atomic increment
/// on the strong-count. The public surface — `as_str`, `primary`,
/// `Display`, `PartialEq`, `Eq`, `Hash`, `Serialize`,
/// `Deserialize` — is unaffected because [`Arc<str>`] derefs to
/// [`str`] and inherits its equality / hashing semantics.
///
/// `Deserialize` is implemented by hand (rather than derived as
/// `#[serde(transparent)]`) so that round-tripping a tag through
/// any serde format — SQLCipher BLOB columns, observation JSON
/// payloads emitted by connectors, FFI bridges, structured log
/// records — enforces the exact same `trim` + lower-case + empty
/// check that [`LanguageTag::new`] applies. Without this an
/// adversarial or malformed payload (`"language_tag": ""`,
/// `"language_tag": " "`, `"language_tag": "EN-US"`) would
/// otherwise materialise as an un-normalised tag and silently
/// derail the per-locale lexicon / FTS5 tokenizer selection.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LanguageTag(Arc<str>);

impl Serialize for LanguageTag {
    /// Serialise transparently as the bare BCP-47 string (e.g.
    /// `"ja"`). We can't `#[derive(Serialize)]` with
    /// `#[serde(transparent)]` because `Arc<str>` does not
    /// implement [`Serialize`] by default — serde only implements
    /// it for `Arc<T>` when the `rc` feature is enabled, which the
    /// workspace deliberately does not enable (the substrate also
    /// uses `Arc` for in-flight, non-persisted values and we don't
    /// want every `Arc<T>` to silently become serializable).
    ///
    /// The implementation calls [`Serializer::serialize_str`]
    /// directly on the inner [`str`] rather than going through
    /// `<Arc<str> as Serialize>::serialize` or relying on Rust's
    /// method-resolution deref-coercion to find
    /// `<str as Serialize>::serialize`. Calling `serialize_str`
    /// directly removes any dependence on which crate features are
    /// enabled in the dependency graph — the wire form is locked
    /// to the bare JSON string regardless of whether a future
    /// dependency turns on serde's `rc` feature.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for LanguageTag {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        LanguageTag::new(&raw).ok_or_else(|| {
            serde::de::Error::custom(
                "language_tag must be a non-empty BCP-47 primary subtag after trim",
            )
        })
    }
}

impl LanguageTag {
    /// Construct a [`LanguageTag`] from a raw BCP-47 primary
    /// subtag. The input is trimmed and lower-cased; an
    /// effectively-empty input (whitespace only) is rejected
    /// (`None`).
    ///
    /// This does not validate the tag against the IANA registry —
    /// callers are expected to pass values that originated from
    /// either [`detect_language`] or an explicit user-supplied
    /// locale string ("en", "en-US", "ja", …).
    pub fn new(raw: impl AsRef<str>) -> Option<Self> {
        let lower = raw.as_ref().trim().to_ascii_lowercase();
        if lower.is_empty() {
            return None;
        }
        // `String -> Arc<str>` is a single allocation that copies
        // the bytes into the Arc inline; subsequent `Clone` calls
        // are O(1) refcount bumps rather than another heap copy.
        Some(Self(Arc::from(lower)))
    }

    /// Borrow the BCP-47 tag string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Extract the primary language subtag (the bit before the
    /// first `-`). For `"en-US"` this returns `"en"`; for `"ja"`
    /// it returns `"ja"`. Most substrate consumers
    /// (per-language lexicons, FTS5 tokenizer selection) only
    /// care about the primary subtag.
    pub fn primary(&self) -> &str {
        self.0.split('-').next().unwrap_or(&self.0)
    }
}

impl std::fmt::Display for LanguageTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One language-detection result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LanguageDetection {
    /// BCP-47 primary subtag (e.g. `"en"`, `"ja"`).
    pub tag: LanguageTag,
    /// Detector confidence in `0.0 ..= 1.0`.
    pub confidence: f64,
    /// Whether the detector judged the result reliable on its
    /// internal heuristic (corpus-size + entropy). Always `true`
    /// when this struct is returned by [`detect_language`] — the
    /// field is kept on the struct so callers can stash the
    /// reliability flag through serialisation if needed.
    pub is_reliable: bool,
}

/// Detect the primary language of `text`.
///
/// Returns `None` when the detector either refuses to classify
/// (empty / pure-punctuation / pure-emoji input) or marks the
/// result as unreliable (`is_reliable == false`), or when the
/// detected language has no canonical BCP-47 mapping in
/// [`whatlang_lang_to_bcp47`].
pub fn detect_language(text: &str) -> Option<LanguageDetection> {
    let info = whatlang::detect(text)?;
    if !info.is_reliable() {
        return None;
    }
    let tag = whatlang_lang_to_bcp47(info.lang())?;
    // Route through `LanguageTag::new` (rather than `LanguageTag(tag.to_string())`)
    // so the empty / whitespace guard is enforced on every
    // construction path. Defensive against a future addition to
    // `whatlang_lang_to_bcp47` that accidentally maps a variant
    // to `Some("")` — the substrate would refuse to classify
    // rather than emit an empty tag downstream. In practice every
    // current arm returns a non-empty static, so this is
    // unreachable today; we still pay the one branch to keep the
    // single-construction-path invariant honest.
    let tag = LanguageTag::new(tag)?;
    Some(LanguageDetection {
        tag,
        confidence: info.confidence(),
        is_reliable: true,
    })
}

/// Map a [`whatlang::Lang`] (ISO 639-3) to its BCP-47 primary
/// language subtag. Every current `whatlang` variant has an entry,
/// but the function is intentionally typed as `Option<&'static str>`
/// rather than `&'static str`: the wrapper signals that callers
/// (in [`detect_language`]) must surface "no mapping" as
/// `LanguageDetection = None` rather than fall back to a default,
/// and it lets us add `None` arms in the future without an API
/// break if `whatlang` adds languages whose BCP-47 mapping is
/// ambiguous (Cmn / Yue / Wuu all map to `zh`, etc.). The
/// exhaustive match below also ensures the next `whatlang` release
/// that introduces a new `Lang` variant fails compilation here
/// rather than silently emitting an unmapped 3-letter code.
#[allow(clippy::unnecessary_wraps)]
fn whatlang_lang_to_bcp47(lang: whatlang::Lang) -> Option<&'static str> {
    use whatlang::Lang;
    Some(match lang {
        Lang::Afr => "af",
        Lang::Aka => "ak",
        Lang::Amh => "am",
        Lang::Ara => "ar",
        Lang::Aze => "az",
        Lang::Bel => "be",
        Lang::Ben => "bn",
        Lang::Bul => "bg",
        Lang::Cat => "ca",
        Lang::Ces => "cs",
        Lang::Cmn => "zh",
        Lang::Cym => "cy",
        Lang::Dan => "da",
        Lang::Deu => "de",
        Lang::Ell => "el",
        Lang::Eng => "en",
        Lang::Epo => "eo",
        Lang::Est => "et",
        Lang::Fin => "fi",
        Lang::Fra => "fr",
        Lang::Guj => "gu",
        Lang::Heb => "he",
        Lang::Hin => "hi",
        Lang::Hrv => "hr",
        Lang::Hun => "hu",
        Lang::Hye => "hy",
        Lang::Ind => "id",
        Lang::Ita => "it",
        Lang::Jav => "jv",
        Lang::Jpn => "ja",
        Lang::Kan => "kn",
        Lang::Kat => "ka",
        Lang::Khm => "km",
        Lang::Kor => "ko",
        Lang::Lat => "la",
        Lang::Lav => "lv",
        Lang::Lit => "lt",
        Lang::Mal => "ml",
        Lang::Mar => "mr",
        Lang::Mkd => "mk",
        Lang::Mya => "my",
        Lang::Nep => "ne",
        Lang::Nld => "nl",
        Lang::Nob => "nb",
        Lang::Ori => "or",
        Lang::Pan => "pa",
        Lang::Pes => "fa",
        Lang::Pol => "pl",
        Lang::Por => "pt",
        Lang::Ron => "ro",
        Lang::Rus => "ru",
        Lang::Sin => "si",
        Lang::Slk => "sk",
        Lang::Slv => "sl",
        Lang::Sna => "sn",
        Lang::Spa => "es",
        Lang::Srp => "sr",
        Lang::Swe => "sv",
        Lang::Tam => "ta",
        Lang::Tel => "te",
        Lang::Tgl => "tl",
        Lang::Tha => "th",
        Lang::Tuk => "tk",
        Lang::Tur => "tr",
        Lang::Ukr => "uk",
        Lang::Urd => "ur",
        Lang::Uzb => "uz",
        Lang::Vie => "vi",
        Lang::Yid => "yi",
        Lang::Zul => "zu",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_tag() {
        assert!(LanguageTag::new("").is_none());
        assert!(LanguageTag::new("   ").is_none());
        assert!(LanguageTag::new("\n\t  \n").is_none());
    }

    #[test]
    fn normalises_case_and_trims() {
        let t = LanguageTag::new("  EN-us  ").unwrap();
        assert_eq!(t.as_str(), "en-us");
        assert_eq!(t.primary(), "en");
    }

    #[test]
    fn primary_subtag() {
        assert_eq!(LanguageTag::new("ja").unwrap().primary(), "ja");
        assert_eq!(LanguageTag::new("zh-Hant").unwrap().primary(), "zh");
        assert_eq!(LanguageTag::new("pt-BR").unwrap().primary(), "pt");
    }

    #[test]
    fn detects_english() {
        let det = detect_language("The migration ships next Friday and we approved the rollout plan in yesterday's review meeting.",
        )
        .expect("english should be reliably detected");
        assert_eq!(det.tag.as_str(), "en");
        assert!(det.confidence > 0.5);
    }

    #[test]
    fn detects_japanese() {
        // Long enough to be reliable.
        let det =
            detect_language("金曜日に移行をリリースしますので、レビュー会議で承認をお願いします。")
                .expect("japanese should be reliably detected");
        assert_eq!(det.tag.as_str(), "ja");
    }

    #[test]
    fn detects_chinese_mandarin() {
        let det = detect_language("我们决定下周五发布新的迁移计划，请大家在评审会议上确认。")
            .expect("mandarin should be reliably detected");
        assert_eq!(det.tag.as_str(), "zh");
    }

    #[test]
    fn detects_korean() {
        let det = detect_language(
            "다음 주 금요일에 새로운 마이그레이션을 배포할 예정이니 회의에서 검토 부탁드립니다.",
        )
        .expect("korean should be reliably detected");
        assert_eq!(det.tag.as_str(), "ko");
    }

    #[test]
    fn detects_spanish() {
        let det = detect_language("Aprobamos el plan de migración y enviaremos el comunicado a todo el equipo el próximo viernes.",
        )
        .expect("spanish should be reliably detected");
        assert_eq!(det.tag.as_str(), "es");
    }

    #[test]
    fn detects_french() {
        let det = detect_language("Nous avons décidé de déployer la nouvelle migration vendredi prochain et de présenter les résultats à toute l'équipe.",
        )
        .expect("french should be reliably detected");
        assert_eq!(det.tag.as_str(), "fr");
    }

    #[test]
    fn detects_german() {
        let det = detect_language("Wir haben beschlossen, die neue Migration am nächsten Freitag freizugeben und die Ergebnisse dem gesamten Team vorzustellen.",
        )
        .expect("german should be reliably detected");
        assert_eq!(det.tag.as_str(), "de");
    }

    #[test]
    fn detects_portuguese() {
        let det = detect_language("Decidimos lançar a nova migração na próxima sexta-feira e apresentar os resultados a toda a equipe.",
        )
        .expect("portuguese should be reliably detected");
        assert_eq!(det.tag.as_str(), "pt");
    }

    #[test]
    fn detects_arabic() {
        let det = detect_language(
            "قررنا إطلاق عملية الترحيل الجديدة يوم الجمعة المقبل وعرض النتائج على الفريق بأكمله.",
        )
        .expect("arabic should be reliably detected");
        assert_eq!(det.tag.as_str(), "ar");
    }

    #[test]
    fn detects_vietnamese() {
        let det = detect_language("Chúng tôi đã quyết định triển khai bản di chuyển mới vào thứ Sáu tuần sau và trình bày kết quả cho toàn đội.",
        )
        .expect("vietnamese should be reliably detected");
        assert_eq!(det.tag.as_str(), "vi");
    }

    #[test]
    fn detects_thai() {
        let det = detect_language("เราตัดสินใจที่จะปล่อยการย้ายระบบใหม่ในวันศุกร์หน้าและนำเสนอผลลัพธ์ให้กับทั้งทีม")
            .expect("thai should be reliably detected");
        assert_eq!(det.tag.as_str(), "th");
    }

    #[test]
    fn detects_indonesian() {
        let det = detect_language("Kami memutuskan untuk meluncurkan migrasi baru pada hari Jumat depan dan menyajikan hasilnya kepada seluruh tim.",
        )
        .expect("indonesian should be reliably detected");
        assert_eq!(det.tag.as_str(), "id");
    }

    #[test]
    fn rejects_unreliable_or_empty_input() {
        assert!(detect_language("").is_none());
        assert!(detect_language("    \n\t  ").is_none());
        // Pure punctuation / emoji — whatlang declines.
        assert!(detect_language("!!!").is_none());
        assert!(detect_language("👍").is_none());
    }

    #[test]
    fn detect_language_only_returns_reliable_results() {
        // `detect_language` filters out unreliable classifications
        // up front: any `Some(...)` it returns MUST carry
        // `is_reliable = true`. Iterate a handful of short /
        // ambiguous inputs and assert the post-condition holds
        // for whatever subset the detector chose to classify.
        for input in ["ok", "hi", "no", "yes", "the cat"] {
            if let Some(det) = detect_language(input) {
                assert!(
                    det.is_reliable,
                    "detect_language must only surface reliable results: {det:?}"
                );
                assert!(
                    (0.0..=1.0).contains(&det.confidence),
                    "confidence must be in [0, 1]: {det:?}"
                );
            }
        }
    }

    #[test]
    fn clone_shares_allocation_via_arc() {
        // : `LanguageTag` clones happen
        // in the extractor entity-class loops (~6+ per call) and in
        // the doc pipeline's per-chunk threading. The internal
        // representation was switched from `String` to `Arc<str>`
        // so that `Clone` is a refcount bump rather than a heap
        // allocation. Verify the contract by asserting that two
        // cloned tags share the same underlying byte allocation:
        // `str::as_ptr` returns the data pointer of the slice, and
        // for two `Arc<str>` clones it must point at the same
        // location (the single Arc'd buffer).
        let a = LanguageTag::new("ja").unwrap();
        let b = a.clone();
        assert_eq!(
            a.as_str().as_ptr(),
            b.as_str().as_ptr(),
            "LanguageTag::clone must share the underlying Arc<str> allocation \
             (got distinct data pointers — Clone is allocating again, which \
             defeats the Arc<str> refactor for an earlier review finding)"
        );
        // Equality + hashing still behave as `str`-based comparison
        // (a fresh-construction "ja" allocates a separate buffer
        // but must still compare equal).
        let c = LanguageTag::new("ja").unwrap();
        assert_eq!(a, c, "tags with identical content must compare equal");
        let mut hasher_a = std::collections::hash_map::DefaultHasher::new();
        let mut hasher_c = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&a, &mut hasher_a);
        std::hash::Hash::hash(&c, &mut hasher_c);
        assert_eq!(
            std::hash::Hasher::finish(&hasher_a),
            std::hash::Hasher::finish(&hasher_c),
            "tags with identical content must hash equal regardless of Arc identity"
        );
    }

    #[test]
    fn serde_roundtrip_language_tag() {
        let tag = LanguageTag::new("ja").unwrap();
        let json = serde_json::to_string(&tag).unwrap();
        // Serialize is `#[serde(transparent)]` so the wire form
        // is the bare inner string. The custom Deserialize impl
        // still funnels it through `LanguageTag::new` on the way
        // back in to enforce trim + lowercase + non-empty.
        assert_eq!(json, "\"ja\"");
        let decoded: LanguageTag = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, tag);
    }

    #[test]
    fn deserialize_normalises_and_rejects_invalid_tags() {
        // Mixed case round-trips to normalised lowercase rather than
        // materialising as `LanguageTag("EN-US")`. This is the
        // invariant the custom Deserialize impl is defending: the
        // SQLCipher row, FFI bridge, connector JSON payload, or
        // structured log entry that hands us back a tag MUST end up
        // bit-equal to what `LanguageTag::new()` would have produced
        // on the same raw string.
        let decoded: LanguageTag = serde_json::from_str("\"EN-US\"").unwrap();
        assert_eq!(decoded.as_str(), "en-us");
        assert_eq!(decoded.primary(), "en");

        // Surrounding whitespace is trimmed.
        let decoded: LanguageTag = serde_json::from_str("\"  ja  \"").unwrap();
        assert_eq!(decoded.as_str(), "ja");

        // Empty / whitespace-only inputs are rejected (rather than
        // becoming `LanguageTag("")` which would semantically
        // collide with the "language unknown" state that callers
        // represent as `Option<LanguageTag>::None`).
        for empty in ["\"\"", "\"   \""] {
            let res: Result<LanguageTag, _> = serde_json::from_str(empty);
            assert!(
                res.is_err(),
                "deserialising empty / whitespace-only input must fail: {empty}"
            );
        }
    }
}
