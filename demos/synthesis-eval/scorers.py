#!/usr/bin/env python3
"""
Deterministic, offline synthesis-quality scorers.

These functions score *already-recorded* model output — they never call a
model, a network, the clock or an RNG — so the same inputs always yield the
same scores. That is what lets the harness run in CI on a box with no GPU and
no llama-server, and what makes the regression gate meaningful.

Three scorers, mirroring the gaps named in the catch-up proposal (G1/G6):

  1. term_coverage(recap, expected)   — factual/term coverage of a recap
                                         against a *labeled* expected-terms
                                         fixture (the thing a buyer reads for).
  2. ungrounded_entities(recap, ev)   — faithfulness/grounding: recap entities
                                         (identifiers, codes, brand names) that
                                         do NOT appear in the session evidence,
                                         i.e. likely hallucinations.
  3. in_language(script, recap)       — in-language correctness via a Unicode
                                         script detector (an Arabic session
                                         answered in English fails here even
                                         though it is "usable" text).

The script detector and the salient-term tokeniser are deliberately kept
byte-for-byte consistent with the existing demo logic
(`demos/multilingual-rollup/run_rollup.py`) and the production crate
(`crates/synthesis_pipeline/src/quality.rs::salient_terms_from_texts` and
`crates/synthesis_pipeline/src/eval.rs`) so the demo, the CI gate and the
shipped library all agree on what they measure.

No third-party dependencies: a non-developer can read it top-to-bottom.
"""

from __future__ import annotations

import re
import unicodedata
from dataclasses import dataclass, field

# --------------------------------------------------------------------------- #
# 1. In-language correctness (Unicode script detector)
# --------------------------------------------------------------------------- #
# Per-language script family. Mirrors `SCRIPTS` in
# demos/multilingual-rollup/run_rollup.py and the `Script` enum in
# crates/synthesis_pipeline/src/eval.rs.
SCRIPTS: dict[str, str] = {
    "English": "Latin", "French": "Latin", "German": "Latin", "Spanish": "Latin",
    "Vietnamese": "Latin", "Indonesian": "Latin", "Portuguese": "Latin",
    "Japanese": "CJK", "Chinese": "CJK", "Thai": "Thai", "Arabic": "Arabic",
    "Hindi": "Devanagari",
}
LATIN_SCRIPTS = {"Latin"}


def script_of(lang: str) -> str:
    """Map a language name to its script family, defaulting to Latin for any
    not-yet-classified language (the safe assumption that never over-claims a
    non-Latin stress case)."""
    return SCRIPTS.get(lang, "Latin")


def is_non_latin(lang: str) -> bool:
    return script_of(lang) not in LATIN_SCRIPTS


def _script_of_char(ch: str) -> str | None:
    """Map one alphabetic character to a script bucket, or None for
    non-alphabetic characters (digits, punctuation, whitespace)."""
    if not ch.isalpha():
        return None
    try:
        name = unicodedata.name(ch)
    except ValueError:
        return None
    if "CJK" in name or "HIRAGANA" in name or "KATAKANA" in name:
        return "CJK"
    if "THAI" in name:
        return "Thai"
    if "ARABIC" in name:
        return "Arabic"
    if "DEVANAGARI" in name:
        return "Devanagari"
    if "LATIN" in name:
        return "Latin"
    return "Other"


def in_language(lang: str, recap: str) -> bool:
    """Honest check that a recap is written in the session's own language, not
    merely that it is usable text.

    Business tokens (``MySQL``, ``Postgres``, ``SKU-6310``, ``VNPay``) are
    legitimately Latin even inside a Thai/Arabic/CJK recap, so we compare
    *alphabetic* character counts by script rather than demanding a pure block:

      - Latin-script languages pass only when *zero* alphabetic characters of
        another known script (CJK/Thai/Arabic/Devanagari) appear.
      - Non-Latin languages pass when the expected script is at least as
        prevalent as Latin — tolerating embedded Latin product names while
        still failing a recap that answered, say, an Arabic session in English.

    An empty/placeholder recap has no alphabetic characters and never counts as
    in-language. Mirrors `in_language` in run_rollup.py and
    `recap_in_language` in crates/synthesis_pipeline/src/eval.rs.
    """
    counts: dict[str, int] = {}
    for ch in recap or "":
        s = _script_of_char(ch)
        if s:
            counts[s] = counts.get(s, 0) + 1
    if not counts:
        return False
    expected = script_of(lang)
    latin = counts.get("Latin", 0)
    if expected in LATIN_SCRIPTS:
        non_latin = sum(v for k, v in counts.items() if k not in ("Latin", "Other"))
        return latin > 0 and non_latin == 0
    return counts.get(expected, 0) >= latin


