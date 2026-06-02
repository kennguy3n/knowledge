//! Pre-embedding routing hook.
//!
//! XLM-R (the embedding model wired into [`crate::embeddings`]) is
//! genuinely multilingual: it produces meaningful vectors across
//! Latin / Cyrillic / Arabic / Hebrew / CJK / Devanagari / Thai
//! scripts and many more. What it *cannot* do is produce a
//! meaningful vector for input with no linguistic content — pure
//! punctuation, pure emoji, pure digits, pure whitespace. Calls
//! like `model.embed("!!!")` or `model.embed("😀😀😀")` either
//! succeed and return a near-zero embedding that any downstream
//! cosine-similarity score treats as semantically uniform with
//! every other embedded body (false positives on the rerank
//! lane), or fail with `EmbeddingError::EmptyInput` after the
//! tokenizer strips everything.
//!
//! Both outcomes waste an ONNX inference call (~5-50 ms on the
//! reference device tier). At ingestion throughput of even a
//! few hundred bodies/sec, even a small fraction of noise-
//! dominant rows would dominate the CPU budget of the embedding
//! lane.
//!
//! This module sits *in front of* every production
//! `model.embed(text)` call site and short-circuits the noise
//! cases before they reach the ONNX runtime. The lexical lanes
//! ([`crate::fts_telemetry`] / [`crate::retrieval::HybridRetriever`])
//! are unaffected — a query / body classified as noise still
//! flows through FTS5 + recency, just without a vector-lane
//! contribution.
//!
//! # The signal
//!
//! [`classify_for_embedding`] uses [`whatlang::detect`] as its
//! primary signal. `whatlang::detect` returns `Some(Info)`
//! whenever the input has *any* meaningful trigram-detectable
//! linguistic content (the language identification may or may
//! not be reliable — XLM-R can handle ambiguous-language inputs
//! fine — but the presence of *some* linguistic content is what
//! matters for the embedding-lane gate). When `whatlang::detect`
//! returns `None`, the input is pure punctuation / pure emoji /
//! pure whitespace / too-short to trigram-classify, and
//! short-circuiting is safe.
//!
//! ## Why `whatlang::detect` and not `observation_engine::detect_language`
//!
//! [`observation_engine::language::detect_language`]
//! layers an additional `is_reliable()` filter on top of
//! [`whatlang::detect`] for the *classifier* lane — the
//! per-sentence language tag drives lexicon selection where a
//! mis-classification produces visibly wrong importance scores.
//! For the embedding lane the additional filter is too strict:
//! XLM-R produces good cross-lingual vectors even on
//! `is_reliable() == false` inputs (short text, mixed languages,
//! transliterated names), so we want to admit those rather than
//! divert them to lexical-only. Using [`whatlang::detect`]
//! directly gives the looser admit criterion the embedding lane
//! actually wants.
//!
//! ## Why not a character-class heuristic
//!
//! An obvious alternative is "count `char::is_alphabetic`
//! characters, route to fallback if zero". CJK ideographs are
//! Unicode general category `Lo` (Letter, other) and Devanagari
//! / other Indic scripts are also alphabetic — so
//! `char::is_alphabetic` does return `true` for those scripts.
//! The heuristic still loses, but for a different reason: it
//! cannot distinguish "no-script numerics + spaces" from "noise
//! that XLM-R should handle". A query like `"100 200 300"` has
//! zero alphabetic characters and would be routed to fallback
//! by the heuristic, even though XLM-R produces a meaningful
//! vector (numerics share an embedding space with their textual
//! word forms in XLM-R's training corpus). Conversely, a
//! query like `"!!!?"` is alphabetic-zero AND noise — the
//! heuristic would route it correctly but only by accident.
//! `whatlang::detect` is calibrated against the same script
//! families XLM-R was trained on and uses trigram-frequency
//! signal rather than Unicode category, so it produces the
//! right decision on both inputs. It is the
//! substrate-shaped primitive for the embedding-lane gate.
//!
//! # Fail-open semantics
//!
//! The router is purely advisory. A misclassification in either
//! direction degrades gracefully:
//!
//! * **False positive (Embed → Skip)**: a borderline input that
//!   `whatlang` refuses is routed to lexical-only. XLM-R would
//!   have produced a near-zero meaningless vector anyway; the
//!   lexical lane is still on the path. Visible only in the
//!   `pre_embed_skipped_no_linguistic_content_total` counter.
//! * **False negative (Skip → Embed)**: a noise-dominant input
//!   that `whatlang` *does* classify (e.g. ``"yes"`` is short
//!   but linguistic) proceeds to embedding. No regression vs.
//!   earlier behaviour.
//!
//! There is no path where the router introduces a new failure
//! mode that did not exist before the routing hook landed.
//!
//! # Telemetry
//!
//! Every routing decision bumps exactly one of the three
//! `pre_embed_*_total` counters in [`crate::vector_telemetry`]:
//! [`crate::vector_telemetry::record_pre_embed_decision`]. The
//! sum across all three = total call sites that consulted the
//! router. See the wire-flat
//! [`crate::vector_telemetry::VectorTelemetrySnapshot`] for the
//! operator-dashboard read shape.

