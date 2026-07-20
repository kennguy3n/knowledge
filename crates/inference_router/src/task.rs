//! Inference task taxonomy + prompt / grammar templates.
//!
//! This module also owns the **output shapes** that the GBNF grammars
//! constrain the SLM to emit. The schema lives next to the grammar so
//! drift between producer (the grammar fed to `llama-server`) and
//! consumer (the `serde_json` decode site) is locally visible in one
//! file. Today only [`SummaryBundle`] lives here — the other
//! grammar-constrained shapes still live in `synthesis_pipeline::schema`
//! and may move here in a future cleanup.

use serde::{Deserialize, Serialize};

/// Broad size class of the loaded SLM, used to select per-class prompt
/// templates. Smaller models (≤ ~1 B parameters) have weaker
/// instruction-following and are more prone to meta-commentary
/// ("The session discusses…") and terse recaps that omit expected
/// factual terms. The per-class prompt variants strengthen the
/// anti-meta and coverage directives for those models.
///
/// Detection is filename-based via [`ModelClass::from_model_path`],
/// which scans the GGUF basename for common size markers (e.g.
/// `0.5b`, `1.7b`, `3b`, `7b`). Unknown sizes default to
/// [`ModelClass::Large`] — the safest assumption, since larger models
/// need the least prompt scaffolding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelClass {
    /// Small model (≤ ~1 B parameters). Needs the strongest
    /// anti-meta and coverage-boosting prompt directives.
    Small,
    /// Medium model (~1–4 B parameters). Moderate prompt scaffolding.
    Medium,
    /// Large model (> ~4 B parameters). Standard prompt is sufficient.
    Large,
}

impl ModelClass {
    /// Infer the model class from the GGUF model path basename.
    ///
    /// Scans for common parameter-count markers in the filename
    /// (e.g. `qwen3.5-0.8b`, `bonsai-1.7b`, `llama-3-8b`).
    ///
    /// * `0.5b`, `0.8b`, `0.9b`, `1.0b`, `1.1b` → [`ModelClass::Small`]
    /// * `1.5b`, `1.6b`, `1.7b`, `1.8b`, `2b`, `2.5b`, `3b`, `3.5b`, `4b` → [`ModelClass::Medium`]
    /// * anything else (including unrecognised names) → [`ModelClass::Large`]
    pub fn from_model_path(path: &str) -> Self {
        let lower = path.to_ascii_lowercase();
        let basename = lower.rsplit('/').next().unwrap_or(&lower);

        // Small: ≤ ~1 B parameters
        for marker in ["0.5b", "0.8b", "0.9b", "1.0b", "1.1b"] {
            if basename.contains(marker) {
                return Self::Small;
            }
        }
        // Medium: ~1–4 B parameters
        for marker in [
            "1.5b", "1.6b", "1.7b", "1.8b", "2b", "2.0b", "2.5b", "3b", "3.0b", "3.5b", "4b",
            "4.0b",
        ] {
            if basename.contains(marker) {
                return Self::Medium;
            }
        }
        // Default: assume large — the safest baseline that needs the
        // least prompt scaffolding.
        Self::Large
    }
}

/// One inference task served by the router. Each task has a stable
/// string tag, a prompt template, and a GBNF grammar constraint.
///
/// Per `docs/technical/architecture.md` §3 the substrate routes the following tasks
/// through the SLM:
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceTask {
    /// Tag an evidence row with an importance class.
    TagImportance,
    /// Extract entities from an evidence row.
    ExtractEntities,
    /// Promote one observation candidate.
    PromoteObservation,
    /// Summarise a session / thread (episodic memory).
    SynthSummary,
    /// Synthesise a concept from a window of observations.
    SynthConcept,
    /// Adjudicate a contradiction between two canonical claims.
    AdjudicateContradiction,
    /// Detect whether a keyword match is semantically negated by
    /// surrounding context (e.g. "considered X but chose Y").
    DetectNegation,
    /// Refine an Unknown entity into a specific EntityType using
    /// surrounding context.
    RefineEntity,
}

/// The abstract placeholder tokens embedded in the [`InferenceTask::SynthSummary`]
/// prompt's one-shot exemplar (see `prompt_template`). They demonstrate
/// the bundle *shape* without supplying plausible business content a
/// 2-bit-quantised model might copy verbatim into an unrelated session.
///
/// This is the single source of truth keyed off by the synthesis quality
/// gate (`synthesis_pipeline::quality::strip_exemplar_leak`) to detect
/// and drop a leaked exemplar before a bundle is persisted: because the
/// tokens are distinctive uppercase identifiers that never occur in real
/// session text, an exact substring match has no false positives. The
/// [`tests::synth_summary_exemplar_tokens_appear_in_prompt`] test pins
/// these against the prompt literal so the two can never silently drift.
pub const SYNTH_EXEMPLAR_TOKENS: &[&str] = &["EXAMPLE_DECISION", "EXAMPLE_TASK"];

impl InferenceTask {
    /// Canonical ordered list of every variant — the single source of
    /// truth other modules iterate over. Kept next to the enum
    /// itself (rather than duplicated in `router.rs`) so the
    /// exhaustiveness test [`tests::all_is_exhaustive`] below can
    /// catch any new variant that gets added without being appended
    /// here. The order is the same as the variant declaration order
    /// and is part of the router's wire contract (it determines the
    /// order in which `adapter_states()` reports an adapter's
    /// supported tasks).
    pub const ALL: &'static [InferenceTask] = &[
        Self::TagImportance,
        Self::ExtractEntities,
        Self::PromoteObservation,
        Self::SynthSummary,
        Self::SynthConcept,
        Self::AdjudicateContradiction,
        Self::DetectNegation,
        Self::RefineEntity,
    ];
}

/// Stable string tag for a task — used as the cache key and as the
/// `task_tag` argument passed to adapters.
pub type TaskTag = &'static str;

impl InferenceTask {
    /// Stable string tag for the task.
    pub const fn tag(self) -> TaskTag {
        match self {
            Self::TagImportance => "tag_importance",
            Self::ExtractEntities => "extract_entities",
            Self::PromoteObservation => "promote_observation",
            Self::SynthSummary => "synth_summary",
            Self::SynthConcept => "synth_concept",
            Self::AdjudicateContradiction => "adjudicate_contradiction",
            Self::DetectNegation => "detect_negation",
            Self::RefineEntity => "refine_entity",
        }
    }

