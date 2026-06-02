//! Cross-lingual recall benchmark suite.
//!
//! The multilingual stack wires XLM-R into the vector lane and
//! validates cross-lingual semantic clustering with three
//! integration tests (English ↔ Japanese weather, French ↔ Spanish
//! weather, English ↔ Spanish via rerank). Those tests pin the
//! *minimal* invariant — that the embedding lane is not
//! script-segregated — but they don't measure the *quality* of the
//! cross-lingual clustering at any meaningful scale.
//!
//! This benchmark suite lifts those three spot-checks into a
//! fixture-driven measurement:
//!
//! * **Concept inventory** — 10 named knowledge concepts (weather,
//!   finance, cooking, sports, technology, travel, music, health,
//!   education, family).
//! * **Language inventory** — 12 BCP-47 tags spanning the four
//!   script families exercised by the multilingual stack (Latin,
//!   CJK / Han, Arabic / Hebrew RTL, Indic / SEA scripts):
//!   `en`, `ja`, `zh`, `ko`, `es`, `fr`, `de`, `ar`, `he`, `hi`,
//!   `vi`, `th`.
//! * **Corpus** — the 10 × 12 = 120-entry concept-paraphrase matrix
//!   ingested into a fresh [`EvidenceStore`] before each metric
//!   measurement.
//! * **Queries** — one query per (concept × language) cell, also
//!   120 total, each labelled with the expected concept ID so the
//!   benchmark can compute recall against ground truth without
//!   per-query manual labelling.
//! * **Metrics** — two complementary IR measurements per query:
//!
//!   * **`recall@k`** — the fraction of the query's relevant
//!     documents (all 12 same-concept paraphrases) that appear
//!     in the top-k results: `|relevant ∩ top_k| / |relevant|`.
//!     Useful for `k = |relevant set| = 12` (the "did we recover
//!     the full cross-lingual cluster" invariant).  By definition,
//!     `recall@k` is bounded by `k / |relevant|` for any
//!     ranker — e.g. `recall@1 ≤ 1/12 ≈ 0.0833` even for a
//!     perfect ranker, since you can only put one document in
//!     the top slot.
//!
//!   * **`hit-rate@k`** — `1.0` if `top_k` contains *any*
//!     relevant document, else `0.0`.  Useful for `k = 1, 3`
//!     (the "is the top result actually about the query’s
//!     concept" invariant).  With the deterministic mock below
//!     all 12 same-concept paraphrases tie at vector-score
//!     `1.0` so `hit-rate@1` is `1.0` for every query.
//!
//!   The two metrics together pin both the "top results are
//!   relevant" and "all relevant docs are recovered" sides of
//!   the cross-lingual recall invariant — conflating them into
//!   a single number would mask asymmetric regressions (a
//!   ranker that always promotes one same-concept doc to the
//!   top but misses the rest would score perfect `hit-rate@1`
//!   and terrible `recall@12`).
//!
//! ## What this benchmark catches that ad-hoc spot-checks miss
//!
//! 1. **Per-language-pair regressions** — a future change that
//!    breaks cross-lingual clustering for one specific direction
//!    (e.g. `ar → he` after a tokeniser tweak) would still pass
//!    the three ad-hoc spot-checks but would drop the
//!    `recall@k` for that one row of the matrix below the pinned
//!    floor.
//! 2. **Asymmetric recall** — XLM-R has known asymmetries
//!    (an Arabic query → English body recall is not necessarily
//!    equal to the reverse).  The benchmark walks both directions
//!    of every language pair, so any asymmetric regression is
//!    captured.
//! 3. **Model-swap validation** — the benchmark is fixture-driven
//!    and model-agnostic.  When the deployed XLM-R is swapped for
//!    a different multilingual embedding (e5-multilingual,
//!    multilingual-MiniLM, LaBSE, etc.), running this benchmark
//!    against the real adapter (with `models/<new>.onnx` plumbed
//!    in) pins the same `recall@k` invariants and surfaces any
//!    regression as a quantitative delta rather than a subjective
//!    eyeballing of one or two queries.
//!
//! ## Why a mock instead of the real ONNX adapter
//!
//! The real XLM-R adapter requires the ONNX runtime + the
//! `models/xlm-r-base.onnx` artifact (~1.1 GB) — not part of the
//! standard CI image.  The mock below simulates XLM-R's defining
//! property (cross-lingual semantic clustering) by mapping every
//! concept to its own orthogonal unit-vector axis, so cosine
//! similarity between paraphrases of the same concept is exactly
//! `1.0` and between unrelated concepts is exactly `0.0`.  The
//! benchmark logic (corpus shape, query fixture, recall@k driver)
//! is the same whether the embedding model is the mock here or
//! the real XLM-R adapter loaded by an operator — the mock just
//! lets the test run in CI without the model artifact.
//!
//! ### Cosine similarity vs `vector_score`
//!
//! Note that the retriever does not surface raw cosine similarity;
//! it projects cosine `[-1.0, 1.0]` into a retrieval-friendly
//! `[0.0, 1.0]` `vector_score` via
//! [`evidence_store::embeddings::similarity_to_score`] —
//! `f64::midpoint(cos, 1.0).clamp(0.0, 1.0)`.  So under the mock:
//!
//! * same-concept paraphrases (cos sim = 1.0) → `vector_score = 1.0`
//! * unrelated concepts (cos sim = 0.0) → `vector_score = 0.5`
//! * opposite vectors (cos sim = -1.0) → `vector_score = 0.0`
//!
//! The benchmark's `recall@12` / `hit-rate@k` assertions are pinned
//! on the rank order — same-concept docs (score 1.0) > unrelated
//! docs (score 0.5) — not on the absolute score magnitudes, so the
//! 0.5 score floor for unrelated concepts is by-design and does not
//! perturb the recall measurement.  A real model producing, say,
//! `cos = 0.6` for same-concept pairs would yield `vector_score =
//! 0.8`, still well above the 0.5 unrelated-concept floor —
//! ranking-correct, just with a narrower score gap than the mock.
//!
//! ## Reading the assertions
//!
//! With the mock model:
//! * `recall@12` (k equal to the size of the relevant set) is
//!   **exactly `1.0`** for every query — all 12 same-concept
//!   docs are in the top-12.
//! * `hit-rate@1` and `hit-rate@3` are **exactly `1.0`** for every
//!   query — the top-1 / top-3 results are always same-concept
//!   docs (any of the 12 tied paraphrases at cos sim = 1.0).
//! * Mean of both metrics aggregated across all 120 queries is
//!   **exactly `1.0`** under the ideal mock.
//!
//! The floors below are conservatively pinned at `0.95` (per query
//! `recall@12`) and `0.99` (aggregate mean `recall@12`) so future
//! contributors who tweak the mock concept inventory or add new
//! concepts have headroom before tripping the gates; the real-model
//! thresholds will be tuned when the benchmark is run against
//! XLM-R or a successor.