# --------------------------------------------------------------------------- #
# 2. Term / factual coverage against a labeled fixture
# --------------------------------------------------------------------------- #
@dataclass(frozen=True)
class Coverage:
    """Coverage of a recap against a labeled expected-terms set."""

    matched: list[str]
    missing: list[str]
    expected: list[str]

    @property
    def fraction(self) -> float:
        if not self.expected:
            return 1.0
        return len(self.matched) / len(self.expected)


def term_coverage(recap: str, expected_terms: list[str]) -> Coverage:
    """Fraction of labeled expected terms a recap mentions.

    Case-insensitive substring match — identical to how the existing persona
    harness computes ``recap_term_coverage`` (``t.lower() in recap.lower()``),
    so a recap that already passed there scores the same here. Order of
    ``expected_terms`` is preserved in ``matched``/``missing`` for a stable,
    diffable report.
    """
    low = (recap or "").lower()
    matched = [t for t in expected_terms if t.lower() in low]
    missing = [t for t in expected_terms if t.lower() not in low]
    return Coverage(matched=matched, missing=missing, expected=list(expected_terms))


# --------------------------------------------------------------------------- #
# 3. Faithfulness / grounding (recap entities absent from the evidence)
# --------------------------------------------------------------------------- #
MIN_SALIENT_TERM_LEN = 4  # mirrors quality.rs::MIN_SALIENT_TERM_LEN

# CamelCase / internal-uppercase brand tokens: CartoNord, GoCardless, MySQL,
# PostgreSQL. Requires a lowercase (or digit) *followed by* an uppercase letter,
# so a leading-acronym brand like "VNPay" (upper->lower at P->a, no lower->upper)
# is intentionally NOT matched here — that is the precision-first trade-off, not
# an oversight.
_CAMEL = re.compile(r"[a-z0-9][A-Z]")
# All-caps acronym, length 3-6: SKU, EUR, OTA, PBC. Two-letter all-caps tokens
# (VP, HR, IT) are too often generic abbreviations to treat as named entities.
_ACRONYM = re.compile(r"^[A-Z]{3,6}$")
# Capitalised proper noun: Priya, Postgres, Adyen, Keyence, Marubeni.
_PROPER = re.compile(r"^[A-Z][a-zà-öø-ÿ][\w'-]*$")
# Ordinals (6th, 1st, 22nd) carry a digit but are not identifiers.
_ORDINAL = re.compile(r"^\d+(st|nd|rd|th)$", re.IGNORECASE)
# Sentence-final punctuation (Latin + CJK), used to spot sentence-initial words.
_SENT_END = set(".!?:;…。！？")


def _is_sentence_initial(tokens: list[str], i: int) -> bool:
    """True when token ``i`` starts a sentence (so a capitalised word there is
    most likely a function word — ``Decision``, ``Quality``, ``Production`` —
    not a named entity)."""
    if i == 0:
        return True
    prev = tokens[i - 1].rstrip("\"')]}»”’")
    return bool(prev) and prev[-1] in _SENT_END


def salient_terms(texts: list[str], min_len: int = MIN_SALIENT_TERM_LEN) -> list[str]:
    """Deduplicated, lowercased, first-seen-ordered salient tokens.

    The same notion of "salient" as ``quality.rs::salient_terms_from_texts``:
    split on non-alphanumeric Unicode scalar values, keep tokens of at least
    ``min_len`` characters, lowercase, dedupe preserving first-seen order.
    Language-agnostic (no per-language word list).

    Tokenisation uses ``str.isalnum`` per character, which tracks Rust's
    ``char::is_alphanumeric`` (so e.g. ``"abcd×efgh"`` splits into two tokens in
    both, rather than being kept whole). This is the recap analogue the Rust
    ``eval::ungrounded_recap_terms`` grounding is built on; the parity is pinned
    by ``test_synthesis_eval.SalientTerms``.
    """
    seen: set[str] = set()
    out: list[str] = []
    for text in texts:
        token: list[str] = []
        for ch in text or "":
            if ch.isalnum():
                token.append(ch)
                continue
            if len(token) >= min_len:
                term = "".join(token).lower()
                if term not in seen:
                    seen.add(term)
                    out.append(term)
            token.clear()
        if len(token) >= min_len:
            term = "".join(token).lower()
            if term not in seen:
                seen.add(term)
                out.append(term)
    return out


