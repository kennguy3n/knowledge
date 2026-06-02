//! Process-singleton observability counters for the multilingual
//! lexicon path.
//!
//! The multilingual rollout introduced per-script
//! [`crate::lexicon::LanguageLexicon`]s,
//! [`crate::lexicon::MatchStrategy`] variants (including the
//! Arabic / Hebrew clitic-aware peelers), and per-sentence
//! language detection — but originally the substrate ran them
//! all "blind", with no way to ask *which* lexicons were hitting
//! on real corpora, *which* match strategies were firing, and how
//! deep the proclitic peelers were going. This module closes that
//! observability gap by adding lock-free [`AtomicU64`] counters at
//! every interesting decision point on the lexicon hot path.
//!
//! # Counter taxonomy
//!
//! * **Lexicon hits per BCP-47 primary subtag** — every call to
//!   [`crate::lexicon::LexiconRegistry::lexicon_for_or_english`]
//!   increments exactly one of [`Counters::hits_en`] /
//!   [`Counters::hits_ja`] / … / [`Counters::hits_zh`], keyed on
//!   the **resolved** lexicon's `primary_tag` (i.e. after the
//!   unknown-tag → English fallback). Separately,
//!   [`Counters::unknown_tag_fallbacks_total`] tracks the cases
//!   where the input tag was `Some(t)` but no lexicon was
//!   configured for `t`, so the host can see how often whatlang's
//!   detection lands on an unsupported tag. The `hits_*` counter
//!   for the English lexicon therefore includes both genuinely
//!   English bodies AND the fallback cases.
//!
//!   **Unit-of-measurement**: each `hits_<tag>` increment counts
//!   one *resolution call* — i.e. one invocation of
//!   `lexicon_for_or_english` — NOT one unique sentence or
//!   document. A single sentence typically triggers several
//!   resolution calls because the classifier loop in
//!   [`crate::extractor::LexiconExtractor::sentence_matches_class`]
//!   re-resolves for each of the up-to-three keyword classes it
//!   tests (Decision, Task, TaskImperative), and the capitalised-
//!   word stop-word filter at
//!   [`crate::lexicon::is_stop_word`] re-resolves once per
//!   capitalised word it inspects. For a typical 5-capitalised-
//!   word English sentence with no class match, expect ~8
//!   `hits_en` increments (3 class checks + 5 stop-word checks).
//!   Operators inferring "documents classified" from `hits_*`
//!   should divide by their measured calls-per-document ratio
//!   rather than reading the counter directly.
//!   this clarification was added — the
//!   counter semantics itself are by design (counting calls is
//!   what makes the ratio `strategy_fires / hits_*` a useful
//!   "how-often-does-each-resolved-lexicon-actually-classify"
//!   signal).
//!
//! * **`MatchStrategy` fires** — every call to
//!   [`crate::lexicon::table_matches`] increments exactly one of
//!   the five [`Counters::strategy_first_token`] /
//!   `_first_bigram` / `_substring` / `_first_token_with_arabic_clitics`
//!   / `_first_token_with_hebrew_clitics` counters, regardless of
//!   whether the call returned `true` or `false`. The counter is
//!   bumped on every *invocation* — combine with the per-lexicon
//!   `hits_*` counters above to see e.g. "the `vi` lexicon was
//!   resolved 1 200 times, and `FirstBigram` fired 4 800 times
//!   (4 calls per resolved lexicon, matching the 4-class
//!   classifier loop in `LexiconExtractor::sentence_matches_class`)".
//!
//! * **Arabic / Hebrew peel-depth distribution** — every call to
//!   the [`crate::lexicon::MatchStrategy::FirstTokenWithArabicClitics`]
//!   / `FirstTokenWithHebrewClitics` matcher increments exactly
//!   one of the five buckets `arabic_peel_depth_{0,1,2,3,exhausted}`
//!   (and similarly for Hebrew). Bucket `0` is incremented on a
//!   direct table hit (no peels needed), bucket `1` on a one-peel
//!   hit, etc., and `exhausted` is incremented when the peel
//!   budget was consumed without finding a table entry (so the
//!   matcher returned `false`). The distribution reveals whether
//!   the peel budget is tight (most calls hit depth 0–1, occasional
//!   2–3, rare exhausted) or wasteful (most calls hit exhausted —
//!   would indicate the matcher is being fed primarily non-Arabic
//!   / non-Hebrew tokens, a routing bug).
//!
//! # Wire-format stability
//!
//! [`LexiconTelemetrySnapshot`] is the wire-flat read-out
//! structure platform hosts deserialize via the FFI. New counters
//! must be added as additional fields with `#[serde(default)]` on
//! the FFI-mirror struct in `crates/ffi/src/metrics.rs` so older
//! emitters whose JSON lacks the new field still deserialize
//! cleanly. The plain Rust struct in *this* module does not need
//! `#[serde(default)]` because it is only constructed locally
//! (read from atomics inside [`snapshot`]) — every field is
//! always populated by [`snapshot`] before any caller sees it.
//!
//! # Performance
//!
//! Every counter is an [`AtomicU64`] incremented with
//! [`Ordering::Relaxed`]. The per-call cost on the hot path is
//! one atomic add — no allocations, no locks. `Relaxed` is
//! sufficient because the host never makes a correctness
//! decision on a single counter read; a slightly-stale read just
//! means the reported number is a few classifier calls behind
//! reality, which is acceptable for a diagnostic surface.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use crate::lexicon::MatchStrategy;