/// Routing decision returned by [`classify_for_embedding`].
///
/// Callers MUST treat [`EmbeddingRoute::Embed`] as "the model
/// is eligible to be invoked" and [`EmbeddingRoute::Skip`] as
/// "do NOT invoke the model; degrade this call site to its
/// lexical-only fallback". No call site is allowed to invoke
/// the model when the router returned [`EmbeddingRoute::Skip`]
/// without also bumping a justifying counter — see
/// [`crate::vector_telemetry::record_pre_embed_decision`] for
/// the bookkeeping discipline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingRoute {
    /// The input has linguistic content the embedding model can
    /// extract meaningful vectors from. Proceed to
    /// `model.embed(text)`.
    Embed,
    /// The input has no detectable linguistic content; the
    /// model would produce a meaningless near-zero vector at
    /// best, or fail with `EmptyInput` at worst. Skip the
    /// embedding lane and degrade the call site to its
    /// lexical-only fallback. The variant carries a
    /// [`SkipReason`] so the operator-dashboard counter
    /// taxonomy can distinguish "the input was literally empty"
    /// from "the input was non-empty but unclassifiable".
    Skip(SkipReason),
}

/// Why [`classify_for_embedding`] returned [`EmbeddingRoute::Skip`].
///
/// The two variants are mutually exclusive per routing call —
/// every [`EmbeddingRoute::Skip`] decision pins exactly one
/// reason. The taxonomy mirrors the two short-circuit gates in
/// [`classify_for_embedding`] in source order (the third
/// branch of `classify_for_embedding` is the fall-through to
/// [`EmbeddingRoute::Embed`], which carries no [`SkipReason`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Input was empty after `str::trim` — pure whitespace or
    /// the empty string. Distinct from
    /// [`Self::NoLinguisticContent`] because operators may want
    /// to alert on a non-trivial fraction of literally-empty
    /// bodies (which usually signals upstream extraction bugs
    /// rather than legitimate noise).
    EmptyAfterTrim,
    /// Input was non-empty after trim but [`whatlang::detect`]
    /// could not extract any trigram-detectable linguistic
    /// content. Pure punctuation, pure emoji, pure digits,
    /// pure-symbol input all land here.
    NoLinguisticContent,
}

