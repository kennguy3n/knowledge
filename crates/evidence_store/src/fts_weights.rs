//! Phase 1.8 — FTS5 BM25 weight constants for the three retrieval lanes.
//!
//! The substrate's lexical retrieval funnels every query through
//! [`crate::store::merged_fts_search`], which fans out over three
//! FTS5 virtual tables — `evidence_fts` (unicode61, whitespace-
//! segmented Latin / Cyrillic / Greek / Arabic / Hebrew /
//! Devanagari / Hangul; Phase 1.1+), `evidence_fts_cjk` (trigram,
//! CJK / Thai recall lane; Phase 1.2 / v14), and
//! `evidence_fts_bigram` (precomputed overlapping 2-codepoint
//! windows for ≤ 2-codepoint CJK / Thai queries; Phase 1.2.1 /
//! v15). Each lane returns its own BM25 `rank` and the three are
//! merged into one `(evidence_id → best_rank)` map by
//! [`crate::store::merged_fts_search`] before sorting + truncating
//! to the caller-requested `limit`.
//!
//! Phase 1.8 introduces two independent weight layers on top of
//! that merge pipeline:
//!
//! 1. **Column weights** (intra-lane) — passed inside each lane's
//!    SQL `bm25(<table>, <weight>...)` invocation, scoped to the
//!    indexed columns of that FTS5 table. Today every FTS5 table
//!    has exactly one indexed column (`content`) so every column-
//!    weight vector is `&[1.0]`; the call shape grows when a
//!    future schema bump adds additional indexed columns
//!    (e.g. a separate `subject` / `title` column would extend
//!    each lane's vector to `&[w_content, w_title]` with
//!    `w_title > w_content`). Today the column weights are a
//!    `1.0` no-op at runtime — their value is the call-site
//!    forward-compat shape: every read path is already a
//!    `bm25(<table>, w...)` invocation rather than a bare `rank`
//!    alias, so a future column addition becomes a one-line
//!    vector update here rather than a multi-file SQL audit.
//!
//! 2. **Lane weights** (inter-lane) — multiplied onto each
//!    lane's per-row BM25 rank *in Rust* immediately before the
//!    rank flows into the cross-lane `merge_min_rank` step. The
//!    lane weights encode the precision asymmetry of the three
//!    tokenisers:
//!    * `evidence_fts` (unicode61) tokenises full words, so its
//!      ranks reflect actual word matches — highest precision.
//!    * `evidence_fts_cjk` (trigram) tokenises overlapping 3-
//!      codepoint windows, so its ranks reflect substring
//!      adjacency — recall-oriented; a query that matches one
//!      shared trigram may not match the surrounding semantic
//!      context.
//!    * `evidence_fts_bigram` tokenises precomputed overlapping
//!      2-codepoint windows, designed specifically to recover
//!      ≤ 2-codepoint CJK / Thai queries that the trigram lane
//!      drops on its 3-codepoint floor — *highest* recall but
//!      *lowest* precision (a single shared bigram in a long
//!      body trivially satisfies the MATCH).
//!
//!    FTS5's BM25 rank is a finite negative `f64` (more negative
//!    = better match), so multiplying by a weight in `(0, 1]`
//!    moves the rank **closer to zero** — degrading it. This
//!    asymmetry means the trigram and bigram lanes' weighted
//!    ranks pay a precision penalty proportional to their recall
//!    bias: a row hit *only* by the bigram lane still ranks
//!    (additive recall is the whole point of the lane) but a
//!    row hit by *both* the unicode61 and bigram lanes resolves
//!    to the unicode61 rank because the merge takes the most
//!    negative score and the unicode61 lane carries weight
//!    `1.0` against bigram's `0.7`.
//!
//! The two layers are deliberately separate: column weights live
//! inside the FTS5 SQL because that is where intra-table
//! tf-idf scoring happens; lane weights live in Rust because the
//! cross-lane comparison is happening between *different* SQL
//! statements whose raw BM25 ranks are not directly comparable.
//! Folding the lane weights into the column-weight vector would
//! be a category error — the column weights would have to be
//! different per-lane to encode the precision asymmetry, and the
//! constant `&[1.0]` shape would no longer reflect the actual
//! column shape of the underlying FTS5 table.