use evidence_store::embeddings::{EmbeddingError, EmbeddingModel, EmbeddingProbe};
use evidence_store::{
    EvidenceId, EvidenceStore, EvidenceStoreConfig, HybridRetriever, HybridWeights,
    ImportanceClass, ScopeId,
};
use std::collections::{BTreeMap, BTreeSet};
use tempfile::tempdir;

// ---------------------------------------------------------------------
// Fixture: concept inventory.
// ---------------------------------------------------------------------

/// Named concept axes for the benchmark.  Each axis maps every
/// language's paraphrase of the same concept to one orthogonal unit
/// vector in the mock embedding space — so the mock's cosine
/// similarity between same-concept docs is `1.0` and between
/// different-concept docs is `0.0`.
///
/// Concepts are chosen from common knowledge domains where
/// language-agnostic clustering is the expected behaviour of any
/// multilingual embedding model trained on a broad CommonCrawl-style
/// corpus.  The inventory deliberately spans abstract / concrete /
/// activity / state categories so any model bias toward one
/// category (e.g. concrete-noun clustering only) would show up as
/// an asymmetric regression on the concepts it doesn't cluster
/// well.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Concept {
    Weather = 0,
    Finance = 1,
    Cooking = 2,
    Sports = 3,
    Technology = 4,
    Travel = 5,
    Music = 6,
    Health = 7,
    Education = 8,
    Family = 9,
}

const NUM_CONCEPTS: usize = 10;

/// Number of orthogonal axes the mock embedding model supports.
/// One axis per concept, plus a catch-all "off-vocabulary" axis at
/// the tail so any unmatched input lands far from every named
/// concept (avoids a tied score with the catch-all triggering
/// false top-k matches on off-vocabulary corpora — defensive even
/// though the benchmark corpus is closed-vocabulary).
const MOCK_EMBEDDING_DIM: usize = NUM_CONCEPTS + 1;

const CATCH_ALL_AXIS: usize = NUM_CONCEPTS;

// ---------------------------------------------------------------------
// Fixture: 10 concepts × 12 languages = 120-entry corpus.
// ---------------------------------------------------------------------
//
// Each row pins (concept, BCP-47 lang tag, paraphrase text).  The
// language inventory covers the four script families the
// multilingual stack supports — Latin (en/es/fr/de/vi), CJK
// (ja/zh/ko), RTL (ar/he), Indic / SEA (hi/th) — so per-language-pair
// recall measurements walk every cross-script direction the
// production retriever sees.
//
// Every concept has exactly NUM_LANGUAGES entries — the matrix is
// dense by construction so the recall@k denominator is uniform
// across all queries (each query's relevant set has exactly
// NUM_LANGUAGES entries).

const NUM_LANGUAGES: usize = 12;

/// `(concept, lang_tag, paraphrase_text)`.
#[derive(Debug, Clone, Copy)]
struct CorpusEntry {
    concept: Concept,
    lang: &'static str,
    text: &'static str,
}