/// Process-singleton bag of atomic counters. Internal — callers
/// touch the counters via [`record_lexicon_hit`] /
/// [`record_match_strategy_fire`] / [`record_arabic_peel_depth`] /
/// [`record_hebrew_peel_depth`] and read via [`snapshot`].
///
/// Fields are `pub(crate)` rather than `pub` because the only
/// supported access path for the FFI / host surface is through
/// [`snapshot`] (which returns a wire-flat plain `u64`
/// `LexiconTelemetrySnapshot`).
#[derive(Default, Debug)]
pub(crate) struct Counters {
    // ─── Lexicon hits per BCP-47 primary subtag ────────────────
    // One counter per resolved-lexicon `primary_tag` (so a fall-
    // back to English shows up here as `hits_en`, not as the
    // requested tag). The 21 fields below mirror
    // `crate::lexicon::SUPPORTED_LEXICON_TAGS` exactly — every
    // new lexicon added to the registry must extend this struct
    // (and the snapshot, and the FFI mirror) symmetrically.
    pub(crate) hits_ar: AtomicU64,
    pub(crate) hits_bo: AtomicU64,
    pub(crate) hits_de: AtomicU64,
    pub(crate) hits_en: AtomicU64,
    pub(crate) hits_es: AtomicU64,
    pub(crate) hits_fr: AtomicU64,
    pub(crate) hits_he: AtomicU64,
    pub(crate) hits_hi: AtomicU64,
    pub(crate) hits_id: AtomicU64,
    pub(crate) hits_it: AtomicU64,
    pub(crate) hits_ja: AtomicU64,
    pub(crate) hits_km: AtomicU64,
    pub(crate) hits_ko: AtomicU64,
    pub(crate) hits_lo: AtomicU64,
    pub(crate) hits_ms: AtomicU64,
    pub(crate) hits_my: AtomicU64,
    pub(crate) hits_pt: AtomicU64,
    pub(crate) hits_ru: AtomicU64,
    pub(crate) hits_th: AtomicU64,
    pub(crate) hits_tl: AtomicU64,
    pub(crate) hits_vi: AtomicU64,
    pub(crate) hits_zh: AtomicU64,
    /// Times an input primary_tag was `Some(t)` but no lexicon
    /// was configured for `t` — i.e. an unknown-tag → English
    /// fallback fired in
    /// [`crate::lexicon::LexiconRegistry::lexicon_for_or_english`].
    /// The matching `hits_en` increment ALSO fires for these
    /// fallback cases (the resolved lexicon IS English), so
    /// `unknown_tag_fallbacks_total` is the strictly-additive
    /// signal "of those English hits, how many were fallbacks
    /// from an unsupported tag".
    pub(crate) unknown_tag_fallbacks_total: AtomicU64,

    // ─── MatchStrategy fires (5 variants, mirrors enum) ─────────
    pub(crate) strategy_first_token: AtomicU64,
    pub(crate) strategy_first_bigram: AtomicU64,
    pub(crate) strategy_substring: AtomicU64,
    pub(crate) strategy_first_token_with_arabic_clitics: AtomicU64,
    pub(crate) strategy_first_token_with_hebrew_clitics: AtomicU64,