/// Inter-lane BM25 weight for `evidence_fts`. Held at 1.0 as the
/// reference baseline — every other lane's precision is expressed
/// **relative to** the unicode61 lane, so this constant is the
/// implicit denominator of the lane-weight ratios. Bumping this
/// above 1.0 would re-anchor the ratios without changing the
/// relative ordering, so the substrate keeps it at 1.0 for clarity.
pub const EVIDENCE_FTS_LANE_WEIGHT: f64 = 1.0;

/// Inter-lane BM25 weight for `evidence_fts_cjk` (trigram lane).
///
/// `0.85` reflects a moderate precision penalty: the trigram lane
/// is the only lane that can serve a 3-codepoint CJK / Thai query
/// that the unicode61 lane has no tokens for, so its ranks must
/// remain competitive — but on cross-lane ties (the same row hit
/// by both `evidence_fts` and `evidence_fts_cjk` for a mixed-script
/// body) the unicode61 lane should win.
pub const EVIDENCE_FTS_CJK_LANE_WEIGHT: f64 = 0.85;

/// Inter-lane BM25 weight for `evidence_fts_bigram` (precomputed-
/// bigram lane).
///
/// `0.7` reflects a heavier precision penalty than the trigram
/// lane: the bigram lane is the highest-recall lane (2-codepoint
/// overlapping windows trivially match any body containing the
/// query's adjacent character pairs), so its ranks need to be
/// demoted enough that a single shared bigram in a long unrelated
/// body cannot outrank a real unicode61 / trigram hit on a related
/// body. The lane's role is to **recover** the ≤ 2-codepoint CJK /
/// Thai query case that the other two lanes drop, not to
/// dominate cross-lane ranking.
pub const EVIDENCE_FTS_BIGRAM_LANE_WEIGHT: f64 = 0.7;

/// Intra-lane BM25 column weights for `evidence_fts`. Single
/// `1.0` entry because the table indexes exactly one column
/// (`content`). When a future schema bump adds a second indexed
/// column (e.g. a separate `subject` / `title` column), grow
/// this vector to `&[w_content, w_title]` — the SQL
/// fragment builder at [`bm25_select_fragment_for_evidence_fts`]
/// reads its length to generate the matching `bm25(<table>, ...)`
/// argument list, so a column addition is a one-line change here
/// plus a `CREATE VIRTUAL TABLE` shape change in
/// [`crate::schema::SCHEMA_SQL`].
pub const EVIDENCE_FTS_COLUMN_WEIGHTS: &[f64] = &[1.0];

/// Intra-lane BM25 column weights for `evidence_fts_cjk`. Same
/// single-column shape as `evidence_fts` for the same reason —
/// the trigram lane indexes exactly one column.
pub const EVIDENCE_FTS_CJK_COLUMN_WEIGHTS: &[f64] = &[1.0];

/// Intra-lane BM25 column weights for `evidence_fts_bigram`. Same
/// single-column shape as the other two lanes.
pub const EVIDENCE_FTS_BIGRAM_COLUMN_WEIGHTS: &[f64] = &[1.0];