/// All 120 paraphrases — dense (concept × lang) matrix.
///
/// The paraphrase choices favour high-frequency, idiomatic
/// phrasings that the real XLM-R was demonstrably trained on
/// (CommonCrawl text snippets).  Single-token concepts are avoided
/// where a multi-word phrasing is more natural (e.g. Spanish
/// "pronóstico del tiempo" rather than just "tiempo" — the latter
/// is ambiguous with "time").
#[rustfmt::skip]
const CORPUS: &[CorpusEntry] = &[
    // Concept 0 — Weather forecast.
    CorpusEntry { concept: Concept::Weather, lang: "en", text: "weather forecast" },
    CorpusEntry { concept: Concept::Weather, lang: "ja", text: "天気予報" },
    CorpusEntry { concept: Concept::Weather, lang: "zh", text: "天气预报" },
    CorpusEntry { concept: Concept::Weather, lang: "ko", text: "날씨 예보" },
    CorpusEntry { concept: Concept::Weather, lang: "es", text: "pronóstico del tiempo" },
    CorpusEntry { concept: Concept::Weather, lang: "fr", text: "prévisions météo" },
    CorpusEntry { concept: Concept::Weather, lang: "de", text: "Wettervorhersage" },
    CorpusEntry { concept: Concept::Weather, lang: "ar", text: "توقعات الطقس" },
    CorpusEntry { concept: Concept::Weather, lang: "he", text: "תחזית מזג האוויר" },
    CorpusEntry { concept: Concept::Weather, lang: "hi", text: "मौसम पूर्वानुमान" },
    CorpusEntry { concept: Concept::Weather, lang: "vi", text: "dự báo thời tiết" },
    CorpusEntry { concept: Concept::Weather, lang: "th", text: "พยากรณ์อากาศ" },

    // Concept 1 — Stock market / finance.
    CorpusEntry { concept: Concept::Finance, lang: "en", text: "stock market" },
    CorpusEntry { concept: Concept::Finance, lang: "ja", text: "株式市場" },
    CorpusEntry { concept: Concept::Finance, lang: "zh", text: "股票市场" },
    CorpusEntry { concept: Concept::Finance, lang: "ko", text: "주식 시장" },
    CorpusEntry { concept: Concept::Finance, lang: "es", text: "mercado de valores" },
    CorpusEntry { concept: Concept::Finance, lang: "fr", text: "marché boursier" },
    CorpusEntry { concept: Concept::Finance, lang: "de", text: "Aktienmarkt" },
    CorpusEntry { concept: Concept::Finance, lang: "ar", text: "سوق الأسهم" },
    CorpusEntry { concept: Concept::Finance, lang: "he", text: "שוק המניות" },
    CorpusEntry { concept: Concept::Finance, lang: "hi", text: "शेयर बाज़ार" },
    CorpusEntry { concept: Concept::Finance, lang: "vi", text: "thị trường chứng khoán" },
    CorpusEntry { concept: Concept::Finance, lang: "th", text: "ตลาดหุ้น" },

    // Concept 2 — Recipe / cooking.
    CorpusEntry { concept: Concept::Cooking, lang: "en", text: "recipe ingredients" },
    CorpusEntry { concept: Concept::Cooking, lang: "ja", text: "レシピの材料" },
    CorpusEntry { concept: Concept::Cooking, lang: "zh", text: "食谱配料" },
    CorpusEntry { concept: Concept::Cooking, lang: "ko", text: "요리 재료" },
    CorpusEntry { concept: Concept::Cooking, lang: "es", text: "ingredientes de receta" },
    CorpusEntry { concept: Concept::Cooking, lang: "fr", text: "ingrédients de recette" },
    CorpusEntry { concept: Concept::Cooking, lang: "de", text: "Rezeptzutaten" },
    CorpusEntry { concept: Concept::Cooking, lang: "ar", text: "مكونات الوصفة" },
    CorpusEntry { concept: Concept::Cooking, lang: "he", text: "מצרכי מתכון" },
    CorpusEntry { concept: Concept::Cooking, lang: "hi", text: "रेसिपी सामग्री" },
    CorpusEntry { concept: Concept::Cooking, lang: "vi", text: "nguyên liệu công thức" },
    CorpusEntry { concept: Concept::Cooking, lang: "th", text: "ส่วนผสมสูตรอาหาร" },

    // Concept 3 — Football / sports.
    CorpusEntry { concept: Concept::Sports, lang: "en", text: "football match" },
    CorpusEntry { concept: Concept::Sports, lang: "ja", text: "サッカーの試合" },
    CorpusEntry { concept: Concept::Sports, lang: "zh", text: "足球比赛" },
    CorpusEntry { concept: Concept::Sports, lang: "ko", text: "축구 경기" },
    CorpusEntry { concept: Concept::Sports, lang: "es", text: "partido de fútbol" },
    CorpusEntry { concept: Concept::Sports, lang: "fr", text: "match de football" },
    CorpusEntry { concept: Concept::Sports, lang: "de", text: "Fußballspiel" },
    CorpusEntry { concept: Concept::Sports, lang: "ar", text: "مباراة كرة القدم" },
    CorpusEntry { concept: Concept::Sports, lang: "he", text: "משחק כדורגל" },
    CorpusEntry { concept: Concept::Sports, lang: "hi", text: "फ़ुटबॉल मैच" },
    CorpusEntry { concept: Concept::Sports, lang: "vi", text: "trận đấu bóng đá" },
    CorpusEntry { concept: Concept::Sports, lang: "th", text: "การแข่งขันฟุตบอล" },

    // Concept 4 — Artificial intelligence / technology.
    CorpusEntry { concept: Concept::Technology, lang: "en", text: "artificial intelligence" },
    CorpusEntry { concept: Concept::Technology, lang: "ja", text: "人工知能" },
    CorpusEntry { concept: Concept::Technology, lang: "zh", text: "人工智能" },
    CorpusEntry { concept: Concept::Technology, lang: "ko", text: "인공 지능" },
    CorpusEntry { concept: Concept::Technology, lang: "es", text: "inteligencia artificial" },
    CorpusEntry { concept: Concept::Technology, lang: "fr", text: "intelligence artificielle" },
    CorpusEntry { concept: Concept::Technology, lang: "de", text: "künstliche Intelligenz" },
    CorpusEntry { concept: Concept::Technology, lang: "ar", text: "ذكاء اصطناعي" },
    CorpusEntry { concept: Concept::Technology, lang: "he", text: "בינה מלאכותית" },
    CorpusEntry { concept: Concept::Technology, lang: "hi", text: "कृत्रिम बुद्धिमत्ता" },
    CorpusEntry { concept: Concept::Technology, lang: "vi", text: "trí tuệ nhân tạo" },
    CorpusEntry { concept: Concept::Technology, lang: "th", text: "ปัญญาประดิษฐ์" },

    // Concept 5 — Air travel / airport.
    CorpusEntry { concept: Concept::Travel, lang: "en", text: "international airport" },
    CorpusEntry { concept: Concept::Travel, lang: "ja", text: "国際空港" },
    CorpusEntry { concept: Concept::Travel, lang: "zh", text: "国际机场" },
    CorpusEntry { concept: Concept::Travel, lang: "ko", text: "국제 공항" },
    CorpusEntry { concept: Concept::Travel, lang: "es", text: "aeropuerto internacional" },
    CorpusEntry { concept: Concept::Travel, lang: "fr", text: "aéroport international" },
    CorpusEntry { concept: Concept::Travel, lang: "de", text: "internationaler Flughafen" },
    CorpusEntry { concept: Concept::Travel, lang: "ar", text: "مطار دولي" },
    CorpusEntry { concept: Concept::Travel, lang: "he", text: "נמל תעופה בינלאומי" },
    CorpusEntry { concept: Concept::Travel, lang: "hi", text: "अंतर्राष्ट्रीय हवाई अड्डा" },
    CorpusEntry { concept: Concept::Travel, lang: "vi", text: "sân bay quốc tế" },
    CorpusEntry { concept: Concept::Travel, lang: "th", text: "สนามบินนานาชาติ" },

    // Concept 6 — Classical music / music.
    CorpusEntry { concept: Concept::Music, lang: "en", text: "classical music" },
    CorpusEntry { concept: Concept::Music, lang: "ja", text: "クラシック音楽" },
    CorpusEntry { concept: Concept::Music, lang: "zh", text: "古典音乐" },
    CorpusEntry { concept: Concept::Music, lang: "ko", text: "클래식 음악" },
    CorpusEntry { concept: Concept::Music, lang: "es", text: "música clásica" },
    CorpusEntry { concept: Concept::Music, lang: "fr", text: "musique classique" },
    CorpusEntry { concept: Concept::Music, lang: "de", text: "klassische Musik" },
    CorpusEntry { concept: Concept::Music, lang: "ar", text: "موسيقى كلاسيكية" },
    CorpusEntry { concept: Concept::Music, lang: "he", text: "מוזיקה קלאסית" },
    CorpusEntry { concept: Concept::Music, lang: "hi", text: "शास्त्रीय संगीत" },
    CorpusEntry { concept: Concept::Music, lang: "vi", text: "nhạc cổ điển" },
    CorpusEntry { concept: Concept::Music, lang: "th", text: "ดนตรีคลาสสิก" },

    // Concept 7 — Hospital / health.
    CorpusEntry { concept: Concept::Health, lang: "en", text: "general hospital" },
    CorpusEntry { concept: Concept::Health, lang: "ja", text: "総合病院" },
    CorpusEntry { concept: Concept::Health, lang: "zh", text: "综合医院" },
    CorpusEntry { concept: Concept::Health, lang: "ko", text: "종합 병원" },
    CorpusEntry { concept: Concept::Health, lang: "es", text: "hospital general" },
    CorpusEntry { concept: Concept::Health, lang: "fr", text: "hôpital général" },
    CorpusEntry { concept: Concept::Health, lang: "de", text: "allgemeines Krankenhaus" },
    CorpusEntry { concept: Concept::Health, lang: "ar", text: "مستشفى عام" },
    CorpusEntry { concept: Concept::Health, lang: "he", text: "בית חולים כללי" },
    CorpusEntry { concept: Concept::Health, lang: "hi", text: "सामान्य अस्पताल" },
    CorpusEntry { concept: Concept::Health, lang: "vi", text: "bệnh viện đa khoa" },
    CorpusEntry { concept: Concept::Health, lang: "th", text: "โรงพยาบาลทั่วไป" },

    // Concept 8 — University / education.
    CorpusEntry { concept: Concept::Education, lang: "en", text: "public university" },
    CorpusEntry { concept: Concept::Education, lang: "ja", text: "公立大学" },
    // Note: Mainland Chinese paraphrases use `公办大学` ("publicly-run
    // university") rather than the Han-character-identical `公立大学`
    // form used in Japanese — otherwise the two cells would collide
    // on the same `&'static str` and the corpus dedup invariant
    // in `corpus_shape_invariants` would fire.  Both
    // forms map to the same Education concept axis, so this does
    // not perturb the recall measurement.
    CorpusEntry { concept: Concept::Education, lang: "zh", text: "公办大学" },
    CorpusEntry { concept: Concept::Education, lang: "ko", text: "공립 대학교" },
    CorpusEntry { concept: Concept::Education, lang: "es", text: "universidad pública" },
    CorpusEntry { concept: Concept::Education, lang: "fr", text: "université publique" },
    CorpusEntry { concept: Concept::Education, lang: "de", text: "öffentliche Universität" },
    CorpusEntry { concept: Concept::Education, lang: "ar", text: "جامعة حكومية" },
    CorpusEntry { concept: Concept::Education, lang: "he", text: "אוניברסיטה ציבורית" },
    CorpusEntry { concept: Concept::Education, lang: "hi", text: "सार्वजनिक विश्वविद्यालय" },
    CorpusEntry { concept: Concept::Education, lang: "vi", text: "đại học công lập" },
    CorpusEntry { concept: Concept::Education, lang: "th", text: "มหาวิทยาลัยรัฐ" },

    // Concept 9 — Family reunion / family.
    CorpusEntry { concept: Concept::Family, lang: "en", text: "family reunion" },
    CorpusEntry { concept: Concept::Family, lang: "ja", text: "家族の再会" },
    CorpusEntry { concept: Concept::Family, lang: "zh", text: "家庭团聚" },
    CorpusEntry { concept: Concept::Family, lang: "ko", text: "가족 모임" },
    CorpusEntry { concept: Concept::Family, lang: "es", text: "reunión familiar" },
    CorpusEntry { concept: Concept::Family, lang: "fr", text: "réunion de famille" },
    CorpusEntry { concept: Concept::Family, lang: "de", text: "Familientreffen" },
    CorpusEntry { concept: Concept::Family, lang: "ar", text: "لم شمل العائلة" },
    CorpusEntry { concept: Concept::Family, lang: "he", text: "איחוד משפחתי" },
    CorpusEntry { concept: Concept::Family, lang: "hi", text: "पारिवारिक पुनर्मिलन" },
    CorpusEntry { concept: Concept::Family, lang: "vi", text: "đoàn tụ gia đình" },
    CorpusEntry { concept: Concept::Family, lang: "th", text: "การรวมตัวของครอบครัว" },
];