    // ─── Arabic peel-depth distribution (5 buckets) ─────────────
    // Buckets 0..=3 are the depth at which a table entry matched
    // (0 = direct, 1-3 = N peels needed). `exhausted` is incremented
    // when the peel budget was consumed without finding a match.
    pub(crate) arabic_peel_depth_0_matches: AtomicU64,
    pub(crate) arabic_peel_depth_1_matches: AtomicU64,
    pub(crate) arabic_peel_depth_2_matches: AtomicU64,
    pub(crate) arabic_peel_depth_3_matches: AtomicU64,
    pub(crate) arabic_peel_depth_exhausted: AtomicU64,

    // ─── Hebrew peel-depth distribution (5 buckets) ─────────────
    pub(crate) hebrew_peel_depth_0_matches: AtomicU64,
    pub(crate) hebrew_peel_depth_1_matches: AtomicU64,
    pub(crate) hebrew_peel_depth_2_matches: AtomicU64,
    pub(crate) hebrew_peel_depth_3_matches: AtomicU64,
    pub(crate) hebrew_peel_depth_exhausted: AtomicU64,
}

static COUNTERS: OnceLock<Counters> = OnceLock::new();

/// Borrow the process-singleton counter block. Internal — call
/// [`snapshot`] for a read-out or one of the `record_*` helpers
/// for an increment.
#[inline]
fn counters() -> &'static Counters {
    COUNTERS.get_or_init(Counters::default)
}