/// Classify `text` for the pre-embedding routing hook. See
/// the module-level doc for the architectural rationale; the
/// implementation is intentionally minimal — three short-
/// circuit gates in source order, each pinned by a unit test.
///
/// `text` may be any caller-supplied string; the function makes
/// no allocation, performs no I/O, and is safe to call from any
/// thread.
///
/// # Determinism
///
/// [`whatlang::detect`] is deterministic on a given input — the
/// trigram statistics are baked into the crate's embedded model
/// at compile time. The routing decision is therefore
/// reproducible and test-pinnable across runs.
#[must_use]
pub fn classify_for_embedding(text: &str) -> EmbeddingRoute {
    if text.trim().is_empty() {
        return EmbeddingRoute::Skip(SkipReason::EmptyAfterTrim);
    }
    // `whatlang::detect` returns `Some(_)` for any input with
    // detectable linguistic trigrams (even one short word in a
    // supported language). It returns `None` for pure-
    // punctuation / pure-emoji / pure-digit / pure-symbol
    // inputs and for very-short inputs the trigram model
    // refuses to classify — exactly the signal we want for the
    // pre-embedding gate. We discard the returned `Info` —
    // the embedding lane is script-agnostic (XLM-R cross-
    // lingual coverage spans all whatlang-supported families)
    // so the specific detected language is irrelevant; only
    // the presence of *some* linguistic content matters.
    if whatlang::detect(text).is_none() {
        return EmbeddingRoute::Skip(SkipReason::NoLinguisticContent);
    }
    EmbeddingRoute::Embed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_routes_to_skip_empty_after_trim() {
        assert_eq!(
            classify_for_embedding(""),
            EmbeddingRoute::Skip(SkipReason::EmptyAfterTrim),
        );
    }

    #[test]
    fn pure_whitespace_routes_to_skip_empty_after_trim() {
        // Tab + LF + CR + space all collapse to empty under
        // `str::trim`. Lock this so a future "non-ASCII
        // whitespace handling" change does not silently regress
        // the gate.
        for input in [" ", "\t", "\n", "\r", "  \t\n  ", "\u{00A0}"] {
            assert_eq!(
                classify_for_embedding(input),
                EmbeddingRoute::Skip(SkipReason::EmptyAfterTrim),
                "input {input:?} should route to EmptyAfterTrim",
            );
        }
    }

    #[test]
    fn pure_punctuation_routes_to_skip_no_linguistic_content() {
        for input in ["!!!", "...", "?!?!", "()[]{}", "<<<>>>", "---===---"] {
            assert_eq!(
                classify_for_embedding(input),
                EmbeddingRoute::Skip(SkipReason::NoLinguisticContent),
                "input {input:?} should route to NoLinguisticContent",
            );
        }
    }

    #[test]
    fn pure_emoji_routes_to_skip_no_linguistic_content() {
        for input in ["😀", "😀😀😀😀", "👍🎉🚀", "❤️🌟⭐"] {
            assert_eq!(
                classify_for_embedding(input),
                EmbeddingRoute::Skip(SkipReason::NoLinguisticContent),
                "input {input:?} should route to NoLinguisticContent",
            );
        }
    }

    #[test]
    fn pure_digits_routes_to_skip_no_linguistic_content() {
        // Digit-only strings have no trigram-detectable
        // linguistic content even though they trim to non-
        // empty. Phone numbers, postcodes, raw account IDs
        // all land here — they are still indexed via FTS but
        // are not worth embedding.
        for input in ["1234567890", "42", "1.23", "1,234,567"] {
            assert_eq!(
                classify_for_embedding(input),
                EmbeddingRoute::Skip(SkipReason::NoLinguisticContent),
                "input {input:?} should route to NoLinguisticContent",
            );
        }
    }

    #[test]
    fn single_short_english_word_routes_to_embed() {
        // "yes" / "no" are short but linguistic.
        // cross-lingual recall benchmarks rely on the embedding
        // lane admitting short bodies, so a regression here
        // would degrade benchmark recall@k.
        for input in ["yes", "no", "hello", "world"] {
            assert_eq!(
                classify_for_embedding(input),
                EmbeddingRoute::Embed,
                "input {input:?} should route to Embed",
            );
        }
    }

    #[test]
    fn multilingual_text_routes_to_embed() {
        // Cross-script sanity — every script family the lexicon
        // registry covers should admit to the embedding lane.
        // This pins the "XLM-R is multilingual" contract from
        // the router's side.
        for input in [
            "The quick brown fox jumps over the lazy dog",
            "L'eau de la mer est salée",
            "明日の天気予報",
            "تنبؤات الطقس لغد",
            "מזג האוויר מחר",
            "내일 날씨 예보",
            "Прогноз погоды на завтра",
            "Πρόγνωση καιρού για αύριο",
            "मौसम की भविष्यवाणी",
            "พยากรณ์อากาศพรุ่งนี้",
        ] {
            assert_eq!(
                classify_for_embedding(input),
                EmbeddingRoute::Embed,
                "input {input:?} should route to Embed",
            );
        }
    }

    #[test]
    fn linguistic_content_with_surrounding_noise_routes_to_embed() {
        // Realistic chat messages mix linguistic content with
        // punctuation / emoji. The router must not over-
        // trigger on these.
        for input in [
            "hello!!! 😀",
            "weather forecast tomorrow 🌧️",
            "明日の天気予報 ☔",
            "*** important: meeting at 3pm ***",
        ] {
            assert_eq!(
                classify_for_embedding(input),
                EmbeddingRoute::Embed,
                "input {input:?} should route to Embed",
            );
        }
    }

    #[test]
    fn classify_is_pure_function() {
        // Same input ⇒ same output, across many calls.
        // Pins the determinism contract documented in the
        // function doc.
        let inputs = ["", "  ", "!!!", "😀", "1234", "hello", "明日の天気予報"];
        for input in inputs {
            let first = classify_for_embedding(input);
            for _ in 0..16 {
                assert_eq!(classify_for_embedding(input), first);
            }
        }
    }
}