// ---------------------------------------------------------------------
// Mock embedding model — orthogonal-axis concept clustering.
// ---------------------------------------------------------------------

/// Concept-axis lookup for the mock embedding model.  Identical
/// concepts produce identical unit vectors (cos sim = 1.0); different
/// concepts produce orthogonal unit vectors (cos sim = 0.0); unknown
/// inputs land on the catch-all axis (also orthogonal to every
/// named concept).
///
/// The lookup is a linear scan over [`CORPUS`] — fine at 120 entries
/// and avoids the build-up overhead of a `HashMap` static.  Called
/// once per `embed` call (one per `search_hybrid` query + one per
/// candidate body), so even with full corpus traversal the
/// per-benchmark scan cost is `O(CORPUS.len()^2) ≈ 14_400 entries`,
/// which is well under a millisecond.
fn concept_axis_for(text: &str) -> usize {
    for entry in CORPUS {
        if entry.text == text {
            return entry.concept as usize;
        }
    }
    CATCH_ALL_AXIS
}

/// Deterministic mock that simulates a real multilingual embedding
/// model's signature property — same-concept paraphrases cluster
/// onto the same vector-space axis, unrelated concepts land on
/// orthogonal axes.  Used in place of the real XLM-R ONNX adapter
/// so the benchmark runs without the `models/xlm-r-base.onnx`
/// artifact and without the ONNX runtime in CI.
///
/// The mock is pure — no randomness, no model artifact, no
/// allocations beyond the returned vector — so the benchmark is
/// bit-reproducible across runs and platforms.
struct BenchmarkMockModel;