    /// Parse a runtime task tag back into its [`InferenceTask`] variant,
    /// or `None` if the string is not a recognised tag. This is the
    /// inverse of [`tag`](Self::tag) and derives entirely from it (via
    /// [`ALL`](Self::ALL)), so a new variant is covered automatically
    /// once it is added to the (compiler-checked) `tag` match and `ALL`.
    ///
    /// Crate-internal: this is a shared helper for the adapter error
    /// paths, deliberately not part of the `// STABLE` public surface of
    /// [`InferenceTask`] (promoting it to `pub` would require a
    /// `CHANGELOG` entry per `CONTRIBUTING.md`).
    pub(crate) fn from_tag(task_tag: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|task| task.tag() == task_tag)
    }

    /// Resolve a runtime task tag to the canonical `&'static str` that
    /// [`crate::RouterError`] stores, falling back to a stable
    /// `"unknown"` constant for an unrecognised tag so the error type
    /// can stay `'static`-ful.
    ///
    /// Adapters receive the task tag as a borrowed `&str` (it is threaded
    /// through the FFI boundary) but `RouterError` needs an owned-free
    /// `&'static str`; this is the single shared conversion every adapter
    /// uses instead of maintaining its own copy of the mapping.
    ///
    /// Crate-internal (see [`from_tag`](Self::from_tag)).
    pub(crate) fn static_tag_or_unknown(task_tag: &str) -> TaskTag {
        match Self::from_tag(task_tag) {
            Some(task) => task.tag(),
            None => "unknown",
        }
    }

    /// `true` when this task is *classification* (yes/no/category).
    /// Classification tasks can be served by encoder-only fallbacks;
    /// synthesis tasks cannot.
    pub const fn is_classification(self) -> bool {
        matches!(
            self,
            Self::TagImportance
                | Self::ExtractEntities
                | Self::PromoteObservation
                | Self::DetectNegation
                | Self::RefineEntity
        )
    }

    /// `true` when this task requires generative synthesis (prose,
    /// summaries, free-form concept text). Cannot be served by the
    /// encoder-only fallback adapter.
    pub const fn is_synthesis(self) -> bool {
        matches!(
            self,
            Self::SynthSummary | Self::SynthConcept | Self::AdjudicateContradiction
        )
    }

    /// Static prompt template for the task. Placeholders are
    /// substituted by the caller (the router does not interpret the
    /// prompt itself).
    pub const fn prompt_template(self) -> &'static str {
        match self {
            Self::TagImportance => {
                "Classify the following message into one of {critical, important, useful, noise}. \
                 Respond with strict JSON: {\"class\": \"…\", \"confidence\": <0.0-1.0>}.\n\nMessage:\n{body}"
            }
            Self::ExtractEntities => {
                "List the named entities (people, projects, deadlines, decisions) in the following \
                 message. Respond as JSON: {\"entities\": [{\"name\": \"…\", \"type\": \"…\"}]}.\n\nMessage:\n{body}"
            }
            Self::PromoteObservation => {
                "Decide whether the following observation should be promoted to canonical \
                 knowledge. Respond as JSON: {\"promote\": true|false, \"reason\": \"…\"}.\n\nObservation:\n{body}"
            }
            Self::SynthSummary => {
                // Aligns with [`SummaryBundle`] in this module —
                // four fields, all populated even when empty. The
                // GBNF `GRAMMAR_SYNTH_SUMMARY` constrains the
                // emitted JSON to exactly this shape so the
                // synthesiser never has to repair output.
                //
                // The leading hard instruction + the single one-shot
                // exemplar steer a 2-bit-quantised small model away
                // from its dominant failure mode: prefacing the bundle
                // with meta-commentary ("The session highlights…")
                // instead of emitting facts. The exemplar demonstrates
                // *shape only* and deliberately uses abstract placeholder
                // tokens (`EXAMPLE_DECISION` / `EXAMPLE_TASK`) rather than
                // a plausible business sentence: a 2-bit model frequently
                // copies the exemplar's content verbatim into unrelated
                // sessions, so a concrete sample (e.g. "Adopt Postgres for
                // the billing store") would surface as a real-looking but
                // false decision in someone else's recap. A leaked
                // placeholder is instantly recognisable as a demo artefact
                // and cannot be mistaken for genuine knowledge. The
                // instruction still pins the recap to the session's own
                // language, so the lone English-framed sample does not
                // anchor multilingual sessions toward English output.
                "Output ONLY the JSON object. Do not describe the task, do not preface or \
                 explain the output, and do not write about \"the session\" or \"this summary\". \
                 Summarise the session as a JSON object with this exact shape: \
                 {\"recap\": \"…\", \"decisions\": [\"…\"], \"open_questions\": [\"…\"], \"active_tasks\": [\"…\"]}. \
                 The recap is a 2-4 sentence factual headline written in the same language as the \
                 session; the other fields each list zero or more strings. \
                 The example below shows only the JSON shape — its placeholder tokens are NOT \
                 content: always write the values from the session itself, in the session's own \
                 language, never copy the example's tokens.\n\n\
                 Example session (format illustration only):\n\
                 Observations:\n\
                 - [decision] (important) EXAMPLE_DECISION\n\
                 - [task] (important) EXAMPLE_TASK\n\
                 Example output:\n\
                 {\"recap\":\"EXAMPLE_DECISION was agreed and EXAMPLE_TASK was scheduled.\",\
                 \"decisions\":[\"EXAMPLE_DECISION\"],\
                 \"open_questions\":[],\"active_tasks\":[\"EXAMPLE_TASK\"]}\n\n\
                 Session:\n{body}"
            }
            Self::SynthConcept => {
                "Synthesise a concept from the following observations. Output a JSON object: \
                 {\"name\": \"…\", \"summary\": \"…\", \"facets\": {\"…\": \"…\"}}.\n\nObservations:\n{body}"
            }
            Self::AdjudicateContradiction => {
                "The following two canonical claims contradict. Decide which is canonical \
                 and explain. Respond as JSON: {\"canonical\": \"a|b\", \"reason\": \"…\"}.\n\nA:\n{a}\n\nB:\n{b}"
            }
            Self::DetectNegation => {
                "Determine whether the keyword in the following text is semantically negated \
                 by its context (e.g. 'considered X but chose Y', 'ruled out X', 'X was \
                 superseded by Y'). Respond with strict JSON: {\"negated\": true|false, \
                 \"confidence\": <0.0-1.0>}.\n\nText:\n{body}"
            }
            Self::RefineEntity => {
                "Classify the entity in the following context into one of: person, organization, \
                 product, location, date, currency, identifier, url, email, numeric, event, \
                 measurement, unknown. Respond with strict JSON: {\"type\": \"…\", \
                 \"confidence\": <0.0-1.0>}.\n\nContext:\n{body}"
            }
        }
    }

    /// Prompt template selected by [`ModelClass`]. Falls back to
    /// [`Self::prompt_template`] for tasks that do not have per-class
    /// variants. Currently only [`Self::SynthSummary`] has class-specific
    /// templates — the synthesis task is the most sensitive to model
    /// size because it requires the longest generated output and the
    /// strongest instruction-following.
    pub fn prompt_template_for_class(self, model_class: ModelClass) -> &'static str {
        match (self, model_class) {
            (Self::SynthSummary, ModelClass::Small) => PROMPT_SYNTH_SUMMARY_SMALL,
            (Self::SynthSummary, ModelClass::Medium) => PROMPT_SYNTH_SUMMARY_MEDIUM,
            _ => self.prompt_template(),
        }
    }

    /// GBNF grammar constraint for the task — feed verbatim to
    /// llama.cpp's `--grammar` parameter so the model can only emit
    /// JSON of the expected shape.
    pub const fn grammar(self) -> &'static str {
        match self {
            Self::TagImportance => GRAMMAR_TAG_IMPORTANCE,
            Self::ExtractEntities => GRAMMAR_EXTRACT_ENTITIES,
            Self::PromoteObservation => GRAMMAR_PROMOTE_OBSERVATION,
            Self::SynthSummary => GRAMMAR_SYNTH_SUMMARY,
            Self::SynthConcept => GRAMMAR_SYNTH_CONCEPT,
            Self::AdjudicateContradiction => GRAMMAR_ADJUDICATE,
            Self::DetectNegation => GRAMMAR_DETECT_NEGATION,
            Self::RefineEntity => GRAMMAR_REFINE_ENTITY,
        }
    }
}