/// Build the SQL `bm25(<table>, w1, w2, ...)` fragment used in the
/// SELECT list of each lane's MATCH query.
///
/// Returns a string of the form `bm25(<table>, 1.0, 1.0)` (no
/// trailing whitespace). The fragment is meant to replace a bare
/// `rank` alias so that future column additions become an
/// `EVIDENCE_FTS_*_COLUMN_WEIGHTS` constant update rather than a
/// SQL audit. The `table` argument must be a substrate-controlled
/// identifier (`evidence_fts` / `evidence_fts_cjk` /
/// `evidence_fts_bigram`) — there is no escaping because the
/// argument is never derived from user input.
pub fn bm25_select_fragment(table: &str, column_weights: &[f64]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(8 + table.len() + column_weights.len() * 5);
    s.push_str("bm25(");
    s.push_str(table);
    for w in column_weights {
        // `{:?}` on `f64` always renders a decimal point (e.g.
        // `1.0` rather than `1`) so the SQL parser sees a REAL
        // literal — important on edge cases where SQLite's
        // numeric affinity would otherwise coerce an integer-
        // shaped literal into INTEGER for arithmetic.
        write!(s, ", {w:?}").expect("write to String never fails");
    }
    s.push(')');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_weights_form_strict_precision_hierarchy() {
        // Phase 1.8 invariant: the three lanes encode a strict
        // precision ordering — unicode61 (whole-word) wins over
        // trigram (3-codepoint windows), which wins over bigram
        // (2-codepoint windows). The lane weights are the
        // mechanism that pins this ordering at merge time, so a
        // regression that re-orders or equalises them silently
        // collapses the precision asymmetry that motivated the
        // three-lane architecture in the first place.
        //
        // Const-block asserts so the precision hierarchy fails at
        // compile time on a regression rather than at test time.
        // The `clippy::assertions_on_constants` lint specifically
        // recommends this for const-folded comparisons.
        const _: () = assert!(EVIDENCE_FTS_LANE_WEIGHT > EVIDENCE_FTS_CJK_LANE_WEIGHT);
        const _: () = assert!(EVIDENCE_FTS_CJK_LANE_WEIGHT > EVIDENCE_FTS_BIGRAM_LANE_WEIGHT);
    }

    #[test]
    fn lane_weights_are_in_open_unit_interval() {
        // Phase 1.8 invariant: lane weights live in `(0, 1]` —
        // `1.0` for the precision baseline (unicode61), strictly
        // less than `1.0` for recall lanes (trigram, bigram).
        // A weight `≤ 0` would either zero out a lane (rank
        // becomes `0.0` regardless of match strength) or invert
        // it (more-negative-is-better becomes less-negative);
        // a weight `> 1.0` would *boost* a lane above the
        // unicode61 baseline, inverting the precision hierarchy.
        for (name, w) in [
            ("evidence_fts", EVIDENCE_FTS_LANE_WEIGHT),
            ("evidence_fts_cjk", EVIDENCE_FTS_CJK_LANE_WEIGHT),
            ("evidence_fts_bigram", EVIDENCE_FTS_BIGRAM_LANE_WEIGHT),
        ] {
            assert!(
                w > 0.0 && w <= 1.0,
                "{name} lane weight {w} must be in (0, 1] — see module docstring"
            );
        }
    }

    #[test]
    fn column_weights_match_single_column_fts5_shape() {
        // Phase 1.8 invariant: every FTS5 table in the substrate
        // currently indexes exactly one column. The column-weight
        // vector length is the integration point for future
        // multi-column FTS5 tables — a length mismatch between
        // this constant and the actual `CREATE VIRTUAL TABLE`
        // column count would mean
        // `bm25(<table>, EVIDENCE_FTS_*_COLUMN_WEIGHTS...)`
        // sees the wrong number of weight arguments and SQLite
        // returns a runtime error. Pin the count here so the
        // schema authors of any future v16+ migration have an
        // obvious test-failure breadcrumb pointing at this file.
        assert_eq!(
            EVIDENCE_FTS_COLUMN_WEIGHTS.len(),
            1,
            "evidence_fts has 1 indexed column today (content); \
             schema bump that adds a column must also extend \
             EVIDENCE_FTS_COLUMN_WEIGHTS"
        );
        assert_eq!(
            EVIDENCE_FTS_CJK_COLUMN_WEIGHTS.len(),
            1,
            "evidence_fts_cjk has 1 indexed column today (content); \
             schema bump that adds a column must also extend \
             EVIDENCE_FTS_CJK_COLUMN_WEIGHTS"
        );
        assert_eq!(
            EVIDENCE_FTS_BIGRAM_COLUMN_WEIGHTS.len(),
            1,
            "evidence_fts_bigram has 1 indexed column today (content); \
             schema bump that adds a column must also extend \
             EVIDENCE_FTS_BIGRAM_COLUMN_WEIGHTS"
        );
    }

    #[test]
    fn column_weights_are_positive_finite_real_numbers() {
        // Phase 1.8 invariant: column weights are passed verbatim
        // into FTS5's bm25() call. FTS5 silently treats a NaN or
        // negative weight as zero (column drops out of the
        // ranking) — a defensive guard here pins the constants
        // to finite positive reals so the lane weights are the
        // only mechanism for downweighting.
        for (name, ws) in [
            ("EVIDENCE_FTS_COLUMN_WEIGHTS", EVIDENCE_FTS_COLUMN_WEIGHTS),
            (
                "EVIDENCE_FTS_CJK_COLUMN_WEIGHTS",
                EVIDENCE_FTS_CJK_COLUMN_WEIGHTS,
            ),
            (
                "EVIDENCE_FTS_BIGRAM_COLUMN_WEIGHTS",
                EVIDENCE_FTS_BIGRAM_COLUMN_WEIGHTS,
            ),
        ] {
            for (i, w) in ws.iter().enumerate() {
                assert!(
                    w.is_finite() && *w > 0.0,
                    "{name}[{i}] = {w} must be finite and strictly positive"
                );
            }
        }
    }

    #[test]
    fn bm25_select_fragment_unicode61_lane() {
        // Phase 1.8: pin the exact SQL fragment shape so the
        // call sites in `merged_fts_search` cannot drift from
        // the bm25() argument list FTS5 expects. The fragment
        // is `bm25(<table>, w...)` with `{:?}` formatting on
        // each weight so the SQL parser sees a REAL literal
        // (e.g. `1.0`) rather than an INTEGER literal (`1`).
        assert_eq!(
            bm25_select_fragment("evidence_fts", EVIDENCE_FTS_COLUMN_WEIGHTS),
            "bm25(evidence_fts, 1.0)"
        );
    }

    #[test]
    fn bm25_select_fragment_cjk_lane() {
        assert_eq!(
            bm25_select_fragment("evidence_fts_cjk", EVIDENCE_FTS_CJK_COLUMN_WEIGHTS),
            "bm25(evidence_fts_cjk, 1.0)"
        );
    }

    #[test]
    fn bm25_select_fragment_bigram_lane() {
        assert_eq!(
            bm25_select_fragment("evidence_fts_bigram", EVIDENCE_FTS_BIGRAM_COLUMN_WEIGHTS),
            "bm25(evidence_fts_bigram, 1.0)"
        );
    }

    #[test]
    fn bm25_select_fragment_extends_with_extra_columns() {
        // Phase 1.8 forward-compat: when a future schema bump
        // adds a second indexed column (e.g. `title`), grow the
        // column-weight vector and the fragment grows in lockstep.
        // Pin the multi-weight call shape now so the integration
        // point is exercised before the schema actually adds the
        // column.
        assert_eq!(
            bm25_select_fragment("evidence_fts", &[1.0, 2.0]),
            "bm25(evidence_fts, 1.0, 2.0)"
        );
        assert_eq!(
            bm25_select_fragment("evidence_fts_cjk", &[1.5, 0.5, 0.25]),
            "bm25(evidence_fts_cjk, 1.5, 0.5, 0.25)"
        );
    }

    #[test]
    fn apply_lane_weight_preserves_negative_bm25_sign() {
        // Phase 1.8 invariant: applying a `(0, 1]` lane weight
        // to a negative BM25 rank keeps the rank negative
        // (multiplying two non-zero same-sign reals stays in
        // the same sign sector). Pin this so a regression that
        // accidentally swaps in a `(score + weight)` or `(weight
        // - score)` formula instead of a multiply fails loudly.
        //
        // Raw rank chosen as `-2.5` (not `-3.14`) to avoid the
        // clippy `approx_constant` warning about PI-shaped
        // literals — the actual value is unimportant, only its
        // negative-finite sign matters for the invariant.
        let raw_rank: f64 = -2.5;
        let weighted = raw_rank * EVIDENCE_FTS_CJK_LANE_WEIGHT;
        assert!(
            weighted < 0.0,
            "weighted rank {weighted} must stay negative"
        );
        assert!(
            weighted > raw_rank,
            "weighted rank {weighted} must be closer to zero than raw rank {raw_rank} \
             (precision penalty: smaller |rank| means worse cross-lane comparison)"
        );
    }

    #[test]
    fn apply_lane_weight_baseline_is_identity() {
        // Phase 1.8: the unicode61 lane weight is the precision
        // baseline (1.0), so multiplying a raw rank by it is a
        // no-op. Pin this so a regression that nudges the
        // baseline weight off 1.0 silently shifts the cross-
        // lane ratios without re-anchoring.
        //
        // Use `f64::to_bits` for exact equality (clippy::float_cmp
        // disallows raw `==` on f64). Identity multiplication by
        // 1.0 must preserve the bit-exact representation —
        // anything else would indicate a non-1.0 weight or a
        // float-arithmetic regression.
        let raw_rank: f64 = -2.5;
        let weighted = raw_rank * EVIDENCE_FTS_LANE_WEIGHT;
        assert_eq!(
            weighted.to_bits(),
            raw_rank.to_bits(),
            "EVIDENCE_FTS_LANE_WEIGHT = 1.0 must be the bit-exact identity on rank \
             multiplication (weighted={weighted}, raw={raw_rank})"
        );
    }
}