impl EmbeddingModel for BenchmarkMockModel {
    fn embed(&self, text: &str) -> evidence_store::embeddings::Result<Vec<f32>> {
        if text.is_empty() {
            return Err(EmbeddingError::EmptyInput);
        }
        let axis = concept_axis_for(text);
        let mut v = vec![0.0_f32; MOCK_EMBEDDING_DIM];
        v[axis] = 1.0;
        Ok(v)
    }
    fn dimension(&self) -> usize {
        MOCK_EMBEDDING_DIM
    }
    fn probe(&self) -> EmbeddingProbe {
        EmbeddingProbe::Available
    }
}

// ---------------------------------------------------------------------
// Benchmark driver.
// ---------------------------------------------------------------------

const MASTER_KEY: [u8; 32] = [0xC7; 32];

/// Open a fresh in-memory-equivalent (tempdir-backed) `EvidenceStore`
/// with the [`BenchmarkMockModel`] wired in for both ingest-side
/// and query-side embeddings.  Returned tempdir keeps the SQLCipher
/// file alive for the lifetime of the test.
fn open_benchmark_store() -> (tempfile::TempDir, EvidenceStore) {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("open store")
        .with_embedding_model(BenchmarkMockModel, "benchmark-mock-v1");
    (dir, store)
}

/// Ingest every [`CORPUS`] entry into `store` under `scope`, return
/// the (text → evidence_id, evidence_id → concept) maps for use
/// when scoring recall.
fn ingest_corpus(
    store: &mut EvidenceStore,
    scope: ScopeId,
) -> (
    BTreeMap<&'static str, EvidenceId>,
    BTreeMap<EvidenceId, Concept>,
) {
    let mut by_text = BTreeMap::new();
    let mut concept_by_id = BTreeMap::new();
    for entry in CORPUS {
        let record = store
            .ingest(scope, entry.text.as_bytes(), None, ImportanceClass::Useful)
            .expect("ingest corpus row");
        by_text.insert(entry.text, record.evidence_id);
        concept_by_id.insert(record.evidence_id, entry.concept);
    }
    assert_eq!(by_text.len(), CORPUS.len(), "corpus dedup invariant");
    (by_text, concept_by_id)
}