/// Record a lexicon resolution. Increments the `hits_<tag>`
/// counter matching `resolved_primary_tag`; if `requested` was
/// `Some(t)` but `t != resolved_primary_tag`, also increments
/// [`Counters::unknown_tag_fallbacks_total`].
///
/// `resolved_primary_tag` is the `primary_tag` of the
/// [`crate::lexicon::LanguageLexicon`] that
/// [`crate::lexicon::LexiconRegistry::lexicon_for_or_english`]
/// returned. Unrecognised tags are silently ignored — i.e. a
/// future lexicon added to the registry without a counter field
/// here is a no-op increment, not a panic. The
/// [`tag_counters_cover_all_supported_tags`] test pins this
/// invariant.
#[inline]
pub fn record_lexicon_hit(requested: Option<&str>, resolved_primary_tag: &str) {
    let c = counters();
    let counter = match resolved_primary_tag {
        "ar" => &c.hits_ar,
        "bo" => &c.hits_bo,
        "de" => &c.hits_de,
        "en" => &c.hits_en,
        "es" => &c.hits_es,
        "fr" => &c.hits_fr,
        "he" => &c.hits_he,
        "hi" => &c.hits_hi,
        "id" => &c.hits_id,
        "it" => &c.hits_it,
        "ja" => &c.hits_ja,
        "km" => &c.hits_km,
        "ko" => &c.hits_ko,
        "lo" => &c.hits_lo,
        "ms" => &c.hits_ms,
        "my" => &c.hits_my,
        "pt" => &c.hits_pt,
        "ru" => &c.hits_ru,
        "th" => &c.hits_th,
        "tl" => &c.hits_tl,
        "vi" => &c.hits_vi,
        "zh" => &c.hits_zh,
        // Unknown resolved tag — silently skip. A future lexicon
        // added without extending this match arm is a no-op
        // increment. The companion test
        // `tag_counters_cover_all_supported_tags` flags any
        // SUPPORTED_LEXICON_TAGS entry missing here.
        _ => return,
    };
    counter.fetch_add(1, Ordering::Relaxed);

    // Detect fallback: input was Some(t) but resolved to a
    // different tag. This is the canonical "unknown tag →
    // English fallback" case (e.g. requested "xx" → resolved
    // "en"). We do NOT count `None` input as a fallback — that
    // path is the explicit "no language detected" case which
    // unambiguously routes to English by design, not a fallback
    // from a failed lookup.
    if let Some(req) = requested {
        if req != resolved_primary_tag {
            c.unknown_tag_fallbacks_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Record a [`crate::lexicon::table_matches`] invocation. Bumps
/// exactly one of the five `strategy_*` counters per the
/// `strategy` argument.
#[inline]
pub fn record_match_strategy_fire(strategy: MatchStrategy) {
    let c = counters();
    let counter = match strategy {
        MatchStrategy::FirstToken => &c.strategy_first_token,
        MatchStrategy::FirstBigram => &c.strategy_first_bigram,
        MatchStrategy::Substring => &c.strategy_substring,
        MatchStrategy::FirstTokenWithArabicClitics => &c.strategy_first_token_with_arabic_clitics,
        MatchStrategy::FirstTokenWithHebrewClitics => &c.strategy_first_token_with_hebrew_clitics,
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Outcome of an Arabic / Hebrew clitic-peel attempt — fed into
/// [`record_arabic_peel_depth`] / [`record_hebrew_peel_depth`] to
/// pick the right histogram bucket.
///
/// `MatchedAtDepth(0)` is incremented when the first
/// alphabetic token matched a table entry *without* any peel
/// (i.e. the bare token was already a hit). `MatchedAtDepth(N)`
/// for N in 1..=`PEEL_BUDGET` is incremented when the table
/// entry was found *after* N successive peels of one proclitic
/// each. `BudgetExhausted` is incremented when the peel budget
/// was consumed without finding a match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeelOutcome {
    /// Match found at depth `0..=3`.
    MatchedAtDepth(u8),
    /// Budget consumed without a match.
    BudgetExhausted,
}

/// Bump the Arabic peel-depth histogram bucket matching
/// `outcome`. Bucket selection mirrors the constants on
/// [`crate::lexicon::MatchStrategy::FirstTokenWithArabicClitics`]
/// (`ARABIC_PROCLITIC_PEEL_BUDGET` = 3, so the valid match-depth
/// values are 0..=3). A `MatchedAtDepth(d)` with `d > 3` is
/// clamped to bucket 3 — the matcher never reports a depth
/// outside its own budget, but the clamp keeps the counter
/// defensive against future budget changes.
#[inline]
pub fn record_arabic_peel_depth(outcome: PeelOutcome) {
    let c = counters();
    let counter = match outcome {
        PeelOutcome::MatchedAtDepth(0) => &c.arabic_peel_depth_0_matches,
        PeelOutcome::MatchedAtDepth(1) => &c.arabic_peel_depth_1_matches,
        PeelOutcome::MatchedAtDepth(2) => &c.arabic_peel_depth_2_matches,
        PeelOutcome::MatchedAtDepth(_) => &c.arabic_peel_depth_3_matches,
        PeelOutcome::BudgetExhausted => &c.arabic_peel_depth_exhausted,
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Bump the Hebrew peel-depth histogram bucket matching
/// `outcome`. Mirror of [`record_arabic_peel_depth`] — see that
/// function for the bucketing semantics.
#[inline]
pub fn record_hebrew_peel_depth(outcome: PeelOutcome) {
    let c = counters();
    let counter = match outcome {
        PeelOutcome::MatchedAtDepth(0) => &c.hebrew_peel_depth_0_matches,
        PeelOutcome::MatchedAtDepth(1) => &c.hebrew_peel_depth_1_matches,
        PeelOutcome::MatchedAtDepth(2) => &c.hebrew_peel_depth_2_matches,
        PeelOutcome::MatchedAtDepth(_) => &c.hebrew_peel_depth_3_matches,
        PeelOutcome::BudgetExhausted => &c.hebrew_peel_depth_exhausted,
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Wire-flat read-out of every counter at the moment of the
/// [`snapshot`] call.
///
/// This struct is exposed through the FFI by the mirror struct
/// in `crates/ffi/src/metrics.rs` (with `uniffi::Record` /
/// `napi::ObjectFinalize` derives that this plain Rust struct
/// cannot pick up because `observation_engine` doesn't depend on
/// either FFI runtime). Adding a new counter to [`Counters`]
/// requires (1) extending this struct's field list, (2) extending
/// the [`snapshot`] function's load list, and (3) extending the
/// FFI-mirror struct in `crates/ffi/src/metrics.rs` with the
/// `#[serde(default)]` attribute so older emitters' JSON still
/// deserializes.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct LexiconTelemetrySnapshot {
    /// Resolved-lexicon hits for `ar`.
    pub hits_ar: u64,
    /// Resolved-lexicon hits for `bo`.
    pub hits_bo: u64,
    /// Resolved-lexicon hits for `de`.
    pub hits_de: u64,
    /// Resolved-lexicon hits for `en`. Includes the
    /// unknown-tag → English fallback path (see
    /// [`unknown_tag_fallbacks_total`](Self::unknown_tag_fallbacks_total)).
    pub hits_en: u64,
    /// Resolved-lexicon hits for `es`.
    pub hits_es: u64,
    /// Resolved-lexicon hits for `fr`.
    pub hits_fr: u64,
    /// Resolved-lexicon hits for `he`.
    pub hits_he: u64,
    /// Resolved-lexicon hits for `hi`.
    pub hits_hi: u64,
    /// Resolved-lexicon hits for `id`.
    pub hits_id: u64,
    /// Resolved-lexicon hits for `it`.
    pub hits_it: u64,
    /// Resolved-lexicon hits for `ja`.
    pub hits_ja: u64,
    /// Resolved-lexicon hits for `km`.
    pub hits_km: u64,
    /// Resolved-lexicon hits for `ko`.
    pub hits_ko: u64,
    /// Resolved-lexicon hits for `lo`.
    pub hits_lo: u64,
    /// Resolved-lexicon hits for `ms`.
    pub hits_ms: u64,
    /// Resolved-lexicon hits for `my`.
    pub hits_my: u64,
    /// Resolved-lexicon hits for `pt`.
    pub hits_pt: u64,
    /// Resolved-lexicon hits for `ru`.
    pub hits_ru: u64,
    /// Resolved-lexicon hits for `th`.
    pub hits_th: u64,
    /// Resolved-lexicon hits for `tl`.
    pub hits_tl: u64,
    /// Resolved-lexicon hits for `vi`.
    pub hits_vi: u64,
    /// Resolved-lexicon hits for `zh`.
    pub hits_zh: u64,
    /// Times an input primary_tag was `Some(t)` but no lexicon
    /// was configured for `t`, so
    /// [`crate::lexicon::LexiconRegistry::lexicon_for_or_english`]
    /// fell back to the English lexicon. The matching `hits_en`
    /// counter ALSO fires for these cases, so
    /// `unknown_tag_fallbacks_total <= hits_en` always holds.
    pub unknown_tag_fallbacks_total: u64,
    /// [`MatchStrategy::FirstToken`] fires.
    pub strategy_first_token: u64,
    /// [`MatchStrategy::FirstBigram`] fires.
    pub strategy_first_bigram: u64,
    /// [`MatchStrategy::Substring`] fires.
    pub strategy_substring: u64,
    /// [`MatchStrategy::FirstTokenWithArabicClitics`] fires.
    pub strategy_first_token_with_arabic_clitics: u64,
    /// [`MatchStrategy::FirstTokenWithHebrewClitics`] fires.
    pub strategy_first_token_with_hebrew_clitics: u64,
    /// Arabic clitic-peeler matches at depth 0 (no peel needed).
    pub arabic_peel_depth_0_matches: u64,
    /// Arabic clitic-peeler matches at depth 1 (one peel).
    pub arabic_peel_depth_1_matches: u64,
    /// Arabic clitic-peeler matches at depth 2 (two peels).
    pub arabic_peel_depth_2_matches: u64,
    /// Arabic clitic-peeler matches at depth 3 (three peels).
    pub arabic_peel_depth_3_matches: u64,
    /// Arabic clitic-peeler budget exhausted without a match.
    pub arabic_peel_depth_exhausted: u64,
    /// Hebrew clitic-peeler matches at depth 0 (no peel needed).
    pub hebrew_peel_depth_0_matches: u64,
    /// Hebrew clitic-peeler matches at depth 1 (one peel).
    pub hebrew_peel_depth_1_matches: u64,
    /// Hebrew clitic-peeler matches at depth 2 (two peels).
    pub hebrew_peel_depth_2_matches: u64,
    /// Hebrew clitic-peeler matches at depth 3 (three peels).
    pub hebrew_peel_depth_3_matches: u64,
    /// Hebrew clitic-peeler budget exhausted without a match.
    pub hebrew_peel_depth_exhausted: u64,
}

/// Return a wire-flat snapshot of every lexicon-telemetry
/// counter. Reads each [`AtomicU64`] with [`Ordering::Relaxed`]
/// — see the module docs for why that's sufficient.
#[must_use]
pub fn snapshot() -> LexiconTelemetrySnapshot {
    let c = counters();
    LexiconTelemetrySnapshot {
        hits_ar: c.hits_ar.load(Ordering::Relaxed),
        hits_bo: c.hits_bo.load(Ordering::Relaxed),
        hits_de: c.hits_de.load(Ordering::Relaxed),
        hits_en: c.hits_en.load(Ordering::Relaxed),
        hits_es: c.hits_es.load(Ordering::Relaxed),
        hits_fr: c.hits_fr.load(Ordering::Relaxed),
        hits_he: c.hits_he.load(Ordering::Relaxed),
        hits_hi: c.hits_hi.load(Ordering::Relaxed),
        hits_id: c.hits_id.load(Ordering::Relaxed),
        hits_it: c.hits_it.load(Ordering::Relaxed),
        hits_ja: c.hits_ja.load(Ordering::Relaxed),
        hits_km: c.hits_km.load(Ordering::Relaxed),
        hits_ko: c.hits_ko.load(Ordering::Relaxed),
        hits_lo: c.hits_lo.load(Ordering::Relaxed),
        hits_ms: c.hits_ms.load(Ordering::Relaxed),
        hits_my: c.hits_my.load(Ordering::Relaxed),
        hits_pt: c.hits_pt.load(Ordering::Relaxed),
        hits_ru: c.hits_ru.load(Ordering::Relaxed),
        hits_th: c.hits_th.load(Ordering::Relaxed),
        hits_tl: c.hits_tl.load(Ordering::Relaxed),
        hits_vi: c.hits_vi.load(Ordering::Relaxed),
        hits_zh: c.hits_zh.load(Ordering::Relaxed),
        unknown_tag_fallbacks_total: c.unknown_tag_fallbacks_total.load(Ordering::Relaxed),
        strategy_first_token: c.strategy_first_token.load(Ordering::Relaxed),
        strategy_first_bigram: c.strategy_first_bigram.load(Ordering::Relaxed),
        strategy_substring: c.strategy_substring.load(Ordering::Relaxed),
        strategy_first_token_with_arabic_clitics: c
            .strategy_first_token_with_arabic_clitics
            .load(Ordering::Relaxed),
        strategy_first_token_with_hebrew_clitics: c
            .strategy_first_token_with_hebrew_clitics
            .load(Ordering::Relaxed),
        arabic_peel_depth_0_matches: c.arabic_peel_depth_0_matches.load(Ordering::Relaxed),
        arabic_peel_depth_1_matches: c.arabic_peel_depth_1_matches.load(Ordering::Relaxed),
        arabic_peel_depth_2_matches: c.arabic_peel_depth_2_matches.load(Ordering::Relaxed),
        arabic_peel_depth_3_matches: c.arabic_peel_depth_3_matches.load(Ordering::Relaxed),
        arabic_peel_depth_exhausted: c.arabic_peel_depth_exhausted.load(Ordering::Relaxed),
        hebrew_peel_depth_0_matches: c.hebrew_peel_depth_0_matches.load(Ordering::Relaxed),
        hebrew_peel_depth_1_matches: c.hebrew_peel_depth_1_matches.load(Ordering::Relaxed),
        hebrew_peel_depth_2_matches: c.hebrew_peel_depth_2_matches.load(Ordering::Relaxed),
        hebrew_peel_depth_3_matches: c.hebrew_peel_depth_3_matches.load(Ordering::Relaxed),
        hebrew_peel_depth_exhausted: c.hebrew_peel_depth_exhausted.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexicon::SUPPORTED_LEXICON_TAGS;

    /// Pin the invariant that every BCP-47 tag in
    /// [`SUPPORTED_LEXICON_TAGS`] has a `hits_<tag>` counter in
    /// this module. A future contributor adding a new lexicon
    /// to the registry will fail this test if they forget to
    /// extend [`Counters`] / [`LexiconTelemetrySnapshot`] /
    /// [`record_lexicon_hit`].
    #[test]
    fn tag_counters_cover_all_supported_tags() {
        // Capture per-tag baselines (counters are process-
        // singleton; other tests may have incremented them).
        let baseline = snapshot();
        let baseline_by_tag: Vec<(&str, u64)> = SUPPORTED_LEXICON_TAGS
            .iter()
            .map(|tag| {
                let v = match *tag {
                    "ar" => baseline.hits_ar,
                    "bo" => baseline.hits_bo,
                    "de" => baseline.hits_de,
                    "en" => baseline.hits_en,
                    "es" => baseline.hits_es,
                    "fr" => baseline.hits_fr,
                    "he" => baseline.hits_he,
                    "hi" => baseline.hits_hi,
                    "id" => baseline.hits_id,
                    "it" => baseline.hits_it,
                    "ja" => baseline.hits_ja,
                    "km" => baseline.hits_km,
                    "ko" => baseline.hits_ko,
                    "lo" => baseline.hits_lo,
                    "ms" => baseline.hits_ms,
                    "my" => baseline.hits_my,
                    "pt" => baseline.hits_pt,
                    "ru" => baseline.hits_ru,
                    "th" => baseline.hits_th,
                    "tl" => baseline.hits_tl,
                    "vi" => baseline.hits_vi,
                    "zh" => baseline.hits_zh,
                    other => panic!(
                        "SUPPORTED_LEXICON_TAGS contains {other:?} but the test \
                        match arm doesn't cover it — extend both this test and the \
                        record_lexicon_hit match arm"
                    ),
                };
                (*tag, v)
            })
            .collect();

        // Drive one increment for every supported tag. The
        // resolved_primary_tag and requested args match, so no
        // unknown-tag fallback should fire.
        for (tag, _) in &baseline_by_tag {
            record_lexicon_hit(Some(tag), tag);
        }

        let after = snapshot();
        for (tag, base) in &baseline_by_tag {
            let v_after = match *tag {
                "ar" => after.hits_ar,
                "bo" => after.hits_bo,
                "de" => after.hits_de,
                "en" => after.hits_en,
                "es" => after.hits_es,
                "fr" => after.hits_fr,
                "he" => after.hits_he,
                "hi" => after.hits_hi,
                "id" => after.hits_id,
                "it" => after.hits_it,
                "ja" => after.hits_ja,
                "km" => after.hits_km,
                "ko" => after.hits_ko,
                "lo" => after.hits_lo,
                "ms" => after.hits_ms,
                "my" => after.hits_my,
                "pt" => after.hits_pt,
                "ru" => after.hits_ru,
                "th" => after.hits_th,
                "tl" => after.hits_tl,
                "vi" => after.hits_vi,
                "zh" => after.hits_zh,
                _ => unreachable!("baseline match already exhausted"),
            };
            assert!(
                v_after > *base,
                "hits_{tag} did not increment — record_lexicon_hit \
                 must have a match arm for {tag:?}"
            );
        }
    }

    /// Pin the invariant that an unknown input tag → English
    /// fallback bumps BOTH `hits_en` AND
    /// `unknown_tag_fallbacks_total`.
    #[test]
    fn unknown_tag_fallback_increments_en_and_fallback_counter() {
        let before = snapshot();
        record_lexicon_hit(Some("xx-unsupported-tag"), "en");
        let after = snapshot();
        assert_eq!(
            after.hits_en,
            before.hits_en + 1,
            "fallback must bump hits_en"
        );
        assert_eq!(
            after.unknown_tag_fallbacks_total,
            before.unknown_tag_fallbacks_total + 1,
            "fallback must bump unknown_tag_fallbacks_total"
        );
    }

    /// An input tag of `None` (no language detected) routes to
    /// English by design and is NOT counted as a fallback.
    #[test]
    fn none_input_does_not_increment_fallback_counter() {
        let before = snapshot();
        record_lexicon_hit(None, "en");
        let after = snapshot();
        assert_eq!(
            after.hits_en,
            before.hits_en + 1,
            "None input still bumps hits_en"
        );
        assert_eq!(
            after.unknown_tag_fallbacks_total, before.unknown_tag_fallbacks_total,
            "None input is NOT a fallback — counter must NOT increment"
        );
    }

    /// Direct hit on an English body (requested == resolved == "en")
    /// is NOT a fallback either.
    #[test]
    fn direct_en_hit_does_not_increment_fallback_counter() {
        let before = snapshot();
        record_lexicon_hit(Some("en"), "en");
        let after = snapshot();
        assert_eq!(after.hits_en, before.hits_en + 1);
        assert_eq!(
            after.unknown_tag_fallbacks_total, before.unknown_tag_fallbacks_total,
            "direct en hit is NOT a fallback"
        );
    }

    /// Pin the invariant that every [`MatchStrategy`] variant
    /// bumps a distinct counter — a future contributor adding a
    /// new variant will fail this test if they forget to extend
    /// [`record_match_strategy_fire`].
    #[test]
    fn match_strategy_fires_increment_distinct_counters() {
        let before = snapshot();
        record_match_strategy_fire(MatchStrategy::FirstToken);
        record_match_strategy_fire(MatchStrategy::FirstBigram);
        record_match_strategy_fire(MatchStrategy::Substring);
        record_match_strategy_fire(MatchStrategy::FirstTokenWithArabicClitics);
        record_match_strategy_fire(MatchStrategy::FirstTokenWithHebrewClitics);
        let after = snapshot();
        assert_eq!(after.strategy_first_token, before.strategy_first_token + 1);
        assert_eq!(
            after.strategy_first_bigram,
            before.strategy_first_bigram + 1
        );
        assert_eq!(after.strategy_substring, before.strategy_substring + 1);
        assert_eq!(
            after.strategy_first_token_with_arabic_clitics,
            before.strategy_first_token_with_arabic_clitics + 1
        );
        assert_eq!(
            after.strategy_first_token_with_hebrew_clitics,
            before.strategy_first_token_with_hebrew_clitics + 1
        );
    }

    /// Pin the Arabic peel-depth bucket selection. The 4 match
    /// depths (0..=3) all go to distinct counters; depth values
    /// outside the valid range clamp to the depth-3 bucket.
    #[test]
    fn arabic_peel_depth_bucket_selection() {
        let before = snapshot();
        record_arabic_peel_depth(PeelOutcome::MatchedAtDepth(0));
        record_arabic_peel_depth(PeelOutcome::MatchedAtDepth(1));
        record_arabic_peel_depth(PeelOutcome::MatchedAtDepth(2));
        record_arabic_peel_depth(PeelOutcome::MatchedAtDepth(3));
        record_arabic_peel_depth(PeelOutcome::MatchedAtDepth(4)); // clamps to bucket 3
        record_arabic_peel_depth(PeelOutcome::BudgetExhausted);
        let after = snapshot();
        assert_eq!(
            after.arabic_peel_depth_0_matches,
            before.arabic_peel_depth_0_matches + 1
        );
        assert_eq!(
            after.arabic_peel_depth_1_matches,
            before.arabic_peel_depth_1_matches + 1
        );
        assert_eq!(
            after.arabic_peel_depth_2_matches,
            before.arabic_peel_depth_2_matches + 1
        );
        // depth 3 AND clamped depth 4 → +2 in the depth-3 bucket
        assert_eq!(
            after.arabic_peel_depth_3_matches,
            before.arabic_peel_depth_3_matches + 2
        );
        assert_eq!(
            after.arabic_peel_depth_exhausted,
            before.arabic_peel_depth_exhausted + 1
        );
    }

    /// Pin the Hebrew peel-depth bucket selection. Mirror of
    /// the Arabic test.
    #[test]
    fn hebrew_peel_depth_bucket_selection() {
        let before = snapshot();
        record_hebrew_peel_depth(PeelOutcome::MatchedAtDepth(0));
        record_hebrew_peel_depth(PeelOutcome::MatchedAtDepth(1));
        record_hebrew_peel_depth(PeelOutcome::MatchedAtDepth(2));
        record_hebrew_peel_depth(PeelOutcome::MatchedAtDepth(3));
        record_hebrew_peel_depth(PeelOutcome::MatchedAtDepth(7)); // clamps to bucket 3
        record_hebrew_peel_depth(PeelOutcome::BudgetExhausted);
        let after = snapshot();
        assert_eq!(
            after.hebrew_peel_depth_0_matches,
            before.hebrew_peel_depth_0_matches + 1
        );
        assert_eq!(
            after.hebrew_peel_depth_1_matches,
            before.hebrew_peel_depth_1_matches + 1
        );
        assert_eq!(
            after.hebrew_peel_depth_2_matches,
            before.hebrew_peel_depth_2_matches + 1
        );
        // depth 3 AND clamped depth 7 → +2 in the depth-3 bucket
        assert_eq!(
            after.hebrew_peel_depth_3_matches,
            before.hebrew_peel_depth_3_matches + 2
        );
        assert_eq!(
            after.hebrew_peel_depth_exhausted,
            before.hebrew_peel_depth_exhausted + 1
        );
    }
}