def _strip_token(tok: str) -> str:
    """Trim surrounding punctuation but keep internal hyphens/dots/digits so
    ``FA-2025-0411,`` -> ``FA-2025-0411`` and ``(SKU-8842)`` -> ``SKU-8842``."""
    return tok.strip(" \t\n\r.,;:!?()[]{}\"'«»“”‘’、。：，！？…")


def _digits(s: str) -> str:
    return re.sub(r"\D", "", s)


def recap_entities(recap: str) -> list[str]:
    """Extract the named entities / identifiers a faithful recap must have
    drawn from the evidence. Deterministic and documented (see README):

      a) tokens with a digit and >=2 chars, excluding ordinals — SKU-8842,
         FA-2025-0411, BR-2505, v2.4.1, 12,4, AX-7  (not ``6th``)
      b) CamelCase / internal-uppercase brand tokens          — CartoNord,
         MySQL, VNPay
      c) ALL-CAPS acronyms of length 3-6                       — SKU, EUR, OTA
      d) capitalised proper nouns (>=4 chars), excluding        — Priya,
         sentence-initial words (a capitalised word that          Postgres,
         opens a sentence is usually a function word —            Adyen
         ``Decision``, ``Quality`` — not a named entity)

    Rule (d) deliberately favours precision over recall: a genuine entity that
    happens to open a sentence is skipped rather than risk flagging a common
    word as a hallucination. Grounding is a weak, precision-first signal, so
    under-detection is the safe failure mode.

    First-seen order is preserved; duplicates (case-folded) are dropped.
    """
    seen: set[str] = set()
    out: list[str] = []
    tokens = (recap or "").split()
    for i, raw in enumerate(tokens):
        tok = _strip_token(raw)
        if not tok:
            continue
        is_entity = False
        if any(c.isdigit() for c in tok) and len(tok) >= 2 and not _ORDINAL.match(tok):
            is_entity = True
        elif _CAMEL.search(tok):
            is_entity = True
        elif _ACRONYM.match(tok):
            is_entity = True
        elif (_PROPER.match(tok) and len(tok) >= 4
              and not _is_sentence_initial(tokens, i)):
            is_entity = True
        if not is_entity:
            continue
        key = tok.lower()
        if key not in seen:
            seen.add(key)
            out.append(tok)
    return out


@dataclass(frozen=True)
class Grounding:
    """Faithfulness of a recap against the session evidence."""

    entities: list[str]
    ungrounded: list[str]

    @property
    def grounded_fraction(self) -> float:
        if not self.entities:
            return 1.0
        return 1.0 - len(self.ungrounded) / len(self.entities)


def ungrounded_entities(recap: str, evidence_texts: list[str]) -> Grounding:
    """Recap entities that do not appear in the session evidence.

    An entity is *grounded* when its case-folded form is a substring of the
    case-folded evidence corpus, OR (for numeric/identifier tokens) when its
    digit run is a substring of the evidence's digit run — so ``12,600`` is
    grounded by evidence ``12 600`` despite the thousands separator. Anything
    left over is flagged as a likely hallucination.

    A recap with no extractable entities is vacuously grounded (no claim to
    verify), so ``ungrounded`` is empty.
    """
    corpus = "\n".join(evidence_texts).lower()
    corpus_digits = _digits(corpus)
    ents = recap_entities(recap)
    ungrounded: list[str] = []
    for ent in ents:
        low = ent.lower()
        if low in corpus:
            continue
        d = _digits(ent)
        # Pure-numeric / numeric-bearing tokens: match on the digit run so
        # locale separators (12 600 vs 12,600) don't cause false flags.
        if d and len(d) >= 2 and d in corpus_digits:
            continue
        ungrounded.append(ent)
    return Grounding(entities=ents, ungrounded=ungrounded)


# --------------------------------------------------------------------------- #
# Combined per-recap score
# --------------------------------------------------------------------------- #
@dataclass
class RecapScore:
    """The three scores for a single recorded recap."""

    label: str
    language: str
    script: str = ""
    coverage: Coverage | None = None
    grounding: Grounding | None = None
    in_lang: bool | None = None
    notes: list[str] = field(default_factory=list)