/// Recall@k for a single query — the standard IR definition.
///
/// `|relevant ∩ top_k| / |relevant|`.  The relevant set is every
/// corpus document that shares the query's concept (12 documents,
/// since the corpus is dense at `NUM_LANGUAGES` paraphrases per
/// concept).  With a perfect embedding model `recall@12` should be
/// `1.0`; smaller `k` is bounded by `k / |relevant| = k / 12`
/// even for a perfect ranker (you can only put `k` docs in the
/// top-k slot), so the assertions below pin `recall@12` for the
/// strict floor and use `hit_rate_at_k` for the top-result quality
/// invariant.
fn recall_at_k(
    top_k: &[EvidenceId],
    expected_concept: Concept,
    concept_by_id: &BTreeMap<EvidenceId, Concept>,
) -> f64 {
    let relevant_total: usize = concept_by_id
        .values()
        .filter(|c| **c == expected_concept)
        .count();
    assert_eq!(
        relevant_total, NUM_LANGUAGES,
        "corpus must be dense at NUM_LANGUAGES paraphrases per concept"
    );

    let found: usize = top_k
        .iter()
        .filter(|id| concept_by_id.get(id) == Some(&expected_concept))
        .count();
    found as f64 / relevant_total as f64
}

/// Hit-rate@k for a single query — the "is the top-k slot
/// occupied by any same-concept doc" invariant.
///
/// Returns `1.0` if `top_k` contains at least one document of
/// `expected_concept`, else `0.0`.  This is the right metric for
/// `k = 1, 3` because `recall@k` is structurally capped at
/// `k / |relevant|` for these small `k` values (e.g.
/// `recall@1 ≤ 1/12 ≈ 0.0833` for ANY ranker on this corpus),
/// which makes `recall@1` a useless gate — hit-rate@1 is the
/// gate that actually catches a regression ("top-1 is NOT a
/// same-concept doc anymore").
fn hit_rate_at_k(
    top_k: &[EvidenceId],
    expected_concept: Concept,
    concept_by_id: &BTreeMap<EvidenceId, Concept>,
) -> f64 {
    if top_k
        .iter()
        .any(|id| concept_by_id.get(id) == Some(&expected_concept))
    {
        1.0
    } else {
        0.0
    }
}

/// Per-query measurement row.  Captured for diagnostic printing
/// when an assertion fails — at debug time the operator sees
/// exactly which (query_lang, concept) cell missed its recall@k
/// or hit-rate@k floor, not just an aggregate number.
///
/// `#[allow(dead_code)]` on the diagnostic-only fields
/// (`query_lang`, `query_text`) is required because they are read
/// only through the [`Debug`] derive (`{:#?}` in the
/// `per_query_failures` panic message).  Rust's dead-code analysis
/// explicitly ignores derived `Debug` impls when deciding which
/// fields count as "used" — the compiler warning text says so
/// directly — so the allow is the documented escape hatch for
/// diagnostic-only struct fields.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct QueryMeasurement {
    concept: Concept,
    query_lang: &'static str,
    query_text: &'static str,
    hit_rate_at_1: f64,
    hit_rate_at_3: f64,
    recall_at_12: f64,
}

// ---------------------------------------------------------------------
// The benchmark suite.
// ---------------------------------------------------------------------

/// Floor for `recall@12` aggregated across all 120 queries.  The
/// mock model produces exactly `1.0` for every (concept, lang) cell,
/// so `0.99` leaves headroom for future concept additions that may
/// have one or two edge cases without tripping the gate.  When the
/// benchmark is run against a real multilingual model (XLM-R or a
/// successor), this floor should be re-tuned to reflect the real
/// model's measured recall (likely `0.85`–`0.95` depending on the
/// model and the language pair distribution).
const MEAN_RECALL_AT_12_FLOOR: f64 = 0.99;

/// Per-query floor for `recall@12` — the strictest invariant.  With
/// the mock model this is exactly `1.0`; the `0.95` floor catches
/// any future code change that degrades recall by more than one
/// document for any (concept, lang) cell.
const PER_QUERY_RECALL_AT_12_FLOOR: f64 = 0.95;

/// Per-query floor for `hit-rate@1` — "the top result is a
/// same-concept doc" invariant.  With the mock model this is
/// exactly `1.0` for every query (any of the 12 tied paraphrases
/// satisfies it).  A real model might occasionally promote a
/// closely-related concept to the top-1 spot, but the floor is
/// pinned tight at `0.95` here because under the mock there is
/// no excuse for any miss — if the benchmark is later run with
/// a real model, this floor should be re-tuned to match the
/// real model's measured hit-rate.
const PER_QUERY_HIT_RATE_AT_1_FLOOR: f64 = 0.95;

/// Per-query floor for `hit-rate@3` — "at least one of the top-3
/// results is a same-concept doc" invariant.  Strictly looser
/// than hit-rate@1 (any miss on hit-rate@3 is also a miss on
/// hit-rate@1), but the floor is pinned at the same `0.95` value
/// because the mock makes both exactly `1.0`.
const PER_QUERY_HIT_RATE_AT_3_FLOOR: f64 = 0.95;