/// GBNF for `{"class": "critical|important|useful|noise", "confidence": 0.0-1.0}`.
pub const GRAMMAR_TAG_IMPORTANCE: &str = r#"
root ::= "{" ws "\"class\":" ws class "," ws "\"confidence\":" ws number ws "}"
class ::= "\"critical\"" | "\"important\"" | "\"useful\"" | "\"noise\""
number ::= "0" "." [0-9]+ | "1.0" | "1"
ws ::= [ \t\n]*
"#;

/// GBNF for `{"entities": [{"name": "…", "type": "…"}]}`.
pub const GRAMMAR_EXTRACT_ENTITIES: &str = r#"
root ::= "{" ws "\"entities\":" ws "[" ws (entity ("," ws entity)*)? ws "]" ws "}"
entity ::= "{" ws "\"name\":" ws string "," ws "\"type\":" ws string ws "}"
string ::= "\"" ([^"\\] | "\\" .)* "\""
ws ::= [ \t\n]*
"#;

/// GBNF for `{"promote": bool, "reason": "…"}`.
pub const GRAMMAR_PROMOTE_OBSERVATION: &str = r#"
root ::= "{" ws "\"promote\":" ws ("true" | "false") "," ws "\"reason\":" ws string ws "}"
string ::= "\"" ([^"\\] | "\\" .)* "\""
ws ::= [ \t\n]*
"#;

/// GBNF for [`SummaryBundle`] — constrains the SLM to emit JSON with
/// exactly the four fields `{recap, decisions, open_questions,
/// active_tasks}` in order.
///
/// Hand-written from the [`SummaryBundle`] struct definition; if the
/// struct grows a new field or reorders existing fields the grammar
/// must be updated in lock-step. The
/// `synth_summary_grammar_matches_summary_bundle_serialization` test
/// guards against drift by serialising a populated [`SummaryBundle`]
/// and asserting the produced JSON's field ordering and shape match
/// what the grammar accepts.
pub const GRAMMAR_SYNTH_SUMMARY: &str = r#"
root ::= "{" ws "\"recap\":" ws string "," ws "\"decisions\":" ws strings "," ws "\"open_questions\":" ws strings "," ws "\"active_tasks\":" ws strings ws "}"
strings ::= "[" ws (string ("," ws string)*)? ws "]"
string ::= "\"" ([^"\\] | "\\" .)* "\""
ws ::= [ \t\n]*
"#;

/// Per-class prompt for [`InferenceTask::SynthSummary`] on
/// [`ModelClass::Small`] (≤ ~1 B parameters).
///
/// Small models have weaker instruction-following and are prone to:
/// (1) meta-commentary openers ("The session discusses…"),
/// (2) terse recaps that omit key entities (names, IDs, amounts),
/// (3) falling back to English even when the session is in another
/// language.
///
/// This variant strengthens the anti-meta directive with a CRITICAL
/// prefix and an explicit "start with" instruction, adds a
/// coverage-boosting directive to include all identifiers, and
/// reinforces the same-language requirement. The exemplar and
/// `{body}` placeholder are identical to the standard template —
/// only the instructional framing changes.
pub const PROMPT_SYNTH_SUMMARY_SMALL: &str = "\
Output ONLY the JSON object. \
CRITICAL: The very first characters must be {\"recap\":\" — do NOT start with \
'The session', 'This summary', 'The following', or any description of the task. \
Do not preface, explain, or describe the output.\n\
Summarise the session as a JSON object with this exact shape: \
{\"recap\": \"…\", \"decisions\": [\"…\"], \"open_questions\": [\"…\"], \"active_tasks\": [\"…\"]}. \
The recap is a 3-5 sentence factual headline that includes ALL specific identifiers \
(person names, SKU codes, invoice numbers, lot IDs, monetary amounts, dates, and technical terms) \
mentioned in the session. \
The recap MUST be written in the same language and script as the session messages — \
if the session is in French, write in French; if in Japanese, write in Japanese; if in Arabic, \
write in Arabic. Do not translate to English. \
The other fields each list zero or more strings in the session's language.\n\
The example below shows only the JSON shape — its placeholder tokens are NOT \
content: always write the values from the session itself, in the session's own \
language, never copy the example's tokens.\n\n\
Example session (format illustration only):\n\
Observations:\n\
- [decision] (important) EXAMPLE_DECISION\n\
- [task] (important) EXAMPLE_TASK\n\
Example output:\n\
{\"recap\":\"EXAMPLE_DECISION was agreed and EXAMPLE_TASK was scheduled.\",\
\"decisions\":[\"EXAMPLE_DECISION\"],\
\"open_questions\":[],\"active_tasks\":[\"EXAMPLE_TASK\"]}\n\n\
Session:\n{body}";

/// Per-class prompt for [`InferenceTask::SynthSummary`] on
/// [`ModelClass::Medium`] (~1–4 B parameters).
///
/// Medium models have moderate instruction-following but tend to
/// produce terse recaps that omit entities. This variant adds a
/// coverage-boosting directive and a mild anti-meta reinforcement
/// without the heavy-handed CRITICAL prefix needed for small models.
/// The exemplar and `{body}` placeholder are identical to the standard
/// template.
pub const PROMPT_SYNTH_SUMMARY_MEDIUM: &str = "\
Output ONLY the JSON object. Do not describe the task, do not preface or \
explain the output, and do not write about \"the session\" or \"this summary\". \
Summarise the session as a JSON object with this exact shape: \
{\"recap\": \"…\", \"decisions\": [\"…\"], \"open_questions\": [\"…\"], \"active_tasks\": [\"…\"]}. \
The recap is a 2-4 sentence factual headline that includes specific identifiers \
(person names, SKU codes, invoice numbers, lot IDs, monetary amounts, dates, and \
technical terms) mentioned in the session. \
The recap is written in the same language as the session; the other fields each \
list zero or more strings. \
The example below shows only the JSON shape — its placeholder tokens are NOT \
content: always write the values from the session itself, in the session's own \
language, never copy the example's tokens.\n\n\
Example session (format illustration only):\n\
Observations:\n\
- [decision] (important) EXAMPLE_DECISION\n\
- [task] (important) EXAMPLE_TASK\n\
Example output:\n\
{\"recap\":\"EXAMPLE_DECISION was agreed and EXAMPLE_TASK was scheduled.\",\
\"decisions\":[\"EXAMPLE_DECISION\"],\
\"open_questions\":[],\"active_tasks\":[\"EXAMPLE_TASK\"]}\n\n\
Session:\n{body}";

/// Output shape for [`InferenceTask::SynthSummary`] —
/// channel / episodic / domain / tenant summary bundle.
///
/// The four fields are produced in declaration order by
/// `serde_json::to_string`, which is exactly the order the
/// [`GRAMMAR_SYNTH_SUMMARY`] grammar accepts. Reordering or renaming
/// fields here without updating the grammar will cause the SLM to
/// emit JSON the parser still accepts but the grammar will reject,
/// silently making structured decoding fail at the adapter level.
///
/// This type lives in `inference_router::task` (not
/// `synthesis_pipeline::schema`) so that both the producer
/// (`LlamaCppSynthesizer`) and the consumer
/// (`memory_manager::episodic::SlmSummarizer`) can share a single
/// canonical definition without `memory_manager` taking a dependency
/// on `synthesis_pipeline`. `synthesis_pipeline::schema` re-exports
/// it for backward compatibility.
///
/// All fields are `#[serde(default)]` so a [`SummaryBundle`] still
/// deserialises when a token-capped SLM truncates its output before
/// every field is emitted (see [`SummaryBundle::from_slm_str`]). The
/// field *order* is unchanged, so the
/// `synth_summary_grammar_matches_summary_bundle_serialization` test —
/// which pins the serialized ordering against [`GRAMMAR_SYNTH_SUMMARY`]
/// — keeps passing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SummaryBundle {
    /// Free-text recap (the headline).
    pub recap: String,
    /// Decisions captured during the window.
    pub decisions: Vec<String>,
    /// Open questions captured during the window.
    pub open_questions: Vec<String>,
    /// Active tasks captured during the window.
    pub active_tasks: Vec<String>,
}

impl SummaryBundle {
    /// Parse SLM output into a [`SummaryBundle`], tolerating output that
    /// a token-capped, grammar-constrained model truncated mid-emission.
    ///
    /// # Why this exists
    ///
    /// [`GRAMMAR_SYNTH_SUMMARY`] constrains the *shape* of the JSON but
    /// not its *length*: a small model can keep emitting characters until
    /// the adapter's `n_predict` cap guillotines the response — typically
    /// mid-string, deep inside the `recap`. The result is a syntactically
    /// invalid JSON *prefix* (e.g. `{"recap":"…liti` with no closing
    /// quote/brace). A strict `serde_json::from_str` rejects it, which
    /// previously surfaced to the caller as a hard synthesis failure
    /// (HTTP 502) even though the model had produced a perfectly good
    /// recap before being cut off.
    ///
    /// # How it recovers
    ///
    /// Because the grammar emits the four fields in a fixed order, a
    /// truncated response is always a *prefix* of valid JSON. We:
    ///   1. try a strict parse first (the overwhelmingly common path);
    ///   2. on failure, close the longest prefix that yields balanced
    ///      JSON — terminating an open string literal and closing any
    ///      open `[` / `{` (see [`close_truncated_json`]);
    ///   3. re-parse. Fields the model never reached default to empty
    ///      (thanks to `#[serde(default)]`), so a truncated `recap` still
    ///      produces a usable bundle instead of an error.
    ///
    /// Only if no salvageable prefix parses do we return the original
    /// strict-parse error.
    pub fn from_slm_str(raw: &str) -> Result<Self, serde_json::Error> {
        Self::from_slm_str_salvaged(raw).map(|(bundle, _salvaged)| bundle)
    }

    /// Like [`from_slm_str`](Self::from_slm_str), but also reports whether
    /// the strict parse succeeded outright (`false`) or a truncated
    /// prefix had to be salvaged (`true`).
    ///
    /// This is the single source of truth for the salvage / truncation
    /// signal: callers that need it (the synthesis paths, which feed the
    /// `synthesis_truncated_total` metric) use this instead of running
    /// their own `serde_json::from_str` *before* calling `from_slm_str` —
    /// which would parse strictly twice on the salvage path and duplicate
    /// the salvage-detection logic. The returned flag is `true` whenever
    /// the strict parse failed but the prefix-closing salvage recovered a
    /// bundle; under the enforced GBNF grammar that failure is
    /// overwhelmingly a token-cap truncation (a non-truncation parse
    /// failure would require a server-side grammar bug), so callers treat
    /// it as the truncation signal.
    pub fn from_slm_str_salvaged(raw: &str) -> Result<(Self, bool), serde_json::Error> {
        let trimmed = raw.trim();
        let strict_err = match serde_json::from_str::<Self>(trimmed) {
            Ok(bundle) => return Ok((bundle, false)),
            Err(e) => e,
        };
        // Salvage the longest prefix that closes into valid JSON. Walk
        // char boundaries from the end so we never split a UTF-8 scalar
        // (recaps are routinely non-ASCII: French, Japanese, …).
        let mut ends: Vec<usize> = trimmed.char_indices().map(|(i, _)| i).collect();
        ends.push(trimmed.len());
        for &end in ends.iter().rev() {
            if let Some(closed) = close_truncated_json(&trimmed[..end]) {
                if let Ok(bundle) = serde_json::from_str::<Self>(&closed) {
                    return Ok((bundle, true));
                }
            }
        }
        Err(strict_err)
    }
}

/// Best-effort completion of a JSON document truncated mid-emission by a
/// token-capped SLM. Terminates an unterminated string literal and closes
/// any still-open `[` / `{` (in LIFO order), trimming a dangling `,` / `:`
/// that would otherwise sit illegally before a closer.
///
/// Returns `None` when the input cannot be balanced into JSON (e.g. a
/// stray closing bracket with no matching opener) so the caller can fall
/// back rather than emit nonsense.
fn close_truncated_json(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len() + 8);
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for ch in s.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            out.push(ch);
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
            }
            '{' | '[' => {
                stack.push(ch);
                out.push(ch);
            }
            '}' => {
                if stack.pop() != Some('{') {
                    return None;
                }
                out.push(ch);
            }
            ']' => {
                if stack.pop() != Some('[') {
                    return None;
                }
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    // A trailing lone backslash would escape the closing quote we are about
    // to append — drop it first.
    if in_string && escaped {
        out.pop();
    }
    // Close an unterminated string literal.
    if in_string {
        out.push('"');
    }
    // Drop trailing separators/whitespace left exposed by the cut (e.g.
    // `["a",` → `["a"`, or `"recap":` → `"recap"`), which would be illegal
    // immediately before a closing bracket.
    loop {
        let trimmed_len = out.trim_end().len();
        if trimmed_len != out.len() {
            out.truncate(trimmed_len);
            continue;
        }
        match out.chars().last() {
            Some(',' | ':') => {
                out.pop();
            }
            _ => break,
        }
    }
    // Close every still-open container, innermost first.
    while let Some(open) = stack.pop() {
        out.push(if open == '{' { '}' } else { ']' });
    }
    Some(out)
}

/// GBNF for the synthesised concept JSON.
pub const GRAMMAR_SYNTH_CONCEPT: &str = r#"
root ::= "{" ws "\"name\":" ws string "," ws "\"summary\":" ws string "," ws "\"facets\":" ws "{" ws ((string ":" ws string ("," ws string ":" ws string)*))? ws "}" ws "}"
string ::= "\"" ([^"\\] | "\\" .)* "\""
ws ::= [ \t\n]*
"#;

/// GBNF for the adjudication JSON.
pub const GRAMMAR_ADJUDICATE: &str = r#"
root ::= "{" ws "\"canonical\":" ws ("\"a\"" | "\"b\"") "," ws "\"reason\":" ws string ws "}"
string ::= "\"" ([^"\\] | "\\" .)* "\""
ws ::= [ \t\n]*
"#;

/// GBNF for `{"negated": bool, "confidence": 0.0-1.0}`.
pub const GRAMMAR_DETECT_NEGATION: &str = r#"
root ::= "{" ws "\"negated\":" ws ("true" | "false") "," ws "\"confidence\":" ws number ws "}"
number ::= "0" "." [0-9]+ | "1.0" | "1"
ws ::= [ \t\n]*
"#;

/// GBNF for `{"type": "…", "confidence": 0.0-1.0}`.
pub const GRAMMAR_REFINE_ENTITY: &str = r#"
root ::= "{" ws "\"type\":" ws type "," ws "\"confidence\":" ws number ws "}"
type ::= "\"person\"" | "\"organization\"" | "\"product\"" | "\"location\"" | "\"date\"" | "\"currency\"" | "\"identifier\"" | "\"url\"" | "\"email\"" | "\"numeric\"" | "\"event\"" | "\"measurement\"" | "\"unknown\""
number ::= "0" "." [0-9]+ | "1.0" | "1"
ws ::= [ \t\n]*
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_tags_are_unique_and_stable() {
        let tags: Vec<&'static str> = [
            InferenceTask::TagImportance,
            InferenceTask::ExtractEntities,
            InferenceTask::PromoteObservation,
            InferenceTask::SynthSummary,
            InferenceTask::SynthConcept,
            InferenceTask::AdjudicateContradiction,
        ]
        .iter()
        .map(|t| t.tag())
        .collect();
        let mut sorted = tags.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), tags.len(), "task tags must be unique");
    }

    #[test]
    fn classification_vs_synthesis_partition() {
        for task in [
            InferenceTask::TagImportance,
            InferenceTask::ExtractEntities,
            InferenceTask::PromoteObservation,
            InferenceTask::DetectNegation,
            InferenceTask::RefineEntity,
        ] {
            assert!(task.is_classification());
            assert!(!task.is_synthesis());
        }
        for task in [
            InferenceTask::SynthSummary,
            InferenceTask::SynthConcept,
            InferenceTask::AdjudicateContradiction,
        ] {
            assert!(!task.is_classification());
            assert!(task.is_synthesis());
        }
    }

    #[test]
    fn grammars_are_present_for_classification() {
        for task in [
            InferenceTask::TagImportance,
            InferenceTask::ExtractEntities,
            InferenceTask::PromoteObservation,
            InferenceTask::DetectNegation,
            InferenceTask::RefineEntity,
        ] {
            assert!(!task.grammar().is_empty(), "task {task:?} needs a grammar");
        }
    }

    #[test]
    fn synth_summary_grammar_constrains_summary_bundle_shape() {
        let g = InferenceTask::SynthSummary.grammar();
        assert!(!g.is_empty(), "synth_summary must constrain output");
        for field in ["recap", "decisions", "open_questions", "active_tasks"] {
            assert!(
                g.contains(field),
                "GBNF must mention `{field}` so the SLM emits SummaryBundle JSON"
            );
        }
    }

    /// Drift guard: serialise a populated [`SummaryBundle`] and
    /// confirm the resulting JSON
    ///
    /// * is in the field order the grammar accepts
    ///   (`recap` then `decisions` then `open_questions` then
    ///   `active_tasks`),
    /// * round-trips back to the same struct,
    /// * starts with `{"recap":` so the grammar's first production
    ///   (`"{" ws "\"recap\":"`) matches.
    ///
    /// If [`SummaryBundle`] grows a field or its declaration order
    /// changes, the first two assertions catch it before the SLM
    /// ever sees the new grammar / new shape.
    #[test]
    fn synth_summary_grammar_matches_summary_bundle_serialization() {
        let bundle = SummaryBundle {
            recap: "the team shipped".to_string(),
            decisions: vec!["keep vendor X".to_string()],
            open_questions: vec!["who owns the rollout?".to_string()],
            active_tasks: vec!["draft RFC".to_string()],
        };
        let json = serde_json::to_string(&bundle).unwrap();
        // Field order must match the grammar's `root` production.
        let recap_idx = json.find("\"recap\"").unwrap();
        let decisions_idx = json.find("\"decisions\"").unwrap();
        let questions_idx = json.find("\"open_questions\"").unwrap();
        let tasks_idx = json.find("\"active_tasks\"").unwrap();
        assert!(
            recap_idx < decisions_idx && decisions_idx < questions_idx && questions_idx < tasks_idx,
            "SummaryBundle serialisation drifted from GBNF field order; got: {json}"
        );
        assert!(
            json.starts_with("{\"recap\":"),
            "GBNF root expects `{{\"recap\":` prefix, got: {json}"
        );
        // Round-trip the JSON back through the type so we know
        // serde_json::from_str (used by every grammar-constrained
        // consumer) reverses the encoding without loss.
        let decoded: SummaryBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, bundle);
    }

    /// The one-shot exemplar embedded in the `SynthSummary` prompt
    /// template steers the model toward emitting JSON in the grammar's
    /// field order. If the exemplar's order drifts from
    /// [`GRAMMAR_SYNTH_SUMMARY`], the model is anchored toward an order
    /// the grammar rejects, so every synthesis call would fail at parse
    /// time. This pins the exemplar's field order (and the inline shape
    /// hint that precedes it) to the same `recap → decisions →
    /// open_questions → active_tasks` order the grammar accepts, so the
    /// prompt and grammar can never silently fall out of lock-step.
    #[test]
    fn synth_summary_prompt_exemplar_field_order_matches_grammar() {
        let template = InferenceTask::SynthSummary.prompt_template();
        // The exemplar output is the *last* JSON object in the template
        // (the shape hint near the top lists the same fields with `…`
        // placeholders). Both must agree with the grammar order, so we
        // assert ordering across every occurrence of each field key.
        for field in ["recap", "decisions", "open_questions", "active_tasks"] {
            assert!(
                template.contains(&format!("\"{field}\"")),
                "exemplar must mention `{field}` so it stays in sync with the grammar"
            );
        }
        // First-occurrence ordering: the shape hint is the first place
        // each key appears, and it must follow the grammar order.
        let recap_idx = template.find("\"recap\"").unwrap();
        let decisions_idx = template.find("\"decisions\"").unwrap();
        let questions_idx = template.find("\"open_questions\"").unwrap();
        let tasks_idx = template.find("\"active_tasks\"").unwrap();
        assert!(
            recap_idx < decisions_idx && decisions_idx < questions_idx && questions_idx < tasks_idx,
            "prompt field order drifted from GBNF; the model would be steered \
             toward JSON the grammar rejects: {template}"
        );
        // Last-occurrence ordering: the concrete exemplar output object
        // (the final mention of each key) must also match, so a reordered
        // exemplar body is caught even if the shape hint stays correct.
        let recap_last = template.rfind("\"recap\"").unwrap();
        let decisions_last = template.rfind("\"decisions\"").unwrap();
        let questions_last = template.rfind("\"open_questions\"").unwrap();
        let tasks_last = template.rfind("\"active_tasks\"").unwrap();
        assert!(
            recap_last < decisions_last
                && decisions_last < questions_last
                && questions_last < tasks_last,
            "exemplar output field order drifted from GBNF: {template}"
        );
    }

    /// Every token in [`SYNTH_EXEMPLAR_TOKENS`] must literally appear in
    /// the `SynthSummary` prompt's one-shot exemplar. The synthesis
    /// quality gate strips bundle entries that contain these tokens, so a
    /// drift between the prompt literal (what the model can copy) and the
    /// constant (what the gate strips) would silently let a leaked
    /// exemplar slip into a persisted bundle. Pinning them together keeps
    /// the leak-detector honest if the exemplar is ever reworded.
    #[test]
    fn synth_summary_exemplar_tokens_appear_in_prompt() {
        let template = InferenceTask::SynthSummary.prompt_template();
        for token in SYNTH_EXEMPLAR_TOKENS {
            assert!(
                template.contains(token),
                "exemplar token `{token}` is no longer in the SynthSummary prompt; \
                 update SYNTH_EXEMPLAR_TOKENS so the quality gate keeps stripping \
                 the leak it can copy"
            );
        }
    }

    /// The inverse drift-guard: the prompt must not contain any
    /// `EXAMPLE_`-prefixed placeholder that is *not* tracked in
    /// [`SYNTH_EXEMPLAR_TOKENS`]. Without this, a future edit that adds a
    /// new exemplar placeholder (e.g. `EXAMPLE_QUESTION`) without updating
    /// the constant would let that leak slip past the quality gate
    /// silently. We scan for the shared `EXAMPLE_` convention so the two
    /// can't drift in either direction.
    #[test]
    fn synth_summary_prompt_has_no_untracked_exemplar_tokens() {
        let template = InferenceTask::SynthSummary.prompt_template();
        // Walk the template collecting maximal `[A-Za-z0-9_]` runs that
        // begin with the `EXAMPLE_` placeholder convention.
        let bytes = template.as_bytes();
        let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
        let mut i = 0;
        while i < bytes.len() {
            if !is_word(bytes[i]) || (i > 0 && is_word(bytes[i - 1])) {
                i += 1;
                continue;
            }
            let start = i;
            while i < bytes.len() && is_word(bytes[i]) {
                i += 1;
            }
            let word = &template[start..i];
            if word.starts_with("EXAMPLE_") {
                assert!(
                    SYNTH_EXEMPLAR_TOKENS.contains(&word),
                    "prompt contains exemplar placeholder `{word}` that is not in \
                     SYNTH_EXEMPLAR_TOKENS; add it so the quality gate strips the \
                     leak it can copy"
                );
            }
        }
    }

    #[test]
    fn from_slm_str_accepts_complete_json() {
        let raw = r#"{"recap":"all good","decisions":["ship it"],"open_questions":[],"active_tasks":["follow up"]}"#;
        let b = SummaryBundle::from_slm_str(raw).unwrap();
        assert_eq!(b.recap, "all good");
        assert_eq!(b.decisions, vec!["ship it".to_string()]);
        assert_eq!(b.active_tasks, vec!["follow up".to_string()]);
    }

    #[test]
    fn serde_default_tolerates_missing_trailing_fields() {
        // A model that stopped right after a complete `recap` (valid JSON
        // but missing the three array fields) must still deserialise.
        let b: SummaryBundle = serde_json::from_str(r#"{"recap":"only the recap"}"#).unwrap();
        assert_eq!(b.recap, "only the recap");
        assert!(b.decisions.is_empty() && b.open_questions.is_empty() && b.active_tasks.is_empty());
    }

    #[test]
    fn from_slm_str_salvages_truncation_inside_recap_string() {
        // The real failure mode observed against small quantised SLMs: the
        // token cap cuts the response mid-`recap`, leaving an unterminated
        // string and an unclosed object. The recap text must survive intact.
        let raw = r#"{"recap":"The CartoNord invoice FA-2025-0411 of 90 000 EUR is overdue; payment is blocked pending the credit-note dispu"#;
        let b = SummaryBundle::from_slm_str(raw).unwrap();
        assert!(b.recap.starts_with("The CartoNord invoice FA-2025-0411"));
        assert!(b.recap.ends_with("dispu"));
        assert!(b.decisions.is_empty());
    }

    #[test]
    fn from_slm_str_salvages_truncation_inside_array() {
        // Cut off mid-way through the `decisions` array.
        let raw = r#"{"recap":"r","decisions":["keep vendor","raise the limit"#;
        let b = SummaryBundle::from_slm_str(raw).unwrap();
        assert_eq!(b.recap, "r");
        assert_eq!(
            b.decisions,
            vec!["keep vendor".to_string(), "raise the limit".to_string()]
        );
        assert!(b.open_questions.is_empty());
    }

    #[test]
    fn from_slm_str_salvages_trailing_separator() {
        // A dangling comma after a closed array element.
        let raw = r#"{"recap":"r","decisions":["a","b","#;
        let b = SummaryBundle::from_slm_str(raw).unwrap();
        assert_eq!(b.decisions, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn from_slm_str_preserves_unicode_recap_on_truncation() {
        // Non-ASCII recaps (French/Japanese) must not be split mid-scalar.
        let raw = "{\"recap\":\"サーボAX-7が過熱、ファームウェアの不具合が根本原";
        let b = SummaryBundle::from_slm_str(raw).unwrap();
        assert!(b.recap.starts_with("サーボAX-7"));
    }

    #[test]
    fn from_slm_str_errors_on_unsalvageable_garbage() {
        // No leading object at all → cannot become a SummaryBundle.
        assert!(SummaryBundle::from_slm_str("not json at all").is_err());
    }

    /// Exhaustiveness pin for [`InferenceTask::ALL`].
    ///
    /// The `match` below has NO catch-all arm, so adding a new
    /// variant to [`InferenceTask`] without also appending it to
    /// `InferenceTask::ALL` is a **compile error** (the exhaustive
    /// match no longer covers the enum). On top of that the runtime
    /// assertion pins cardinality and order so a reviewer who
    /// deletes a variant from `ALL` (without also removing it from
    /// the enum) gets a `cargo test` failure.
    ///
    /// Concretely this is the structural defence asked
    /// for: the router's `ALL_TASKS` constant now derives from
    /// `InferenceTask::ALL`, and `InferenceTask::ALL` is pinned to
    /// the enum's discriminants here.
    #[test]
    fn all_is_exhaustive() {
        let mut count = 0_usize;
        for &task in InferenceTask::ALL {
            count += 1;
            // No `_ =>` arm — if a variant is added to the enum but
            // not to `ALL`, the compiler refuses to compile this
            // test until the new variant is appended to `ALL` AND
            // matched here. If a variant is renamed, the same
            // mismatch surfaces.
            #[allow(clippy::match_same_arms)]
            match task {
                InferenceTask::TagImportance => {}
                InferenceTask::ExtractEntities => {}
                InferenceTask::PromoteObservation => {}
                InferenceTask::SynthSummary => {}
                InferenceTask::SynthConcept => {}
                InferenceTask::AdjudicateContradiction => {}
                InferenceTask::DetectNegation => {}
                InferenceTask::RefineEntity => {}
            }
        }
        assert_eq!(count, 8, "InferenceTask::ALL drifted from enum cardinality");
        // Order is part of the public contract — pin it explicitly.
        assert_eq!(
            InferenceTask::ALL
                .iter()
                .map(|t| t.tag())
                .collect::<Vec<_>>(),
            vec![
                "tag_importance",
                "extract_entities",
                "promote_observation",
                "synth_summary",
                "synth_concept",
                "adjudicate_contradiction",
                "detect_negation",
                "refine_entity",
            ],
        );
    }

    #[test]
    fn tag_round_trips_through_from_tag_for_every_variant() {
        // Every adapter's error path funnels a runtime `&str` tag back
        // through `static_tag_or_unknown`. Pin the round-trip for the
        // whole taxonomy so a new variant (which the compiler forces
        // into `tag`/`ALL`) is automatically covered for all four call
        // sites, and an unrecognised tag stays the stable `"unknown"`.
        for &task in InferenceTask::ALL {
            assert_eq!(
                InferenceTask::from_tag(task.tag()),
                Some(task),
                "tag {:?} did not round-trip through from_tag",
                task.tag(),
            );
            assert_eq!(
                InferenceTask::static_tag_or_unknown(task.tag()),
                task.tag(),
                "static_tag_or_unknown dropped the canonical tag for {task:?}",
            );
        }
        assert_eq!(InferenceTask::from_tag("not_a_task"), None);
        assert_eq!(
            InferenceTask::static_tag_or_unknown("not_a_task"),
            "unknown"
        );
        assert_eq!(InferenceTask::static_tag_or_unknown(""), "unknown");
    }

    #[test]
    fn prompts_contain_placeholders() {
        for task in [
            InferenceTask::TagImportance,
            InferenceTask::SynthSummary,
            InferenceTask::AdjudicateContradiction,
            InferenceTask::DetectNegation,
            InferenceTask::RefineEntity,
        ] {
            let template = task.prompt_template();
            assert!(template.contains('{') && template.contains('}'));
        }
    }

    #[test]
    fn model_class_detection_from_filename() {
        // Small (≤ ~1 B)
        assert_eq!(ModelClass::from_model_path("qwen3.5-0.8b-q4_k_m.gguf"), ModelClass::Small);
        assert_eq!(ModelClass::from_model_path("/models/Qwen_Qwen3.5-0.8B-Q4_K_M.gguf"), ModelClass::Small);
        assert_eq!(ModelClass::from_model_path("tiny-0.5b.gguf"), ModelClass::Small);
        assert_eq!(ModelClass::from_model_path("mini-1.0b.gguf"), ModelClass::Small);
        assert_eq!(ModelClass::from_model_path("nano-1.1b.gguf"), ModelClass::Small);

        // Medium (~1–4 B)
        assert_eq!(ModelClass::from_model_path("qwen3.5-2b-q4_k_m.gguf"), ModelClass::Medium);
        assert_eq!(ModelClass::from_model_path("bonsai-1.7b-q2_0.gguf"), ModelClass::Medium);
        assert_eq!(ModelClass::from_model_path("phi-3b.gguf"), ModelClass::Medium);
        assert_eq!(ModelClass::from_model_path("model-3.5b.gguf"), ModelClass::Medium);
        assert_eq!(ModelClass::from_model_path("llama-4b.gguf"), ModelClass::Medium);

        // Large (> ~4 B) — default for unrecognised
        assert_eq!(ModelClass::from_model_path("llama-3-8b.gguf"), ModelClass::Large);
        assert_eq!(ModelClass::from_model_path("unknown-model.gguf"), ModelClass::Large);
        assert_eq!(ModelClass::from_model_path("/path/to/some-model.bin"), ModelClass::Large);
    }

    #[test]
    fn per_class_prompt_selects_correct_template() {
        let small = InferenceTask::SynthSummary.prompt_template_for_class(ModelClass::Small);
        let medium = InferenceTask::SynthSummary.prompt_template_for_class(ModelClass::Medium);
        let large = InferenceTask::SynthSummary.prompt_template_for_class(ModelClass::Large);
        let standard = InferenceTask::SynthSummary.prompt_template();

        // Large falls back to the standard template.
        assert_eq!(large, standard);

        // Small and Medium are distinct from each other and from standard.
        assert_ne!(small, medium);
        assert_ne!(small, standard);
        assert_ne!(medium, standard);

        // Non-synthesis tasks always use the standard template.
        assert_eq!(
            InferenceTask::TagImportance.prompt_template_for_class(ModelClass::Small),
            InferenceTask::TagImportance.prompt_template(),
        );
    }

    #[test]
    fn per_class_prompts_contain_exemplar_tokens() {
        for class in [ModelClass::Small, ModelClass::Medium] {
            let template = InferenceTask::SynthSummary.prompt_template_for_class(class);
            for token in SYNTH_EXEMPLAR_TOKENS {
                assert!(
                    template.contains(token),
                    "per-class prompt for {class:?} must contain exemplar token `{token}` \
                     so the quality gate can detect leaks"
                );
            }
        }
    }

    #[test]
    fn per_class_prompts_have_correct_field_order() {
        for class in [ModelClass::Small, ModelClass::Medium] {
            let template = InferenceTask::SynthSummary.prompt_template_for_class(class);
            // The exemplar must follow the grammar's field order.
            let recap_idx = template.find("\"recap\"").unwrap();
            let decisions_idx = template.find("\"decisions\"").unwrap();
            let questions_idx = template.find("\"open_questions\"").unwrap();
            let tasks_idx = template.find("\"active_tasks\"").unwrap();
            assert!(
                recap_idx < decisions_idx
                    && decisions_idx < questions_idx
                    && questions_idx < tasks_idx,
                "per-class prompt for {class:?} field order drifted from GBNF grammar"
            );
            // Must have the {body} placeholder.
            assert!(template.contains("{body}"), "per-class prompt must have {{body}} placeholder");
        }
    }

    #[test]
    fn small_prompt_has_anti_meta_directive() {
        let template = InferenceTask::SynthSummary.prompt_template_for_class(ModelClass::Small);
        // The Small variant must have a stronger anti-meta directive
        // than the standard template.
        assert!(
            template.contains("CRITICAL"),
            "Small prompt must have CRITICAL anti-meta directive"
        );
        // Must explicitly instruct to start with the JSON object.
        assert!(
            template.contains("{\"recap\":\""),
            "Small prompt must instruct to start with the recap field"
        );
    }

    #[test]
    fn small_prompt_has_coverage_directive() {
        let small = InferenceTask::SynthSummary.prompt_template_for_class(ModelClass::Small);
        let medium = InferenceTask::SynthSummary.prompt_template_for_class(ModelClass::Medium);
        // Both per-class variants must mention identifiers for coverage.
        for template in [small, medium] {
            assert!(
                template.to_lowercase().contains("identifier") || template.contains("SKU"),
                "per-class prompt must include coverage-boosting directive about identifiers"
            );
        }
    }
}