/// The  cross-lingual recall benchmark.  Walks every
/// (query_lang × concept) cell of the 120-entry corpus, computes
/// `hit-rate@{1, 3}` (via [`hit_rate_at_k`]) and `recall@12`
/// (via [`recall_at_k`]) per query, asserts both the per-query
/// and aggregate-mean floors, and prints the per-language-pair
/// breakdown for operator inspection.
///
/// The metric split is deliberate: `recall@k` is structurally
/// capped at `k / |relevant|` for any ranker, so `recall@1` and
/// `recall@3` would max out at `1/12` and `3/12` respectively on
/// this corpus — useless as gates.  `hit-rate@k` (any relevant in
/// top-k) is the gate that catches a top-result regression, and
/// `recall@12` (k = `|relevant set|`) is the gate that catches a
/// full-set regression.  See module-level docs for the full
/// rationale.
///
/// With the [`BenchmarkMockModel`] this test runs in well under one
/// second on a stock CI machine (12 × 10 = 120 queries × 120-row
/// `search_hybrid` calls each = ~14_400 retrieval operations, all
/// against an in-memory SQLCipher store and a pure-Rust mock
/// embedder).  Replacing the mock with the real XLM-R adapter
/// (one-line swap in [`open_benchmark_store`]) makes this a
/// human-driven benchmark that an operator runs locally with the
/// model artifact + ONNX runtime present, with the same assertions
/// pinning the same invariants.
#[test]
fn cross_lingual_recall_benchmark() {
    let (_dir, mut store) = open_benchmark_store();
    let scope = ScopeId::new_v4();

    // Ingest the full 120-entry corpus once; reuse across all 120
    // query measurements.
    let (_by_text, concept_by_id) = ingest_corpus(&mut store, scope);

    // The retriever carries its own `EmbeddingModel` handle —
    // wire the same mock in so query-side embeddings land on the
    // same axis as the ingest-side embeddings.  Vector-only
    // weights so the recall measurement is on the embedding
    // pipeline (FTS5 doesn't share script with most queries, so
    // it would contribute mostly noise; the FTS5
    // lane weights are exercised by their own tests).
    let retriever = HybridRetriever::new(&store)
        .with_embedding_model(BenchmarkMockModel, "benchmark-mock-v1")
        .with_weights(HybridWeights {
            fts: 0.0,
            recency: 0.0,
            vector: 1.0,
        });

    // Walk every (concept × lang) cell as a query.  120 queries
    // total — one for each corpus row.
    //
    // `search_hybrid` widens its FTS + recency candidate pool to
    // `limit.saturating_mul(4).max(16)` rows before scoring — to
    // make sure every corpus doc is in the candidate pool (so the
    // recall measurement isolates embedding quality, not the
    // candidate-sizing logic), we pass `limit = CORPUS.len()` and
    // then slice the returned top-N for the recall@k measurements
    // below.  This shape is exactly what an operator would do to
    // benchmark recall on a small fixture: request a result set
    // big enough to cover the relevant universe, then compute
    // recall@k as the fraction of relevant docs in the top-k.
    let mut measurements: Vec<QueryMeasurement> = Vec::with_capacity(CORPUS.len());
    for query in CORPUS {
        let hits = retriever
            .search_hybrid(scope, query.text, CORPUS.len())
            .expect("search_hybrid ok");
        let top_ids: Vec<EvidenceId> = hits.iter().map(|h| h.evidence_id).collect();

        // Compute recall at three k values to surface degradations
        // at different scales.  recall@1 catches top-result
        // regressions; recall@3 catches near-top regressions;
        // recall@12 (k = |relevant set| = NUM_LANGUAGES) catches
        // full-set regressions.
        let hit_at_1 = hit_rate_at_k(
            &top_ids[..1.min(top_ids.len())],
            query.concept,
            &concept_by_id,
        );
        let hit_at_3 = hit_rate_at_k(
            &top_ids[..3.min(top_ids.len())],
            query.concept,
            &concept_by_id,
        );
        let r_at_12 = recall_at_k(
            &top_ids[..NUM_LANGUAGES.min(top_ids.len())],
            query.concept,
            &concept_by_id,
        );

        measurements.push(QueryMeasurement {
            concept: query.concept,
            query_lang: query.lang,
            query_text: query.text,
            hit_rate_at_1: hit_at_1,
            hit_rate_at_3: hit_at_3,
            recall_at_12: r_at_12,
        });
    }

    // ----- Aggregate-mean floor (the headline invariant) -----
    let n = measurements.len() as f64;
    let mean_hit_1: f64 = measurements.iter().map(|m| m.hit_rate_at_1).sum::<f64>() / n;
    let mean_hit_3: f64 = measurements.iter().map(|m| m.hit_rate_at_3).sum::<f64>() / n;
    let mean_r_at_12: f64 = measurements.iter().map(|m| m.recall_at_12).sum::<f64>() / n;

    eprintln!(
        " cross-lingual recall benchmark (n={n} queries across {c} concepts × {l} languages):",
        c = NUM_CONCEPTS,
        l = NUM_LANGUAGES,
    );
    eprintln!("  mean hit-rate@1  = {mean_hit_1:.4}");
    eprintln!("  mean hit-rate@3  = {mean_hit_3:.4}");
    eprintln!("  mean recall@12   = {mean_r_at_12:.4}");

    assert!(
        mean_r_at_12 >= MEAN_RECALL_AT_12_FLOOR,
        "mean recall@12 = {mean_r_at_12:.4} fell below floor {MEAN_RECALL_AT_12_FLOOR:.4}"
    );

    // ----- Per-query floor (per (concept, lang) cell) -----
    let mut per_query_failures: Vec<&QueryMeasurement> = Vec::new();
    for m in &measurements {
        if m.recall_at_12 < PER_QUERY_RECALL_AT_12_FLOOR
            || m.hit_rate_at_1 < PER_QUERY_HIT_RATE_AT_1_FLOOR
            || m.hit_rate_at_3 < PER_QUERY_HIT_RATE_AT_3_FLOOR
        {
            per_query_failures.push(m);
        }
    }
    assert!(
        per_query_failures.is_empty(),
        "per-query recall floors tripped on {} of {} queries; failures:\n{:#?}",
        per_query_failures.len(),
        measurements.len(),
        per_query_failures,
    );

    // ----- Per-concept floor (every concept must clear its own row) -----
    // Catches a regression that hits one concept across all
    // languages (e.g. "Family" cluster broke") without dragging
    // the global mean below the aggregate floor.
    let mut per_concept_mean: BTreeMap<Concept, (f64, usize)> = BTreeMap::new();
    for m in &measurements {
        let entry = per_concept_mean.entry(m.concept).or_insert((0.0, 0));
        entry.0 += m.recall_at_12;
        entry.1 += 1;
    }
    for (concept, (sum, count)) in &per_concept_mean {
        let mean = sum / *count as f64;
        assert!(mean >= MEAN_RECALL_AT_12_FLOOR,
            "concept {concept:?} mean recall@12 = {mean:.4} fell below floor {MEAN_RECALL_AT_12_FLOOR:.4} \
             (over {count} queries)"
        );
    }
}

/// Sanity check on the corpus shape itself — guards against a
/// future contributor accidentally removing a row, duplicating a
/// row, or breaking the (concept × language) density.  Run as a
/// separate test so a fixture-level regression surfaces as its
/// own failure rather than as a confusing recall-floor breach.
#[test]
fn corpus_shape_invariants() {
    assert_eq!(
        CORPUS.len(),
        NUM_CONCEPTS * NUM_LANGUAGES,
        "CORPUS must be a dense NUM_CONCEPTS × NUM_LANGUAGES matrix"
    );

    let mut by_concept: BTreeMap<Concept, BTreeSet<&'static str>> = BTreeMap::new();
    for entry in CORPUS {
        let langs = by_concept.entry(entry.concept).or_default();
        assert!(
            langs.insert(entry.lang),
            "duplicate (concept={:?}, lang={}) in CORPUS",
            entry.concept,
            entry.lang
        );
    }
    assert_eq!(
        by_concept.len(),
        NUM_CONCEPTS,
        "every concept must appear at least once"
    );
    for (concept, langs) in &by_concept {
        assert_eq!(
            langs.len(),
            NUM_LANGUAGES,
            "concept {concept:?} has {} languages, expected {NUM_LANGUAGES}",
            langs.len()
        );
    }

    // Every corpus text must be unique (no two cells map to the
    // same paraphrase) — a duplicate would alias two evidence_ids
    // to the same text and confuse the recall@k counting.
    let mut all_texts: BTreeSet<&'static str> = BTreeSet::new();
    for entry in CORPUS {
        assert!(
            all_texts.insert(entry.text),
            "duplicate corpus text {:?} (concept={:?}, lang={})",
            entry.text,
            entry.concept,
            entry.lang
        );
    }
}

/// Sanity check on the mock embedding model — guards against the
/// concept-axis lookup silently regressing (e.g. a future change
/// to [`CORPUS`] that introduces a paraphrase that aliases to a
/// different concept's axis would break the recall benchmark in
/// non-obvious ways).
#[test]
fn mock_model_concept_axis_invariants() {
    let model = BenchmarkMockModel;

    // Every corpus entry's text must embed onto its concept's axis,
    // not the catch-all.
    for entry in CORPUS {
        let v = model.embed(entry.text).expect("embed corpus text");
        let axis = entry.concept as usize;
        assert_eq!(
            v.len(),
            MOCK_EMBEDDING_DIM,
            "embed dimension mismatch for {:?}",
            entry.text
        );
        assert!(
            (v[axis] - 1.0).abs() < f32::EPSILON,
            "embedding for {:?} did not land on axis {} (concept {:?})",
            entry.text,
            axis,
            entry.concept
        );
        // Every other axis must be zero — orthogonality is the
        // architectural invariant the mock encodes.
        for (i, &component) in v.iter().enumerate() {
            if i == axis {
                continue;
            }
            assert!(
                component.abs() < f32::EPSILON,
                "embedding for {:?} bled non-zero onto axis {} (expected only axis {})",
                entry.text,
                i,
                axis
            );
        }
    }

    // An off-vocabulary string must land on the catch-all axis,
    // not on a concept axis (would corrupt the recall counting
    // if e.g. a future contributor stored a non-corpus body for
    // a control-group docID and it accidentally clustered with a
    // real concept).
    let off_vocab = model
        .embed("totally unrelated control-group document body")
        .expect("embed off-vocab");
    assert!(
        (off_vocab[CATCH_ALL_AXIS] - 1.0).abs() < f32::EPSILON,
        "off-vocab text did not land on catch-all axis"
    );
    for (i, &component) in off_vocab.iter().enumerate() {
        if i == CATCH_ALL_AXIS {
            continue;
        }
        assert!(
            component.abs() < f32::EPSILON,
            "off-vocab embedding bled onto axis {i}"
        );
    }

    // The empty string must propagate the EmptyInput error rather
    // than panicking — pins the EmbeddingModel contract for the
    // mock so a future change to the trait's error policy
    // surfaces here.
    let empty_result = model.embed("");
    assert!(
        matches!(empty_result, Err(EmbeddingError::EmptyInput)),
        "empty string must produce EmbeddingError::EmptyInput, got {empty_result:?}"
    );
}
